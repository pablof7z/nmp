use super::*;

// ---- write outbox scheduling -------------------------------------------

/// Test 4 analog: `enqueue_is_not_converged` (ledger #9). A durable
/// publish's FIRST status is `Accepted`, never a terminal; an `Ephemeral`
/// intent gets a receipt-only record (still fires onto the wire once
/// signed, but never gains a pending row); an `AtMostOnce` intent sends exactly once and a relay dropping
/// before it acks never produces a retry `PublishEvent`.
#[test]
fn enqueue_is_not_converged() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &relay0, a.public_key());
    authenticate_signer(&mut core, 0, &relay0, &a);
    let session = signer_session(&relay0, a.public_key());

    // -- Durable: first status is Accepted, never a bool/terminal. --
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "durable write")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    assert!(
        matches!(
            effects.first(),
            Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
        ),
        "the first emitted status for a durable publish must be Accepted, never a terminal"
    );
    assert_eq!(sink.0.lock().unwrap().first(), Some(&WriteStatus::Accepted));

    // -- Ephemeral: receipt-only, no durable delivery obligation. --
    let eph_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 2, "ephemeral write")),
            durability: Durability::Ephemeral,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(eph_sink.clone()),
    ));
    assert!(matches!(
        effects.first(),
        Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
    ));
    assert_eq!(
        eph_sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Accepted]
    );
    let (eph_id, eph_generation, eph_unsigned) = find_sign_request(&effects);
    let eph_signed = eph_unsigned.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        eph_id,
        eph_generation,
        Ok(eph_signed),
    ));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(r, _, _) if r == &session)),
        "an ephemeral write is fire-and-forget -- it still reaches the wire"
    );
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::EmitReceipt(_, WriteStatus::Signed(_)))));

    // -- AtMostOnce: sends exactly once; a dropped relay never retries. --
    let amo_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 3, "at most once write")),
            durability: Durability::AtMostOnce,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(amo_sink.clone()),
    ));
    let (amo_id, amo_generation, amo_unsigned) = find_sign_request(&effects);
    let amo_signed = amo_unsigned.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        amo_id,
        amo_generation,
        Ok(amo_signed),
    ));
    let publish_count = effects
        .iter()
        .filter(|e| matches!(e, Effect::PublishEvent(r, _, _) if r == &session))
        .count();
    assert_eq!(publish_count, 1, "at-most-once sends exactly once");

    let correlation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(relay, _, correlation) if relay == &session => Some(*correlation),
            _ => None,
        })
        .unwrap();
    let effects = core.handle(EngineMsg::EventHandoff(
        correlation,
        HandoffResult::Ambiguous,
    ));
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::OutcomeUnknown(r)) if *rid == amo_id && r == &relay0)
        ),
        "an ambiguous at-most-once handoff must become terminal OutcomeUnknown"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "no retry Effect::PublishEvent after a failure -- no blind retry"
    );
}

#[test]
fn ordinary_author_relay_without_auth_challenge_publishes_and_acks() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://ordinary-no-auth.example").unwrap();
    let handle = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let mut core = new_core(FixtureDirectory::new());
    let sink = CapturingReceiptSink::default();
    let (receipt, event, offline) =
        publish_private(&mut core, &author, [relay.clone()], sink.clone());
    assert!(!offline
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let parked = connect_signer(&mut core, 0, &relay, author.public_key());
    assert!(!parked
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let scheduled = release_author_probe(&mut core, handle, &relay, author.public_key());
    assert!(scheduled.iter().any(|effect| matches!(
        effect,
        Effect::PublishEvent(session, candidate, _)
            if session == &signer_session(&relay, author.public_key())
                && candidate.id == event.id
    )));
    mark_written(&mut core, &scheduled, &relay);
    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, author.public_key()),
        RelayFrame::from(RelayMessage::ok(event.id, true, "saved")),
    ));
    assert!(acked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteStatus::Acked(candidate))
            if *id == receipt && candidate == &relay
    )));
    assert!(sink.0.lock().unwrap().contains(&WriteStatus::Acked(relay)));
}

#[test]
fn challenged_author_relay_suppresses_event_until_exact_auth_ready() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://protected-pre-auth.example").unwrap();
    let session = signer_session(&relay, author.public_key());
    let handle = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let mut core = new_core(FixtureDirectory::new());
    let owned = core.handle(EngineMsg::Subscribe(
        protected_pinned_query(&relay, author.public_key(), 1),
        Box::new(CapturingSink::default()),
    ));
    let subscription = subscribed_handle(&owned);
    connect_signer(&mut core, 0, &relay, author.public_key());
    let challenge = core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::Auth {
            challenge: Cow::Borrowed("protect-before-event"),
        }),
    ));
    let policy_token = challenge
        .iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. })
                if token.epoch.session == session =>
            {
                Some(token.clone())
            }
            _ => None,
        })
        .expect("proactive challenge requests exact-session policy");
    let released = release_author_probe(&mut core, handle, &relay, author.public_key());
    assert!(!released
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));

    let sink = CapturingReceiptSink::default();
    let (_, event, scheduled) = publish_private(&mut core, &author, [relay.clone()], sink.clone());
    assert!(!scheduled
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(sink
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::AwaitingAuth { relay }));
    let ready = finish_authentication(&mut core, handle, session.clone(), &author, policy_token);
    assert_eq!(
        ready
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::PublishEvent(candidate, published, _)
                    if candidate == &session && published.id == event.id
            ))
            .count(),
        1,
        "the proactive challenge's exact AUTH OK releases the EVENT once"
    );
    core.handle(EngineMsg::Unsubscribe(subscription));
}

#[test]
fn auth_required_session_reconnect_cannot_publish_before_fresh_generation_auth() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://protected-reconnect-write.example").unwrap();
    let session = signer_session(&relay, author.public_key());
    let generation_one = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let generation_two = RelayHandle {
        slot: 0,
        generation: 2,
    };
    let mut core = new_core(FixtureDirectory::new());
    let subscribed = core.handle(EngineMsg::Subscribe(
        protected_pinned_query(&relay, author.public_key(), 1),
        Box::new(CapturingSink::default()),
    ));
    let subscription = subscribed_handle(&subscribed);
    core.handle(EngineMsg::RelayConnected(generation_one, session.clone()));
    authenticate_signer_generation(&mut core, generation_one, &relay, &author);
    core.handle(EngineMsg::RelayDisconnected(
        generation_one,
        session.clone(),
        nmp_transport::DisconnectReason::Error,
    ));
    core.handle(EngineMsg::RelayConnected(generation_two, session.clone()));
    let released = release_author_probe(&mut core, generation_two, &relay, author.public_key());
    assert!(!released
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));

    let sink = CapturingReceiptSink::default();
    let (_, event, parked) = publish_private(&mut core, &author, [relay.clone()], sink.clone());
    assert!(!parked
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(sink.0.lock().unwrap().contains(&WriteStatus::AwaitingAuth {
        relay: relay.clone(),
    }));

    let ready = authenticate_signer_generation(&mut core, generation_two, &relay, &author);
    assert_eq!(
        ready
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::PublishEvent(candidate, published, _)
                    if candidate == &session && published.id == event.id
            ))
            .count(),
        1,
        "fresh exact-generation AUTH readiness releases the parked EVENT once"
    );
    core.handle(EngineMsg::Unsubscribe(subscription));
}

#[test]
fn stale_auth_probe_release_after_reconnect_cannot_wake_current_generation() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://ordinary-probe-reconnect.example").unwrap();
    let session = signer_session(&relay, author.public_key());
    let generation_one = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let generation_two = RelayHandle {
        slot: 0,
        generation: 2,
    };
    let mut core = new_core(FixtureDirectory::new());
    let subscribed = core.handle(EngineMsg::Subscribe(
        protected_pinned_query(&relay, author.public_key(), 1),
        Box::new(CapturingSink::default()),
    ));
    let subscription = subscribed_handle(&subscribed);
    core.handle(EngineMsg::RelayConnected(generation_one, session.clone()));
    core.handle(EngineMsg::RelayDisconnected(
        generation_one,
        session.clone(),
        nmp_transport::DisconnectReason::Error,
    ));
    core.handle(EngineMsg::RelayConnected(generation_two, session.clone()));
    let sink = CapturingReceiptSink::default();
    let (_, event, parked) = publish_private(&mut core, &author, [relay.clone()], sink);
    assert!(!parked
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));

    let stale = release_author_probe(&mut core, generation_one, &relay, author.public_key());
    assert!(!stale
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let current = release_author_probe(&mut core, generation_two, &relay, author.public_key());
    assert_eq!(
        current
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::PublishEvent(candidate, published, _)
                    if candidate == &session && published.id == event.id
            ))
            .count(),
        1
    );
    core.handle(EngineMsg::Unsubscribe(subscription));
}

#[test]
fn offline_and_auth_waits_consume_no_attempts_and_auth_wake_uses_a_new_ordinal() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://auth-wait.example").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth-wait.redb");

    let (intent, event) = {
        let mut core = EngineCore::new(
            RedbStore::open(&path).unwrap(),
            Box::new(FixtureDirectory::new()),
            10,
        );
        let sink = CapturingReceiptSink::default();
        let (receipt, event, offline) =
            publish_private(&mut core, &author, [relay.clone()], sink.clone());
        let session = signer_session(&relay, event.pubkey);
        assert!(sink
            .0
            .lock()
            .unwrap()
            .contains(&WriteStatus::AwaitingRelay {
                relay: relay.clone(),
            }));
        assert!(offline
            .iter()
            .any(|effect| matches!(effect, Effect::EnsureRelay(r) if r == &session)));
        assert!(!offline
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        drop(core);

        let store = RedbStore::open(&path).unwrap();
        let intent = store.recover_outbox()[0].intent_id;
        assert!(store.recover_attempts(intent).unwrap().is_empty());
        drop(store);

        let mut core = EngineCore::new(
            RedbStore::open(&path).unwrap(),
            Box::new(FixtureDirectory::new()),
            10,
        );
        core.recover_on_boot();
        let recovered = CapturingReceiptSink::default();
        assert!(core
            .reattach_receipt(receipt, Box::new(recovered.clone()))
            .is_attached());
        assert!(recovered
            .0
            .lock()
            .unwrap()
            .contains(&WriteStatus::AwaitingRelay {
                relay: relay.clone(),
            }));
        connect_signer(&mut core, 0, &relay, event.pubkey);
        let first = release_author_probe(
            &mut core,
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            &relay,
            event.pubkey,
        );
        mark_written(&mut core, &first, &relay);
        let auth = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            session.clone(),
            RelayFrame::from(RelayMessage::ok(
                event.id,
                false,
                "auth-required: authenticate",
            )),
        ));
        assert!(!auth
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(auth.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::AwaitingAuth { relay: waiting })
                if waiting == &relay
        )));
        let auth_replay = CapturingReceiptSink::default();
        assert!(core
            .reattach_receipt(receipt, Box::new(auth_replay.clone()))
            .is_attached());
        let auth_replay = auth_replay.0.lock().unwrap();
        assert!(auth_replay.contains(&WriteStatus::Sent {
            relay: relay.clone(),
            attempt: 1,
            written_at: Timestamp::from(0),
        }));
        assert!(auth_replay.contains(&WriteStatus::AwaitingAuth {
            relay: relay.clone(),
        }));
        drop(auth_replay);
        assert_eq!(
            core.next_deadline(),
            None,
            "AUTH wait has no polling deadline"
        );
        assert!(!core
            .handle(EngineMsg::Tick(Timestamp::from(100_000)))
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));

        let handle = RelayHandle {
            slot: 0,
            generation: 1,
        };
        let challenge = core.handle(EngineMsg::RelayFrame(
            handle,
            session.clone(),
            RelayFrame::from(RelayMessage::Auth {
                challenge: Cow::Borrowed("retry challenge"),
            }),
        ));
        let policy_token = challenge
            .into_iter()
            .find_map(|effect| match effect {
                Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. }) => Some(token),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::AuthCapabilityBound {
            token: policy_token.clone(),
            capability: nmp_engine::core::AuthCapability::Policy,
            instance: AuthCapabilityInstance(1),
        });
        let signature = core.handle(EngineMsg::AuthPolicyCompleted(
            policy_token,
            Some(AuthCapabilityInstance(1)),
            AuthPolicyOutcome::Allow,
        ));
        let (sign_token, unsigned) = signature
            .into_iter()
            .find_map(|effect| match effect {
                Effect::RelayAuth(AuthEffect::RequestSignature { token, unsigned }) => {
                    Some((token, unsigned))
                }
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::AuthCapabilityBound {
            token: sign_token.clone(),
            capability: nmp_engine::core::AuthCapability::Signer,
            instance: AuthCapabilityInstance(2),
        });
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let send = core.handle(EngineMsg::AuthSignerCompleted(
            sign_token,
            Some(AuthCapabilityInstance(2)),
            AuthSignerOutcome::Signed(signed),
        ));
        let (send_token, auth_event) = send
            .into_iter()
            .find_map(|effect| match effect {
                Effect::RelayAuth(AuthEffect::Send { token, event, .. }) => Some((token, event)),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::AuthSendCompleted(
            send_token,
            AuthSendOutcome::Accepted,
        ));
        let second = core.handle(EngineMsg::RelayFrame(
            handle,
            session.clone(),
            RelayFrame::from(RelayMessage::ok(auth_event.id, true, "authenticated")),
        ));
        assert!(second.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                _,
                WriteStatus::RetryEligible {
                    relay: eligible,
                    attempt: 1,
                    eligible_at,
                }
            ) if eligible == &relay && *eligible_at == Timestamp::from(100_000)
        )));
        assert_eq!(
            second
                .iter()
                .filter(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &session))
                .count(),
            1
        );
        (intent, event)
    };

    let store = RedbStore::open(&path).unwrap();
    let attempts = store.recover_attempts(intent).unwrap();
    assert_eq!(
        attempts
            .iter()
            .map(|attempt| attempt.ordinal)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "offline/AUTH time allocates nothing; explicit AUTH wake allocates the next ordinal"
    );
    assert!(attempts.iter().all(|attempt| attempt.event == event));
}

/// A durable write parked `WaitingAuth` (the relay demanded auth in response
/// to the EVENT) must never wedge across a transport disconnect/reconnect.
/// The authenticated grant is generation-scoped, so on disconnect the lane
/// falls back to `WaitingConnection` and the fresh generation re-drives it:
/// re-send the EVENT, re-provoke the challenge, re-park, authenticate, wake.
/// Regression guard for the reconnect missed-wakeup the adversarial review
/// caught (the ONLY `WaitingAuth` wake is `finish_auth_ok`, which a
/// lazy-challenging relay never fires again without a client-provoked EVENT).
#[test]
fn parked_auth_write_is_redriven_across_reconnect_not_wedged() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://auth-reconnect.example").unwrap();
    let session = signer_session(&relay, author.public_key());

    let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
    let sink = CapturingReceiptSink::default();
    let (_receipt, event, _) = publish_private(&mut core, &author, [relay.clone()], sink.clone());

    // First generation: connect, release the bounded AUTH-discovery probe,
    // hand off, and let the relay demand auth via an `OK false
    // auth-required` on the durable EVENT. The lane parks and the relay is
    // now KNOWN to require auth for this exact session.
    connect_signer(&mut core, 0, &relay, author.public_key());
    let connected = release_author_probe(
        &mut core,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &relay,
        author.public_key(),
    );
    mark_written(&mut core, &connected, &relay);
    let parked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        session.clone(),
        RelayFrame::from(RelayMessage::ok(
            event.id,
            false,
            "auth-required: authenticate",
        )),
    ));
    assert!(parked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(_, WriteStatus::AwaitingAuth { relay: waiting }) if waiting == &relay
    )));

    // The socket drops mid-handshake (before any AUTH OK), and a fresh
    // generation reconnects. The relay actually REQUIRED auth for this
    // session (`auth_required_sessions` is sticky while the lane owns the
    // worker), so the unauthenticated reconnect must NOT re-drive the
    // publish: replaying the EVENT on a socket the relay already refused
    // pre-auth would only be refused again (#8: a new generation needs a
    // fresh challenge and matching AUTH OK before replay).
    core.handle(EngineMsg::RelayDisconnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        session.clone(),
        DisconnectReason::Closed,
    ));
    let mut reconnected = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 2,
        },
        session.clone(),
    ));
    reconnected.extend(core.handle(EngineMsg::RelayInformationResolved(relay.clone(), None)));
    assert!(
        !reconnected
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &session)),
        "an unauthenticated reconnect must not replay a write the relay \
         already refused pre-auth: {reconnected:?}"
    );

    // Only the fresh generation's own challenge + matching AUTH OK re-drives
    // the parked lane — exactly once.
    let ready = authenticate_signer_generation(
        &mut core,
        RelayHandle {
            slot: 0,
            generation: 2,
        },
        &relay,
        &author,
    );
    assert_eq!(
        ready
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &session))
            .count(),
        1,
        "the fresh generation's AUTH OK must re-drive the parked auth write \
         exactly once, not leave it wedged: {ready:?}"
    );
}

/// The boot-path analog of the reconnect re-drive: a durable write persisted
/// `WaitingAuth` must not survive a restart as `WaitingAuth` (its
/// authenticated grant was generation-scoped to a socket the prior process
/// held). `recover_on_boot` recovers it as `WaitingConnection`, so the first
/// post-boot connect re-drives it instead of stranding it.
#[test]
fn boot_recovers_parked_auth_write_as_redrivable_not_wedged() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://auth-boot.example").unwrap();
    let session = signer_session(&relay, author.public_key());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth-boot.redb");

    let event = {
        let mut core = EngineCore::new(
            RedbStore::open(&path).unwrap(),
            Box::new(FixtureDirectory::new()),
            10,
        );
        let (_receipt, event, _) = publish_private(
            &mut core,
            &author,
            [relay.clone()],
            CapturingReceiptSink::default(),
        );
        connect_signer(&mut core, 0, &relay, author.public_key());
        let connected = release_author_probe(
            &mut core,
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            &relay,
            author.public_key(),
        );
        mark_written(&mut core, &connected, &relay);
        let parked = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            session.clone(),
            RelayFrame::from(RelayMessage::ok(
                event.id,
                false,
                "auth-required: authenticate",
            )),
        ));
        assert!(parked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::AwaitingAuth { relay: waiting }) if waiting == &relay
        )));
        event
    };

    // Fresh process: recover from the persisted store, then connect.
    let mut core = EngineCore::new(
        RedbStore::open(&path).unwrap(),
        Box::new(FixtureDirectory::new()),
        10,
    );
    let recovery = core.recover_on_boot();
    assert!(
        recovery
            .iter()
            .any(|effect| matches!(effect, Effect::EnsureRelay(r) if r == &session)),
        "boot must redial the exact authenticated session for the recovered lane"
    );
    // The fresh process has no in-memory auth-required fact for this relay,
    // so the recovered lane rides the ordinary bounded AUTH-discovery path:
    // connect parks it behind the probe, and the transport's ordered
    // first-read completion re-drives it (a relay still requiring auth would
    // instead deliver its challenge inside that window and park it as
    // WaitingAuth until the fresh AUTH OK).
    connect_signer(&mut core, 0, &relay, author.public_key());
    let released = release_author_probe(
        &mut core,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &relay,
        author.public_key(),
    );
    assert!(
        released.iter().any(
            |effect| matches!(effect, Effect::PublishEvent(r, current, _)
                if r == &session && current.id == event.id)
        ),
        "boot-recovered auth write must re-drive on the first probe release, not stay wedged"
    );
}

#[test]
fn restart_reattachment_preserves_every_active_retry_fact_exactly() {
    let author = Keys::generate();
    let offline = RelayUrl::parse("wss://restart-offline.example").unwrap();
    let auth = RelayUrl::parse("wss://restart-auth.example").unwrap();
    let retry = RelayUrl::parse("wss://restart-retry.example").unwrap();
    let ambiguous = RelayUrl::parse("wss://restart-ambiguous.example").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retry-receipt-restart.redb");

    let (receipt, retry_at) = {
        let mut core = EngineCore::new(
            RedbStore::open(&path).unwrap(),
            Box::new(FixtureDirectory::new()),
            10,
        );
        connect_signer(&mut core, 0, &auth, author.public_key());
        connect_signer(&mut core, 1, &retry, author.public_key());
        connect_signer(&mut core, 2, &ambiguous, author.public_key());
        authenticate_signer(&mut core, 0, &auth, &author);
        authenticate_signer(&mut core, 1, &retry, &author);
        authenticate_signer(&mut core, 2, &ambiguous, &author);
        let sink = CapturingReceiptSink::default();
        let (receipt, event, scheduled) = publish_private(
            &mut core,
            &author,
            [
                offline.clone(),
                auth.clone(),
                retry.clone(),
                ambiguous.clone(),
            ],
            sink,
        );

        let _ = core.handle(EngineMsg::Tick(Timestamp::from(11)));
        mark_written(&mut core, &scheduled, &auth);
        let auth_wait = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            signer_session(&auth, event.pubkey),
            RelayFrame::from(RelayMessage::ok(
                event.id,
                false,
                "auth-required: authenticate",
            )),
        ));
        assert!(auth_wait.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::AwaitingAuth { relay }) if relay == &auth
        )));

        let _ = core.handle(EngineMsg::Tick(Timestamp::from(12)));
        mark_written(&mut core, &scheduled, &retry);
        let retry_wait = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 1,
                generation: 1,
            },
            signer_session(&retry, event.pubkey),
            RelayFrame::from(RelayMessage::ok(event.id, false, "rate-limited: slow down")),
        ));
        let retry_at = retry_wait
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(
                    _,
                    WriteStatus::RetryEligible {
                        relay,
                        attempt: 1,
                        eligible_at,
                    },
                ) if relay == &retry => Some(*eligible_at),
                _ => None,
            })
            .expect("transient classification must expose its exact persisted deadline");

        let _ = core.handle(EngineMsg::Tick(Timestamp::from(13)));
        let ambiguous_correlation = scheduled
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(relay, _, correlation)
                    if relay == &signer_session(&ambiguous, event.pubkey) =>
                {
                    Some(*correlation)
                }
                _ => None,
            })
            .unwrap();
        let ambiguity = core.handle(EngineMsg::EventHandoff(
            ambiguous_correlation,
            HandoffResult::Ambiguous,
        ));
        assert!(ambiguity.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                _,
                WriteStatus::HandoffAmbiguous {
                    relay,
                    attempt: 1,
                    observed_at,
                },
            ) if relay == &ambiguous && *observed_at == Timestamp::from(13)
        )));
        (receipt, retry_at)
    };

    let mut recovered = EngineCore::new(
        RedbStore::open(&path).unwrap(),
        Box::new(FixtureDirectory::new()),
        10,
    );
    recovered.recover_on_boot();
    let replay = CapturingReceiptSink::default();
    assert!(recovered
        .reattach_receipt(receipt, Box::new(replay.clone()))
        .is_attached());
    let replay = replay.0.lock().unwrap();

    assert!(replay.contains(&WriteStatus::AwaitingRelay {
        relay: offline.clone(),
    }));
    assert!(replay.contains(&WriteStatus::Sent {
        relay: auth.clone(),
        attempt: 1,
        written_at: Timestamp::from(11),
    }));
    assert!(replay.contains(&WriteStatus::AwaitingAuth {
        relay: auth.clone(),
    }));
    assert!(replay.contains(&WriteStatus::Sent {
        relay: retry.clone(),
        attempt: 1,
        written_at: Timestamp::from(12),
    }));
    assert!(replay.contains(&WriteStatus::RetryEligible {
        relay: retry.clone(),
        attempt: 1,
        eligible_at: retry_at,
    }));
    assert!(replay.contains(&WriteStatus::HandoffAmbiguous {
        relay: ambiguous.clone(),
        attempt: 1,
        observed_at: Timestamp::from(13),
    }));
    assert!(!replay.iter().any(
        |status| matches!(status, WriteStatus::Sent { relay, .. } if relay == &ambiguous || relay == &offline)
    ));
}

#[test]
fn transient_deadline_is_consumed_once_without_polling_or_duplicate_queue() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://transient-retry.example").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    connect_signer(&mut core, 0, &relay, author.public_key());
    authenticate_signer(&mut core, 0, &relay, &author);
    let sink = CapturingReceiptSink::default();
    let (receipt, event, first) =
        publish_private(&mut core, &author, [relay.clone()], sink.clone());
    mark_written(&mut core, &first, &relay);
    let classified = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, event.pubkey),
        RelayFrame::from(RelayMessage::ok(event.id, false, "rate-limited: slow down")),
    ));
    assert!(!classified
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let due = core
        .next_deadline()
        .expect("transient retry must arm one deadline");
    assert!((3..8).contains(&due.as_secs()));
    assert!(sink
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::RetryEligible {
            relay: relay.clone(),
            attempt: 1,
            eligible_at: due,
        }));
    let replay = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(receipt, Box::new(replay.clone()))
        .is_attached());
    assert!(replay
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::RetryEligible {
            relay: relay.clone(),
            attempt: 1,
            eligible_at: due,
        }));

    assert!(!core
        .handle(EngineMsg::Tick(Timestamp::from(due.as_secs() - 1)))
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let retry = core.handle(EngineMsg::Tick(due));
    assert_eq!(
        retry
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(r, e, _) if r == &signer_session(&relay, event.pubkey) && e.id == event.id))
            .count(),
        1
    );
    assert_eq!(
        core.next_deadline(),
        None,
        "the exposed due row is consumed before the next deadline is armed"
    );
    assert!(
        !core
            .handle(EngineMsg::Tick(due))
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))),
        "repeating the same tick cannot duplicate an already in-flight lane"
    );
}

/// #680 / #46: a PAUSED receipt consumer spanning many real, persisted
/// durable retry ordinals retains one finite live prefix. The retry scheduler
/// keeps advancing independently; the 33rd queued fact marks the stream
/// lagged, the reducer prunes that observer immediately, and the receiver gets
/// an explicit replay boundary after draining the retained prefix.
#[test]
fn paused_receipt_across_repeated_durable_retries_is_bounded_and_loud() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://paused-retry.example").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    connect_signer(&mut core, 0, &relay, author.public_key());
    authenticate_signer(&mut core, 0, &relay, &author);

    let (sender, receiver) = nmp_engine::runtime::fifo_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let sink = BoundedReceiptSink {
        sender,
        calls: calls.clone(),
    };
    let (receipt, event, mut scheduled) =
        publish_private(&mut core, &author, [relay.clone()], sink);

    for attempt in 1..=40 {
        mark_written(&mut core, &scheduled, &relay);
        let classified = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            signer_session(&relay, event.pubkey),
            RelayFrame::from(RelayMessage::ok(
                event.id,
                false,
                "rate-limited: bounded-retry-proof",
            )),
        ));
        assert!(classified.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                _,
                WriteStatus::RetryEligible {
                    relay: got,
                    attempt: got_attempt,
                    ..
                }
            ) if got == &relay && *got_attempt == attempt
        )));
        let due = core
            .next_deadline()
            .expect("every transient durable attempt schedules its retry");
        scheduled = core.handle(EngineMsg::Tick(due));
        assert!(scheduled
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    }

    assert_eq!(
        calls.load(Ordering::SeqCst),
        nmp_engine::runtime::FACT_CHANNEL_CAPACITY + 1,
        "the first rejected fact prunes the lagged sink; later retries never revisit it"
    );
    for _ in 0..nmp_engine::runtime::FACT_CHANNEL_CAPACITY {
        receiver.recv().expect("the retained bounded prefix drains");
    }
    assert_eq!(
        receiver.recv(),
        Err(nmp_engine::runtime::FifoRecvError::Lagged),
        "the missing suffix is explicit and requires durable replay"
    );

    let mut cursor = None;
    let mut replayed = Vec::new();
    loop {
        let (page_sender, page_receiver) = nmp_engine::runtime::fifo_channel();
        let (outcome, next_cursor) = core.reattach_receipt_page(
            receipt,
            Box::new(BoundedReceiptSink {
                sender: page_sender,
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            cursor,
            nmp_engine::runtime::FACT_CHANNEL_CAPACITY,
        );
        assert!(outcome.is_attached());
        let mut page = Vec::new();
        while let Ok(status) = page_receiver.try_recv() {
            page.push(status);
        }
        assert!(
            page.len() <= nmp_engine::runtime::FACT_CHANNEL_CAPACITY,
            "each durable replay page obeys the same finite delivery bound"
        );
        replayed.extend(page);
        match next_cursor {
            Some(next) => cursor = Some(next),
            None => {
                page_receiver.close();
                break;
            }
        }
    }
    assert!(
        replayed.len() > nmp_engine::runtime::FACT_CHANNEL_CAPACITY,
        "the cursor traverses more history than one in-memory page can retain"
    );
    assert!(replayed
        .iter()
        .any(|status| matches!(status, WriteStatus::RetryEligible { attempt: 40, .. })));

    let (full_sender, _full_receiver) = nmp_engine::runtime::fifo_channel();
    for _ in 0..nmp_engine::runtime::FACT_CHANNEL_CAPACITY {
        assert!(full_sender.send(WriteStatus::Accepted));
    }
    let (outcome, refused_cursor) = core.reattach_receipt_page(
        receipt,
        Box::new(BoundedReceiptSink {
            sender: full_sender,
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        None,
        nmp_engine::runtime::FACT_CHANNEL_CAPACITY,
    );
    assert!(outcome.is_attached());
    assert!(
        refused_cursor.is_some(),
        "a sink refusal cannot acknowledge or skip the first undelivered durable fact"
    );
}

/// #680: a continuation is per durable fact identity, never a count into a
/// replay vector rebuilt from mutable store state. Persisting a fact for an
/// earlier-sorted relay between page pulls must neither skip that new fact nor
/// shift a later relay's already-delivered facts back into the stream.
#[test]
fn live_receipt_mutation_between_pages_is_exactly_once() {
    let author = Keys::generate();
    let early = RelayUrl::parse("wss://a-early-page.example").unwrap();
    let late = RelayUrl::parse("wss://z-late-page.example").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    for (slot, relay) in [&early, &late].into_iter().enumerate() {
        connect_signer(&mut core, slot as u32, relay, author.public_key());
        authenticate_signer(&mut core, slot as u32, relay, &author);
    }

    let live = CapturingReceiptSink::default();
    let (receipt, event, mut scheduled) =
        publish_private(&mut core, &author, [early.clone(), late.clone()], live);
    let early_correlation = scheduled
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(candidate, _, correlation) if candidate.relay == early => {
                Some(*correlation)
            }
            _ => None,
        })
        .expect("the earlier relay owns one persisted first attempt");

    // Build more than one page entirely behind the later-sorted relay while
    // the earlier relay's Started attempt has no replay status yet.
    for attempt in 1..=20 {
        mark_written(&mut core, &scheduled, &late);
        let classified = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 1,
                generation: 1,
            },
            signer_session(&late, event.pubkey),
            RelayFrame::from(RelayMessage::ok(
                event.id,
                false,
                "rate-limited: mutable-page-proof",
            )),
        ));
        assert!(classified.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                _,
                WriteStatus::RetryEligible {
                    relay,
                    attempt: got,
                    ..
                }
            ) if relay == &late && *got == attempt
        )));
        let due = core
            .next_deadline()
            .expect("the later relay's retry remains scheduled");
        scheduled = core.handle(EngineMsg::Tick(due));
    }

    let mut cursor = None;
    let mut replayed = Vec::new();
    let (first_sender, first_receiver) = nmp_engine::runtime::fifo_channel();
    let (outcome, next_cursor) = core.reattach_receipt_page(
        receipt,
        Box::new(BoundedReceiptSink {
            sender: first_sender,
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        cursor,
        nmp_engine::runtime::FACT_CHANNEL_CAPACITY,
    );
    assert!(outcome.is_attached());
    cursor = next_cursor;
    while let Ok(status) = first_receiver.try_recv() {
        replayed.push(status);
    }
    assert_eq!(
        replayed.len(),
        nmp_engine::runtime::FACT_CHANNEL_CAPACITY,
        "the first page must cross into the later relay's durable history"
    );

    // This durable Sent fact sorts before every later-relay fact in the
    // rebuilt evidence vector. A numeric offset skips it and repeats the
    // shifted tail; the per-relay identity cursor must do neither.
    core.handle(EngineMsg::EventHandoff(
        early_correlation,
        HandoffResult::Written,
    ));

    while let Some(page_cursor) = cursor {
        let (page_sender, page_receiver) = nmp_engine::runtime::fifo_channel();
        let (outcome, next_cursor) = core.reattach_receipt_page(
            receipt,
            Box::new(BoundedReceiptSink {
                sender: page_sender,
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Some(page_cursor),
            nmp_engine::runtime::FACT_CHANNEL_CAPACITY,
        );
        assert!(outcome.is_attached());
        while let Ok(status) = page_receiver.try_recv() {
            replayed.push(status);
        }
        cursor = next_cursor;
        if cursor.is_none() {
            page_receiver.close();
        }
    }

    assert_eq!(
        replayed
            .iter()
            .filter(|status| matches!(
                status,
                WriteStatus::Sent {
                    relay,
                    attempt: 1,
                    ..
                } if relay == &early
            ))
            .count(),
        1,
        "the fact inserted before the old page boundary is delivered once"
    );
    for attempt in 1..=20 {
        assert_eq!(
            replayed
                .iter()
                .filter(|status| matches!(
                    status,
                    WriteStatus::Sent {
                        relay,
                        attempt: got,
                        ..
                    } if relay == &late && *got == attempt
                ))
                .count(),
            1,
            "later-relay Sent attempt {attempt} duplicated or disappeared"
        );
        assert_eq!(
            replayed
                .iter()
                .filter(|status| matches!(
                    status,
                    WriteStatus::RetryEligible {
                        relay,
                        attempt: got,
                        ..
                    } if relay == &late && *got == attempt
                ))
                .count(),
            1,
            "later-relay RetryEligible attempt {attempt} duplicated or disappeared"
        );
    }
    assert_eq!(
        replayed.len(),
        42,
        "one receipt status, forty later-relay facts, and one live mutation are exact"
    );
}

#[test]
fn scheduler_has_stable_order_and_enforces_global_and_per_relay_caps() {
    let author = Keys::generate();
    let mut relays = (0..33)
        .map(|i| RelayUrl::parse(&format!("wss://cap-{i:02}.example")).unwrap())
        .collect::<Vec<_>>();
    relays.sort();
    let mut core = new_core(FixtureDirectory::new());
    for (slot, relay) in relays.iter().enumerate() {
        connect_signer(&mut core, slot as u32, relay, author.public_key());
        authenticate_signer(&mut core, slot as u32, relay, &author);
    }
    let (_, event, first_wave) = publish_private(
        &mut core,
        &author,
        relays.clone(),
        CapturingReceiptSink::default(),
    );
    let published = first_wave
        .iter()
        .filter_map(|effect| match effect {
            Effect::PublishEvent(session, event, _)
                if session.access == AccessContext::Nip42(event.pubkey) =>
            {
                Some(session.relay.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(published, relays[..32]);

    let first = &relays[0];
    mark_written(&mut core, &first_wave, first);
    let released = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(first, event.pubkey),
        RelayFrame::from(RelayMessage::ok(event.id, true, "")),
    ));
    assert_eq!(
        released
            .iter()
            .filter_map(|effect| match effect {
                Effect::PublishEvent(session, event, _)
                    if session.access == AccessContext::Nip42(event.pubkey) =>
                {
                    Some(session.relay.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![relays[32].clone()],
        "freeing one global slot schedules the stable next lane"
    );
    assert!(!released.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(session, event, _)
            if session == &signer_session(first, event.pubkey))
    ));
}
