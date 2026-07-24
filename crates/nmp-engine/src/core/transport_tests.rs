//! Ownership-domain tests moved with the implementation they falsify.

use super::*;

#[cfg(test)]
mod relay_session_key_tests {
    use super::*;
    use nmp_router::FixtureDirectory;
    use nmp_store::{coverage_key, MemoryStore};
    use nostr::{Keys, SubscriptionId};

    fn relay() -> RelayUrl {
        RelayUrl::parse("wss://session.example.com").unwrap()
    }

    #[test]
    fn wrong_context_eose_cannot_consume_or_credit_another_session() {
        let relay = relay();
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let access_a = AccessContext::Nip42(a);
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let atom = ContextualAtom {
            filter: filter.clone(),
            source: SourceAuthority::Public,
            access: access_a,
            routing_evidence: BTreeSet::new(),
        };
        let key = coverage_key(&atom);
        let sub_id = SubId::for_wire(relay.clone(), &filter, &SourceAuthority::Public, access_a);
        let session_a = RelaySessionKey::new(relay.clone(), access_a);
        let session_b = RelaySessionKey::new(relay, AccessContext::Nip42(b));
        let mut attribution = AttributionState::new();
        attribution.observe_demand([&atom]);
        attribution.record_send(&session_a, &sub_id, &filter, BTreeSet::from([key]));
        let wire_id = wire_sub_id_string(&sub_id);

        assert!(attribution
            .attribute_eose(&session_b, &wire_id, Timestamp::from(10u64))
            .is_empty());
        assert_eq!(
            attribution
                .attribute_eose(&session_a, &wire_id, Timestamp::from(10u64))
                .len(),
            1
        );
    }

    #[test]
    fn correlated_completion_uses_exact_send_shape_and_completion_cap() {
        let relay = relay();
        let session = RelaySessionKey::public(relay.clone());
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            until: Some(150),
            ..ConcreteFilter::default()
        };
        let atom = ContextualAtom {
            filter: filter.clone(),
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let key = coverage_key(&atom);
        let sub_id = SubId::for_wire(
            relay,
            &filter,
            &SourceAuthority::Public,
            AccessContext::Public,
        );
        let mut attribution = AttributionState::new();
        let completed_send =
            attribution.record_send(&session, &sub_id, &filter, BTreeSet::from([key]));
        attribution.record_send(
            &session,
            &sub_id,
            &ConcreteFilter {
                since: Some(100),
                ..filter.clone()
            },
            BTreeSet::from([key]),
        );

        assert_eq!(
            attribution.attribute_correlated_completion(
                &session,
                &wire_sub_id_string(&sub_id),
                completed_send,
                Timestamp::from(200u64),
            ),
            vec![(
                key,
                nmp_store::CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(150u64),),
            )]
        );
        assert_eq!(
            attribution.attribute_eose(
                &session,
                &wire_sub_id_string(&sub_id),
                Timestamp::from(200u64),
            ),
            vec![(
                key,
                nmp_store::CoverageInterval::new(Timestamp::from(100u64), Timestamp::from(150u64),),
            )]
        );
    }

    #[test]
    fn disconnecting_a_preserves_public_and_b_sessions() {
        let relay = relay();
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let public = RelaySessionKey::public(relay.clone());
        let session_a = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(a));
        let session_b = RelaySessionKey::new(relay, AccessContext::Nip42(b));
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        let handles = [
            TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            TransportRelayHandle {
                slot: 1,
                generation: 1,
            },
            TransportRelayHandle {
                slot: 2,
                generation: 1,
            },
        ];
        core.handle(EngineMsg::RelayConnected(handles[0], public.clone()));
        core.handle(EngineMsg::RelayConnected(handles[1], session_a.clone()));
        core.handle(EngineMsg::RelayConnected(handles[2], session_b.clone()));

        core.handle(EngineMsg::RelayDisconnected(
            handles[1],
            session_a.clone(),
            DisconnectReason::Closed,
        ));

        assert!(core.connected_relays.contains(&public));
        assert!(!core.connected_relays.contains(&session_a));
        assert!(core.connected_relays.contains(&session_b));
    }

    #[test]
    fn protected_neg_frames_cannot_resolve_the_public_probe_or_inherit_its_diagnostics() {
        let relay = relay();
        let public = RelaySessionKey::public(relay.clone());
        let protected = RelaySessionKey::new(
            relay.clone(),
            AccessContext::Nip42(Keys::generate().public_key()),
        );
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let atoms = BTreeSet::from([
            ContextualAtom {
                filter: filter.clone(),
                source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
                access: AccessContext::Public,
                routing_evidence: BTreeSet::new(),
            },
            ContextualAtom {
                filter,
                source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
                access: protected.access,
                routing_evidence: BTreeSet::new(),
            },
        ]);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.router
            .compile(&atoms, core.directory.as_ref(), core.cap);
        let public_handle = TransportRelayHandle {
            slot: 5,
            generation: 1,
        };
        let protected_handle = TransportRelayHandle {
            slot: 6,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(public_handle, public.clone()));
        core.handle(EngineMsg::RelayConnected(
            protected_handle,
            protected.clone(),
        ));
        let probe = core.prober.begin_probe(&relay).unwrap();
        let wire_id = wire_sub_id_string(&probe.sub_id);

        let protected_neg_msg = RelayFrame::from(RelayMessage::NegMsg {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(wire_id.clone())),
            message: std::borrow::Cow::Owned("6100".to_string()),
        });
        assert!(core
            .handle(EngineMsg::RelayFrame(
                protected_handle,
                protected.clone(),
                protected_neg_msg,
            ))
            .is_empty());
        let protected_neg_err = RelayFrame::from(RelayMessage::NegErr {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(wire_id.clone())),
            message: std::borrow::Cow::Owned("blocked: unsupported".to_string()),
        });
        assert!(core
            .handle(EngineMsg::RelayFrame(
                protected_handle,
                protected.clone(),
                protected_neg_err,
            ))
            .is_empty());
        assert_eq!(
            core.prober.state(&relay),
            crate::negentropy::ProbeState::Probing
        );

        let probing = core.diagnostics_snapshot();
        let public_diagnostics = probing
            .relays
            .iter()
            .find(|entry| entry.access == AccessContext::Public)
            .unwrap();
        let protected_diagnostics = probing
            .relays
            .iter()
            .find(|entry| entry.access == protected.access)
            .unwrap();
        assert_eq!(public_diagnostics.nip77_behavior, "probing");
        assert_eq!(protected_diagnostics.nip77_behavior, "unknown");

        let public_neg_msg = RelayFrame::from(RelayMessage::NegMsg {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(wire_id)),
            message: std::borrow::Cow::Owned("6100".to_string()),
        });
        core.handle(EngineMsg::RelayFrame(public_handle, public, public_neg_msg));
        assert_eq!(
            core.prober.state(&relay),
            crate::negentropy::ProbeState::Supported
        );
        let resolved = core.diagnostics_snapshot();
        assert_eq!(
            resolved
                .relays
                .iter()
                .find(|entry| entry.access == AccessContext::Public)
                .unwrap()
                .nip77_behavior,
            "behaviorally_proven"
        );
        assert_eq!(
            resolved
                .relays
                .iter()
                .find(|entry| entry.access == protected.access)
                .unwrap()
                .nip77_behavior,
            "unknown"
        );
    }

    #[test]
    fn intentional_close_never_reopens_a_still_planned_session() {
        let relay = relay();
        let session = RelaySessionKey::public(relay.clone());
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1])),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::Pinned(BTreeSet::from([relay])),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.router
            .compile(&BTreeSet::from([atom]), core.directory.as_ref(), core.cap);
        let handle = TransportRelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));

        let effects = core.handle(EngineMsg::RelayDisconnected(
            handle,
            session,
            DisconnectReason::Closed,
        ));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::EnsureReadRelay(..) | Effect::EnsureWriteRelay(..)
        )));
    }
}

#[cfg(test)]
mod durable_retry_policy_tests {
    use super::*;

    fn key() -> LaneKey {
        LaneKey {
            intent_id: IntentId(42),
            relay: RelayUrl::parse("wss://retry-policy.example").unwrap(),
        }
    }

    #[test]
    fn standardized_ok_prefixes_and_unknown_default_are_exact() {
        assert_eq!(classify_relay_ack(true, "anything"), RelayAckClass::Acked);
        assert_eq!(
            classify_relay_ack(false, "duplicate: already have this event"),
            RelayAckClass::Acked
        );
        assert_eq!(
            classify_relay_ack(false, "rate-limited: slow down"),
            RelayAckClass::Transient(TransientCause::RelayRateLimited)
        );
        assert_eq!(
            classify_relay_ack(false, "error: temporary relay failure"),
            RelayAckClass::Transient(TransientCause::RelayError)
        );
        assert_eq!(
            classify_relay_ack(false, "auth-required: authenticate"),
            RelayAckClass::WaitingAuth
        );
        for prefix in ["invalid", "pow", "blocked", "restricted", "mute"] {
            assert_eq!(
                classify_relay_ack(false, &format!("{prefix}: reason")),
                RelayAckClass::Rejected
            );
        }
        for raw in [
            "unknown: reason",
            "malformed without delimiter",
            "duplicate but only in free-form text",
            "Duplicate: prefix matching is case-sensitive",
            " rate-limited: leading whitespace is not a prefix",
        ] {
            assert_eq!(
                classify_relay_ack(false, raw),
                RelayAckClass::Rejected,
                "free-form relay text must never be heuristically classified: {raw}"
            );
        }
    }

    #[test]
    fn retry_backoff_is_bounded_and_deterministic_from_persisted_identity() {
        let key = key();
        let first = retry_delay_secs(&key, 1);
        assert!((3..8).contains(&first));
        assert_eq!(first, retry_delay_secs(&key, 1));
        for ordinal in 1..=16 {
            let delay = retry_delay_secs(&key, ordinal);
            let exponent = ordinal.saturating_sub(1).min(63) as u32;
            let base = RETRY_INITIAL_SECS
                .checked_shl(exponent)
                .unwrap_or(u64::MAX)
                .min(RETRY_MAX_SECS);
            assert!((base..base + RETRY_JITTER_MAX_SECS).contains(&delay));
        }
        assert!((300..305).contains(&retry_delay_secs(&key, u64::MAX)));
        assert_ne!(
            retry_delay_secs(&key, 1),
            retry_delay_secs(
                &LaneKey {
                    intent_id: IntentId(43),
                    relay: key.relay,
                },
                1
            ),
            "this fixture must prove persisted attempt identity participates in jitter"
        );
    }
}

#[cfg(test)]
mod nip65_read_write_split_tests {
    //! Unit A's NIP-65 read/write parse split (`routing-and-ownership.md`
    //! §2.4) -- private free functions, so tested directly in-module rather
    //! than via the heavier `tests/self_bootstrap_outbox.rs`-style engine
    //! harness (which already covers `parse_nip65_write_relays` end-to-end
    //! via `relay_list_parse_excludes_explicit_read_only_relays`).

    use nmp_router::LiveDirectory;
    use nmp_store::MemoryStore;
    use nmp_transport::RelayFrame;
    use nostr::nips::nip65::RelayMetadata;
    use nostr::{EventBuilder, Keys, Kind, RelayMessage, SubscriptionId, Tag, Tags};

    use super::*;

    fn relay_list_event(author: &Keys, tags: Vec<Tag>) -> nostr::Event {
        EventBuilder::new(Kind::RelayList, "")
            .tags(Tags::from_list(tags))
            .sign_with_keys(author)
            .expect("test fixture event must sign cleanly")
    }

    #[test]
    fn nip65_unmarked_relay_is_both_read_and_write() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://both.example.com").unwrap();
        let event = relay_list_event(&author, vec![Tag::relay_metadata(r.clone(), None)]);

        assert_eq!(
            parse_nip65_write_relays(&event),
            vec![LanedRelay::new(r.clone(), Lane::Nip65Write)],
            "an unmarked r tag must count as a write relay"
        );
        assert_eq!(
            parse_nip65_read_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Read)],
            "an unmarked r tag must ALSO count as a read relay (NIP-65: unmarked = both)"
        );
    }

    #[test]
    fn nip65_write_marked_excluded_from_read() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://write-only.example.com").unwrap();
        let event = relay_list_event(
            &author,
            vec![Tag::relay_metadata(r.clone(), Some(RelayMetadata::Write))],
        );

        assert_eq!(
            parse_nip65_write_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Write)],
            "an explicit write-marked relay must still be a write relay"
        );
        assert!(
            parse_nip65_read_relays(&event).is_empty(),
            "an explicit write-marked relay must be excluded from the read set"
        );
    }

    #[test]
    fn nip65_read_marked_excluded_from_write() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://read-only.example.com").unwrap();
        let event = relay_list_event(
            &author,
            vec![Tag::relay_metadata(r.clone(), Some(RelayMetadata::Read))],
        );

        assert!(
            parse_nip65_write_relays(&event).is_empty(),
            "an explicit read-marked relay must be excluded from the write set"
        );
        assert_eq!(
            parse_nip65_read_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Read)],
            "an explicit read-marked relay must still be a read relay"
        );
    }

    /// `ingest_relay_list_winner` stores BOTH sets from the ONE kind:10002
    /// winner in a single pass (`routing-and-ownership.md` §2.4) -- proven
    /// through the real `EngineCore::on_relay_frame` path (not a bypassed
    /// direct directory poke), against a relay list mixing an unmarked
    /// (both), an explicit write-only, and an explicit read-only relay.
    #[test]
    fn live_directory_stores_read_and_write_from_one_winner() {
        let author = Keys::generate();
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let both = RelayUrl::parse("wss://both.example.com").unwrap();
        let write_only = RelayUrl::parse("wss://write-only.example.com").unwrap();
        let read_only = RelayUrl::parse("wss://read-only.example.com").unwrap();

        let dir = LiveDirectory::builder().build();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 10);

        core.handle(EngineMsg::RelayConnected(
            TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            RelaySessionKey::public(relay_url.clone()),
        ));

        let event = relay_list_event(
            &author,
            vec![
                Tag::relay_metadata(both.clone(), None),
                Tag::relay_metadata(write_only.clone(), Some(RelayMetadata::Write)),
                Tag::relay_metadata(read_only.clone(), Some(RelayMetadata::Read)),
            ],
        );
        core.handle(EngineMsg::RelayFrame(
            TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            RelaySessionKey::public(relay_url),
            RelayFrame::from(RelayMessage::event(SubscriptionId::new("s"), event)),
        ));

        let author_hex = author.public_key().to_hex();
        let write_relays: BTreeSet<RelayUrl> = core
            .directory
            .write_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();
        let read_relays: BTreeSet<RelayUrl> = core
            .directory
            .read_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();

        assert_eq!(
            write_relays,
            BTreeSet::from([both.clone(), write_only.clone()]),
            "write set must be {{unmarked, write-marked}}, excluding read-marked"
        );
        assert_eq!(
            read_relays,
            BTreeSet::from([both, read_only]),
            "read set must be {{unmarked, read-marked}}, excluding write-marked"
        );
    }
}

#[cfg(test)]
mod relay_admission_tests {
    //! Issue #121 falsifiers for the provenance-aware discovered-relay
    //! admission gate. All exercise the REAL `EngineCore::on_relay_frame`
    //! ingest path (a validly-signed kind:10002 delivered over the wire),
    //! never a bypassed direct directory poke -- the whole point is that a
    //! *validly signed but hostile* relay list is what we must reject.
    //!
    //! "Never reaches `ensure_open`" is proven structurally: a rejected relay
    //! is absent from `directory.write_relays`/`read_relays`, so the router
    //! never builds a candidate for it, so no `Effect` ever names it, so
    //! `runtime::dispatch_effect` never calls `pool.ensure_open` on it. Each
    //! test pins that absence at the directory, the choke point where a
    //! discovered relay would otherwise become a routable lane.

    use nmp_router::LiveDirectory;
    use nmp_store::MemoryStore;
    use nmp_transport::RelayFrame;
    use nostr::{EventBuilder, Keys, Kind, RelayMessage, SubscriptionId, Tag, Tags};

    // `RelayDirectory` (the trait whose `write_relays`/`read_relays` these
    // tests call) is already in scope via `use super::*` — importing it again
    // here is a redundant-import warning under `-D warnings`.
    use super::*;

    const SLOT: u32 = 0;
    const GEN: u64 = 1;

    fn relay(url: &str) -> RelayUrl {
        RelayUrl::parse(url).expect("valid test relay url")
    }

    /// Drive a signed kind:10002 (declaring every `url` as an unmarked
    /// read+write relay) through the engine's real ingest path.
    fn ingest_relay_list(core: &mut EngineCore<MemoryStore>, author: &Keys, urls: &[&RelayUrl]) {
        // A connected relay is the one the discovery frame arrives on.
        core.handle(EngineMsg::RelayConnected(
            TransportRelayHandle {
                slot: SLOT,
                generation: GEN,
            },
            RelaySessionKey::public(relay("wss://indexer.example.com")),
        ));
        let tags: Vec<Tag> = urls
            .iter()
            .map(|u| Tag::relay_metadata((*u).clone(), None))
            .collect();
        let event = EventBuilder::new(Kind::RelayList, "")
            .tags(Tags::from_list(tags))
            .sign_with_keys(author)
            .expect("test fixture event must sign cleanly");
        core.handle(EngineMsg::RelayFrame(
            TransportRelayHandle {
                slot: SLOT,
                generation: GEN,
            },
            RelaySessionKey::public(relay("wss://indexer.example.com")),
            RelayFrame::from(RelayMessage::event(SubscriptionId::new("s"), event)),
        ));
    }

    fn admitted_writes(core: &EngineCore<MemoryStore>, author: &Keys) -> BTreeSet<RelayUrl> {
        core.directory
            .write_relays(&author.public_key().to_hex())
            .into_iter()
            .map(|lr| lr.url)
            .collect()
    }

    /// The headline falsifier: a validly-signed, network-DISCOVERED kind:10002
    /// listing a loopback, an RFC-1918, and a `.onion` relay alongside one
    /// public relay must admit ONLY the public relay. The three hostile
    /// relays never become lanes (so never reach `ensure_open`), and the
    /// diagnostic rejection counter records exactly them -- for BOTH the read
    /// and write parse of the one event (2.4's dual parse), i.e. 3 hosts ×
    /// 2 lanes = 6 rejections.
    #[test]
    fn discovered_private_and_onion_relays_are_rejected_and_counted() {
        let author = Keys::generate();
        let public = relay("wss://relay.example.com");
        let loopback = relay("ws://127.0.0.1:7777");
        let rfc1918 = relay("ws://10.0.0.5");
        let onion = relay("ws://expyuzz4wqqyqhjn.onion");

        // Secure default: empty allowlist.
        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        );
        ingest_relay_list(&mut core, &author, &[&public, &loopback, &rfc1918, &onion]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([public.clone()]),
            "only the public relay may become a discovered write lane"
        );
        let author_hex = author.public_key().to_hex();
        let admitted_reads: BTreeSet<RelayUrl> = core
            .directory
            .read_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();
        assert_eq!(
            admitted_reads,
            BTreeSet::from([public]),
            "the read lane is gated identically -- no hostile host leaks in via read"
        );
        assert_eq!(
            core.discovered_private_relays_rejected, 6,
            "3 hostile hosts rejected on each of the write AND read parse of the one event"
        );
        assert_eq!(
            core.diagnostics_snapshot()
                .discovered_private_relays_rejected,
            6,
            "the rejection count must be visible in diagnostics (issue #121)"
        );
    }

    /// A user who EXPLICITLY opts a local host in re-admits a DISCOVERED relay
    /// on exactly that host -- provenance the transport layer lacks, which is
    /// why this decision lives in the engine. A different local host stays
    /// rejected.
    #[test]
    fn user_configured_local_host_admits_that_discovered_relay() {
        let author = Keys::generate();
        let opted_in = relay("ws://127.0.0.1:7777");
        let other_local = relay("ws://10.0.0.5");

        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        )
        .with_relay_admission(RelayAdmissionPolicy::new(["127.0.0.1".to_string()]));
        ingest_relay_list(&mut core, &author, &[&opted_in, &other_local]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([opted_in]),
            "the opted-in local host is admitted; a different local host is not"
        );
        assert_eq!(
            core.discovered_private_relays_rejected, 2,
            "only the non-opted-in local host is rejected -- once per lane parse"
        );
    }

    /// The "HOST, never path" falsifier at the engine layer: a real per-user
    /// relay served at a URL PATH is public and must be admitted from
    /// discovery, untouched by the SSRF gate.
    #[test]
    fn discovered_public_host_at_a_path_is_admitted() {
        let author = Keys::generate();
        let per_user = relay("wss://nostr.wine/npub1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");

        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        );
        ingest_relay_list(&mut core, &author, &[&per_user]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([per_user]),
            "a public host with a per-user path must pass admission -- the path is not a host"
        );
        assert_eq!(core.discovered_private_relays_rejected, 0);
    }
}

#[cfg(test)]
mod relay_health_tests {
    use super::*;
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;

    #[test]
    fn verifier_outage_reaches_engine_diagnostics_without_false_misbehavior() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        let handle = TransportRelayHandle {
            slot: 7,
            generation: 1,
        };
        let session = RelaySessionKey::public(RelayUrl::parse("wss://health.example.com").unwrap());
        let health = RelayHealth {
            last_error: Some("signature verification worker unavailable".to_string()),
            invalid_signature_count: 0,
            ..RelayHealth::default()
        };

        // Health for a slot never seen connected is ignored (#8): it can
        // name no verified (handle, session) pair to attribute itself to.
        assert!(core
            .handle(EngineMsg::RelayHealth(
                handle,
                session.clone(),
                health.clone(),
            ))
            .is_empty());
        assert!(core.diagnostics_snapshot().transport_degraded.is_none());

        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        let effects = core.handle(EngineMsg::RelayHealth(handle, session, health));
        assert!(effects.iter().any(|effect| {
            matches!(effect, Effect::EmitDiagnostics(snapshot)
                if snapshot.transport_degraded.as_deref()
                    == Some("signature verification worker unavailable"))
        }));
        assert_eq!(
            core.diagnostics_snapshot().transport_degraded.as_deref(),
            Some("signature verification worker unavailable")
        );
    }
}
