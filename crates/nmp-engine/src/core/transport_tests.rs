//! Ownership-domain tests moved with the implementation they falsify.

use super::*;
use nmp_grammar::RelaySessionKey;

#[cfg(test)]
mod relay_session_key_tests {
    use super::*;
    use nmp_store::{coverage_key, CoverageInterval, RedbStore};
    use nostr::{Keys, SubscriptionId};

    fn relay() -> RelayUrl {
        RelayUrl::parse("wss://session.example.com").unwrap()
    }

    #[test]
    fn wrong_context_eose_cannot_consume_or_credit_another_session() {
        let relay = relay();
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let access_a = Some(a);
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let atom = ContextualAtom {
            filter: filter.clone(),
            routing: ReadRouting::Auto,
            authenticate_as: access_a,
            routing_evidence: BTreeSet::new(),
        };
        let key = coverage_key(&atom);
        let sub_id = SubId::allocate(relay.clone(), &ReadRouting::Auto, access_a, 1000);
        let session_a = RelaySessionKey::new(relay.clone(), access_a);
        let session_b = RelaySessionKey::new(relay, Some(b));
        let mut attribution = AttributionState::new();
        attribution.set_active_demand([&atom]);
        attribution.record_send(
            &session_a,
            &sub_id,
            &filter,
            BTreeSet::from([key]),
            EventFailureTarget::ThisSend,
        );
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
        let session = RelaySessionKey::unauthenticated(relay.clone());
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            until: Some(150),
            ..ConcreteFilter::default()
        };
        let atom = ContextualAtom {
            filter: filter.clone(),
            routing: ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let key = coverage_key(&atom);
        let sub_id = SubId::allocate(relay, &ReadRouting::Auto, None, 1001);
        let mut attribution = AttributionState::new();
        attribution.observe_atom(&atom);
        let completed_send = attribution.record_send(
            &session,
            &sub_id,
            &filter,
            BTreeSet::from([key.clone()]),
            EventFailureTarget::ThisSend,
        );
        let replay_filter = ConcreteFilter {
            since: Some(100),
            ..filter.clone()
        };
        let replay_sub_id = SubId::allocate(session.relay.clone(), &ReadRouting::Auto, None, 1002);
        assert_ne!(sub_id, replay_sub_id, "changed bytes require a fresh id");
        attribution.record_send(
            &session,
            &replay_sub_id,
            &replay_filter,
            BTreeSet::from([key.clone()]),
            EventFailureTarget::ThisSend,
        );

        assert_eq!(
            attribution
                .attribute_correlated_completion(
                    &session,
                    &wire_sub_id_string(&sub_id),
                    completed_send,
                    Timestamp::from(200u64),
                )
                .expect("correlated completion")
                .eligible_claims()
                .expect("eligible completion"),
            vec![(
                key.clone(),
                nmp_store::CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(150u64),),
            )]
            .as_slice()
        );
        assert_eq!(
            attribution.attribute_eose(
                &session,
                &wire_sub_id_string(&replay_sub_id),
                Timestamp::from(200u64),
            ),
            vec![(
                key,
                nmp_store::CoverageInterval::new(Timestamp::from(100u64), Timestamp::from(150u64),),
            )]
        );
    }

    #[test]
    fn event_commit_poison_is_fifo_scoped_monotonic_and_retires_with_owners() {
        let relay = relay();
        let public_session = RelaySessionKey::unauthenticated(relay.clone());
        let protected_session =
            RelaySessionKey::new(relay.clone(), Some(Keys::generate().public_key()));
        let filter_a = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let filter_b = ConcreteFilter {
            kinds: Some(BTreeSet::from([2])),
            ..ConcreteFilter::default()
        };
        let atom_a = ContextualAtom {
            filter: filter_a.clone(),
            routing: ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let atom_b = ContextualAtom {
            filter: filter_b.clone(),
            routing: ReadRouting::Auto,
            authenticate_as: protected_session.authenticate_as,
            routing_evidence: BTreeSet::new(),
        };
        let key_a = coverage_key(&atom_a);
        let key_b = coverage_key(&atom_b);
        let sub_a = SubId::allocate(relay.clone(), &ReadRouting::Auto, None, 1003);
        let sub_b = SubId::allocate(
            relay,
            &ReadRouting::Auto,
            protected_session.authenticate_as,
            1004,
        );
        let wire_a = wire_sub_id_string(&sub_a);
        let wire_b = wire_sub_id_string(&sub_b);
        let mut attribution = AttributionState::new();
        attribution.set_active_demand([&atom_a, &atom_b]);

        for _ in 0..2 {
            attribution.record_send(
                &public_session,
                &sub_a,
                &filter_a,
                BTreeSet::from([key_a.clone()]),
                EventFailureTarget::ThisSend,
            );
        }
        attribution.record_send(
            &protected_session,
            &sub_b,
            &filter_b,
            BTreeSet::from([key_b.clone()]),
            EventFailureTarget::ThisSend,
        );

        attribution.poison_event_commit_failure(&public_session, &wire_a);
        attribution.record_send(
            &public_session,
            &sub_a,
            &filter_a,
            BTreeSet::from([key_a.clone()]),
            EventFailureTarget::ThisSend,
        );

        for _ in 0..2 {
            assert!(attribution
                .attribute_eose_detailed(&public_session, &wire_a, Timestamp::from(10u64))
                .expect("poisoned completion")
                .eligible_claims()
                .is_none());
        }
        assert_eq!(
            attribution
                .attribute_eose_detailed(&public_session, &wire_a, Timestamp::from(10u64))
                .expect("later revision")
                .eligible_claims()
                .expect("later revision remains eligible"),
            vec![(
                key_a.clone(),
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(10u64)),
            )]
            .as_slice()
        );
        assert_eq!(
            attribution
                .attribute_eose_detailed(&protected_session, &wire_b, Timestamp::from(10u64))
                .expect("isolated protected completion")
                .eligible_claims()
                .expect("protected request stays eligible"),
            vec![(
                key_b,
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(10u64)),
            )]
            .as_slice()
        );

        attribution.record_send(
            &public_session,
            &sub_a,
            &filter_a,
            BTreeSet::from([key_a.clone()]),
            EventFailureTarget::ThisSend,
        );
        attribution.poison_event_commit_failure(&public_session, &wire_a);
        attribution.clear_session(&public_session);
        assert!(attribution
            .attribute_eose_detailed(&public_session, &wire_a, Timestamp::from(10u64))
            .is_none());

        attribution.record_send(
            &public_session,
            &sub_a,
            &filter_a,
            BTreeSet::from([key_a]),
            EventFailureTarget::ThisSend,
        );
        attribution.discard_sub(&sub_a);
        assert!(attribution
            .attribute_eose_detailed(&public_session, &wire_a, Timestamp::from(10u64))
            .is_none());
    }

    #[test]
    fn missing_id_event_failure_poisons_the_original_neg_send() {
        let relay = relay();
        let session = RelaySessionKey::unauthenticated(relay.clone());
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let atom = ContextualAtom {
            filter: filter.clone(),
            routing: ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let key = coverage_key(&atom);
        let neg_sub = SubId::allocate(relay.clone(), &ReadRouting::Auto, None, 1005);
        let backfill_filter = ConcreteFilter {
            ids: Some(BTreeSet::from(["01".repeat(32)])),
            ..ConcreteFilter::default()
        };
        let backfill_sub = SubId::allocate(relay, &ReadRouting::Auto, None, 1006);
        let mut attribution = AttributionState::new();
        let neg_send = attribution.record_send(
            &session,
            &neg_sub,
            &filter,
            BTreeSet::from([key]),
            EventFailureTarget::ThisSend,
        );
        attribution.record_send(
            &session,
            &backfill_sub,
            &backfill_filter,
            BTreeSet::new(),
            EventFailureTarget::Correlated(neg_send),
        );

        attribution.poison_event_commit_failure(&session, &wire_sub_id_string(&backfill_sub));

        assert!(attribution
            .attribute_correlated_completion(
                &session,
                &wire_sub_id_string(&neg_sub),
                neg_send,
                Timestamp::from(10u64),
            )
            .expect("original NEG completion")
            .eligible_claims()
            .is_none());
    }

    #[test]
    fn disconnecting_a_preserves_public_and_b_sessions() {
        let relay = relay();
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let unauthenticated = RelaySessionKey::unauthenticated(relay.clone());
        let session_a = RelaySessionKey::new(relay.clone(), Some(a));
        let session_b = RelaySessionKey::new(relay, Some(b));
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
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
        core.handle(EngineMsg::RelayConnected(
            handles[0],
            unauthenticated.clone(),
        ));
        core.handle(EngineMsg::RelayConnected(handles[1], session_a.clone()));
        core.handle(EngineMsg::RelayConnected(handles[2], session_b.clone()));

        core.handle(EngineMsg::RelayDisconnected(
            handles[1],
            session_a.clone(),
            DisconnectReason::Closed,
        ));

        assert!(core.connected_relays.contains(&unauthenticated));
        assert!(!core.connected_relays.contains(&session_a));
        assert!(core.connected_relays.contains(&session_b));
    }

    #[test]
    fn protected_neg_frames_cannot_resolve_the_public_probe_or_inherit_its_diagnostics() {
        let relay = relay();
        let unauthenticated = RelaySessionKey::unauthenticated(relay.clone());
        let protected = RelaySessionKey::new(relay.clone(), Some(Keys::generate().public_key()));
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let atoms = BTreeSet::from([
            ContextualAtom {
                filter: filter.clone(),
                routing: ReadRouting::Explicit(vec![relay.clone()]),
                authenticate_as: None,
                routing_evidence: BTreeSet::new(),
            },
            ContextualAtom {
                filter,
                routing: ReadRouting::Explicit(vec![relay.clone()]),
                authenticate_as: protected.authenticate_as,
                routing_evidence: BTreeSet::new(),
            },
        ]);
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
        core.white_box("router.compile", |s| {
            s.router.compile(&atoms, &s.routing_facts, s.cap)
        });
        let unauthenticated_handle = TransportRelayHandle {
            slot: 5,
            generation: 1,
        };
        let protected_handle = TransportRelayHandle {
            slot: 6,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(
            unauthenticated_handle,
            unauthenticated.clone(),
        ));
        core.handle(EngineMsg::RelayConnected(
            protected_handle,
            protected.clone(),
        ));
        let probe = core.white_box("prober.begin_probe", |s| {
            s.prober.begin_probe(&relay).unwrap()
        });
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
            .find(|entry| entry.authenticate_as.is_none())
            .unwrap();
        let protected_diagnostics = probing
            .relays
            .iter()
            .find(|entry| entry.authenticate_as == protected.authenticate_as)
            .unwrap();
        assert_eq!(public_diagnostics.nip77_behavior, "probing");
        assert_eq!(protected_diagnostics.nip77_behavior, "unknown");

        let public_neg_msg = RelayFrame::from(RelayMessage::NegMsg {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(wire_id)),
            message: std::borrow::Cow::Owned("6100".to_string()),
        });
        core.handle(EngineMsg::RelayFrame(
            unauthenticated_handle,
            unauthenticated,
            public_neg_msg,
        ));
        assert_eq!(
            core.prober.state(&relay),
            crate::negentropy::ProbeState::Supported
        );
        let resolved = core.diagnostics_snapshot();
        assert_eq!(
            resolved
                .relays
                .iter()
                .find(|entry| entry.authenticate_as.is_none())
                .unwrap()
                .nip77_behavior,
            "behaviorally_proven"
        );
        assert_eq!(
            resolved
                .relays
                .iter()
                .find(|entry| entry.authenticate_as == protected.authenticate_as)
                .unwrap()
                .nip77_behavior,
            "unknown"
        );
    }

    #[test]
    fn intentional_close_never_reopens_a_still_planned_session() {
        let relay = relay();
        let session = RelaySessionKey::unauthenticated(relay.clone());
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1])),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![relay]),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
        core.white_box("router.compile", |s| {
            s.router
                .compile(&BTreeSet::from([atom]), &s.routing_facts, s.cap)
        });
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

    fn key() -> PublishQueueLaneKey {
        PublishQueueLaneKey {
            intent_id: IntentId(42),
            event_id: EventId::from_byte_array([42; 32]),
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
            RelayAckClass::Transient(PublishQueueTransientCause::RelayRateLimited)
        );
        assert_eq!(
            classify_relay_ack(false, "error: temporary relay failure"),
            RelayAckClass::Transient(PublishQueueTransientCause::RelayError)
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
                &PublishQueueLaneKey {
                    intent_id: IntentId(43),
                    event_id: EventId::from_byte_array([43; 32]),
                    relay: key.relay,
                },
                1
            ),
            "this fixture must prove persisted attempt identity participates in jitter"
        );
    }
}

#[cfg(test)]
mod relay_health_tests {
    use super::*;
    use nmp_store::RedbStore;

    #[test]
    fn verifier_outage_reaches_engine_diagnostics_without_false_misbehavior() {
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
        let handle = TransportRelayHandle {
            slot: 7,
            generation: 1,
        };
        let session =
            RelaySessionKey::unauthenticated(RelayUrl::parse("wss://health.example.com").unwrap());
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
