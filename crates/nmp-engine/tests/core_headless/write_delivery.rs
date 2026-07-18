use super::*;

// ---- durable write delivery and recovery -------------------------------

/// Test 5 analog: `private_route_fails_closed` (ledger #6). A
/// `PrivateNarrow` route whose relay set is empty (unroutable) fails CLOSED
/// with a typed `WriteStatus::Failed` -- it never reaches a public relay.
/// `NarrowOnly` exposes no widen/insert method by construction (compile-
/// level: there is no method this test -- or any caller -- could call to
/// grow the set after `NarrowOnly::new`).
#[test]
fn private_route_fails_closed() {
    let a = Keys::generate();
    // Deliberately empty directory: even if `PrivateNarrow` DID consult it
    // (it must not), there would be no public write relay to fall back to.
    let dir = FixtureDirectory::new();
    let mut core = new_core(dir);
    activate(&mut core, &a);

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "private dm")),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new(std::iter::empty::<RelayUrl>()),
            }),
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "an unroutable private recipient must never reach ANY relay, public or otherwise"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::Failed(_)) if *rid == id)),
        "must fail CLOSED with a typed error, not silently drop the write"
    );
    assert!(matches!(
        sink.0.lock().unwrap().last(),
        Some(WriteStatus::Failed(_))
    ));
}

/// Test 11 analog: `write_ack_per_relay`. A durable publish to two relays,
/// one OKs and one NACKs -- the receipt stream reaches `Acked(R_ok)` and
/// `Rejected(R_bad, reason)` independently; "is it sent?" is only readable
/// from the stream, never a single bool.
#[test]
fn one_attempt_start_failure_is_owned_nonterminal_and_never_hits_the_wire() {
    let author = Keys::generate();
    let good = RelayUrl::parse("wss://persisted.example").unwrap();
    let blocked = RelayUrl::parse("wss://blocked.example").unwrap();
    let store = SharedFailStartStore::new([blocked.clone()]);
    let sink = CapturingReceiptSink::default();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    connect_signer(&mut core, 0, &good, author.public_key());
    connect_signer(&mut core, 1, &blocked, author.public_key());
    authenticate_signer(&mut core, 0, &good, &author);
    authenticate_signer(&mut core, 1, &blocked, &author);

    let (id, _, effects) = publish_private(
        &mut core,
        &author,
        [good.clone(), blocked.clone()],
        sink.clone(),
    );
    assert!(effects.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(session, event, _)
            if session == &signer_session(&good, event.pubkey))
    ));
    assert!(!effects.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(session, event, _)
            if session == &signer_session(&blocked, event.pubkey))
    ));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(receipt, WriteStatus::PersistenceBlocked(relay))
            if *receipt == id && relay == &blocked
    )));
    let replay = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(id, Box::new(replay.clone()))
        .is_attached());
    assert!(replay
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::PersistenceBlocked(blocked)));
}

// ---- issue #93: durable EVENT handoff -----------------------------------

/// `Sent` must never fire synchronously at enqueue time -- the moment this
/// call returns effects for a signed publish is not the same fact as
/// transport confirming the write. Only `EngineMsg::EventHandoff(_,
/// Written)` may ever produce it (asserted below by actually driving that
/// message and observing exactly one `Sent`).
#[test]
fn sent_never_fires_synchronously_and_only_written_handoff_produces_it() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);
    let sink = CapturingReceiptSink::default();
    connect_signer(&mut core, 0, &relay, author.public_key());
    authenticate_signer(&mut core, 0, &relay, &author);

    let (id, _signed, effects) = publish_private(&mut core, &author, [relay.clone()], sink.clone());

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(_, WriteStatus::Sent { .. }))),
        "Sent must never fire synchronously at enqueue time, got {effects:?}"
    );
    assert!(
        !sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| matches!(s, WriteStatus::Sent { .. })),
        "the sink must not have observed Sent before any handoff result arrives"
    );

    let correlation = effects
        .iter()
        .find_map(|e| match e {
            Effect::PublishEvent(r, event, c) if r == &signer_session(&relay, event.pubkey) => {
                Some(*c)
            }
            _ => None,
        })
        .expect("a PublishEvent effect must have been emitted for this relay");

    let reattached = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(id, Box::new(reattached.clone()))
        .is_attached());
    assert!(
        !reattached
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|status| matches!(status, WriteStatus::Sent { .. })),
        "a persisted Started row is pre-wire and must not replay as Sent"
    );

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10)));
    let handoff_effects = core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    assert!(
        handoff_effects.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(
                receipt,
                WriteStatus::Sent {
                    relay: r,
                    attempt: 1,
                    written_at,
                }
            ) if *receipt == id && r == &relay && *written_at == Timestamp::from(10)
        )),
        "a Written handoff must emit exactly one Sent, got {handoff_effects:?}"
    );
    assert!(sink
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|s| matches!(s, WriteStatus::Sent { relay: r, .. } if r == &relay)));
    assert!(reattached
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|s| matches!(s, WriteStatus::Sent { relay: r, .. } if r == &relay)));

    // The SAME correlation resolving a second time (a defensive duplicate
    // delivery, which transport itself never actually produces) must be a
    // complete no-op -- the correlation was already consumed above.
    let repeat = core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    assert!(
        repeat.is_empty(),
        "an already-resolved correlation must never re-fire Sent, got {repeat:?}"
    );
}

#[test]
fn ephemeral_written_handoff_cannot_mint_persisted_sent_truth() {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://ephemeral-a.example").unwrap();
    let relay_b = RelayUrl::parse("wss://ephemeral-b.example").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &author);
    let sink = CapturingReceiptSink::default();
    let accepted = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&author, 93, "ephemeral handoff")),
            durability: Durability::Ephemeral,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([relay_a.clone(), relay_b.clone()]),
            }),
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(&author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    assert!(!sink
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|status| matches!(status, WriteStatus::Sent { .. })));
    let correlation_for = |relay: &RelayUrl| {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(found, event, correlation)
                    if found == &signer_session(relay, event.pubkey) =>
                {
                    Some(*correlation)
                }
                _ => None,
            })
            .unwrap()
    };

    assert!(core
        .handle(EngineMsg::EventHandoff(
            correlation_for(&relay_a),
            HandoffResult::NotHandedOff,
        ))
        .is_empty());
    let written = core.handle(EngineMsg::EventHandoff(
        correlation_for(&relay_b),
        HandoffResult::Written,
    ));
    assert!(written.is_empty());
    assert!(!sink
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|status| matches!(status, WriteStatus::Sent { .. })));
}

/// The exact handoff class is public receipt truth: `NotHandedOff` waits for
/// the relay without claiming an attempt is sent, while `Ambiguous` carries
/// the persisted ordinal/time and is never collapsed into `Sent`.
#[test]
fn not_handed_off_and_ambiguous_project_distinct_truth_without_sent() {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(
        author.public_key().to_hex(),
        [relay_a.clone(), relay_b.clone()],
    );
    let mut core = new_core(dir);
    let sink = CapturingReceiptSink::default();
    connect_signer(&mut core, 0, &relay_a, author.public_key());
    connect_signer(&mut core, 1, &relay_b, author.public_key());
    authenticate_signer(&mut core, 0, &relay_a, &author);
    authenticate_signer(&mut core, 1, &relay_b, &author);

    let (id, _signed, effects) = publish_private(
        &mut core,
        &author,
        [relay_a.clone(), relay_b.clone()],
        sink.clone(),
    );
    let correlation_for = |relay: &RelayUrl| {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::PublishEvent(r, event, c) if r == &signer_session(relay, event.pubkey) => {
                    Some(*c)
                }
                _ => None,
            })
            .expect("a PublishEvent effect must have been emitted for this relay")
    };

    let not_handed_off = core.handle(EngineMsg::EventHandoff(
        correlation_for(&relay_a),
        HandoffResult::NotHandedOff,
    ));
    assert!(not_handed_off.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(
            receipt,
            WriteStatus::AwaitingRelay { relay }
        ) if *receipt == id && relay == &relay_a
    )));
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10)));
    let ambiguous = core.handle(EngineMsg::EventHandoff(
        correlation_for(&relay_b),
        HandoffResult::Ambiguous,
    ));
    assert!(ambiguous.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(
            receipt,
            WriteStatus::HandoffAmbiguous {
                relay,
                attempt: 1,
                observed_at,
            }
        ) if *receipt == id && relay == &relay_b && *observed_at == Timestamp::from(10)
    )));
    assert!(
        !sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| matches!(s, WriteStatus::Sent { .. })),
        "neither NotHandedOff nor Ambiguous may ever surface as Sent"
    );
}

/// An `EventHandoff` for a correlation this reducer never minted (unknown,
/// or belonging to a different process entirely) is a structural no-op --
/// never a panic, never a stray effect.
#[test]
fn event_handoff_for_an_unknown_correlation_is_inert() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);
    let _ = publish_private(&mut core, &author, [relay], CapturingReceiptSink::default());

    let unknown = nmp_transport::AttemptCorrelation(u64::MAX);
    let effects = core.handle(EngineMsg::EventHandoff(unknown, HandoffResult::Written));
    assert!(effects.is_empty());
}

#[test]
fn all_attempt_start_failures_retain_every_lane_without_empty_terminal_sentinel() {
    let author = Keys::generate();
    let a = RelayUrl::parse("wss://blocked-a.example").unwrap();
    let b = RelayUrl::parse("wss://blocked-b.example").unwrap();
    let store = SharedFailStartStore::new([a.clone(), b.clone()]);
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    let sink = CapturingReceiptSink::default();
    connect_signer(&mut core, 0, &a, author.public_key());
    connect_signer(&mut core, 1, &b, author.public_key());
    authenticate_signer(&mut core, 0, &a, &author);
    authenticate_signer(&mut core, 1, &b, &author);

    let (id, _, effects) =
        publish_private(&mut core, &author, [a.clone(), b.clone()], sink.clone());
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(..)))
            .count(),
        0
    );
    let statuses = sink.0.lock().unwrap();
    assert!(statuses.contains(&WriteStatus::PersistenceBlocked(a.clone())));
    assert!(statuses.contains(&WriteStatus::PersistenceBlocked(b.clone())));
    drop(statuses);
    let replay = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(id, Box::new(replay.clone()))
        .is_attached());
    let replayed = replay.0.lock().unwrap();
    assert!(replayed.contains(&WriteStatus::PersistenceBlocked(a)));
    assert!(replayed.contains(&WriteStatus::PersistenceBlocked(b)));
}

#[test]
fn ack_of_persisted_lane_does_not_terminalize_mixed_blocked_obligation() {
    let author = Keys::generate();
    let good = RelayUrl::parse("wss://ack-persisted.example").unwrap();
    let blocked = RelayUrl::parse("wss://still-blocked.example").unwrap();
    let store = SharedFailStartStore::new([blocked.clone()]);
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&good, author.public_key()),
    ));
    connect_signer(&mut core, 1, &blocked, author.public_key());
    authenticate_signer(&mut core, 0, &good, &author);
    authenticate_signer(&mut core, 1, &blocked, &author);
    let (id, signed, scheduled) = publish_private(
        &mut core,
        &author,
        [good.clone(), blocked.clone()],
        CapturingReceiptSink::default(),
    );
    mark_written(&mut core, &scheduled, &good);
    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&good, signed.pubkey),
        RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
    ));
    assert!(acked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(receipt, WriteStatus::Acked(relay))
            if *receipt == id && relay == &good
    )));
    let replay = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(id, Box::new(replay.clone()))
        .is_attached());
    assert!(replay
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::PersistenceBlocked(blocked)));
}

#[test]
fn restart_rediscovers_unstarted_lane_and_persists_it_before_recovery_publish() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://recover-blocked.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("start-failure.redb");
    let receipt = {
        let mut first = EngineCore::new(
            RedbFailStartStore::open(&path, [relay.clone()]),
            Box::new(FixtureDirectory::new()),
            10,
        );
        connect_signer(&mut first, 0, &relay, author.public_key());
        authenticate_signer(&mut first, 0, &relay, &author);
        let (id, _, effects) = publish_private(
            &mut first,
            &author,
            [relay.clone()],
            CapturingReceiptSink::default(),
        );
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        id
    };

    let mut still_blocked = EngineCore::new(
        RedbFailStartStore::open(&path, [relay.clone()]),
        Box::new(FixtureDirectory::new()),
        10,
    );
    assert!(still_blocked
        .recover_on_boot()
        .iter()
        .any(|effect| matches!(effect, Effect::EnsureRelay(r)
            if r == &signer_session(&relay, author.public_key()))));
    connect_signer(&mut still_blocked, 0, &relay, author.public_key());
    authenticate_signer(&mut still_blocked, 0, &relay, &author);
    let replay = CapturingReceiptSink::default();
    assert!(still_blocked
        .reattach_receipt(receipt, Box::new(replay.clone()))
        .is_attached());
    assert!(replay
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::PersistenceBlocked(relay.clone())));
    drop(still_blocked);

    let mut recovered = EngineCore::new(
        RedbFailStartStore::open(&path, []),
        Box::new(FixtureDirectory::new()),
        10,
    );
    let boot = recovered.recover_on_boot();
    assert!(boot
        .iter()
        .any(|effect| matches!(effect, Effect::EnsureRelay(r)
            if r == &signer_session(&relay, author.public_key()))));
    connect_signer(&mut recovered, 0, &relay, author.public_key());
    let effects = release_author_probe(
        &mut recovered,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &relay,
        author.public_key(),
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
                if r == &signer_session(&relay, event.pubkey)))
            .count(),
        1
    );
    drop(recovered);
    let store = RedbStore::open(&path).expect("inspect recovered redb");
    let intent = store.recover_outbox()[0].intent_id;
    let attempts = store.recover_attempts(intent).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].relay, relay);
    assert_eq!(attempts[0].outcome, AttemptOutcome::Started);
}

#[test]
fn author_outbox_failed_attempt_survives_restart_with_empty_directory() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://durable-author-route.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("author-route.redb");
    let receipt = {
        let directory =
            FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(
            RedbFailStartStore::open(&path, [relay.clone()]),
            Box::new(directory),
            10,
        );
        connect_signer(&mut core, 0, &relay, author.public_key());
        authenticate_signer(&mut core, 0, &relay, &author);
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&author, 86, "dynamic author route")),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            },
            Box::new(CapturingReceiptSink::default()),
        ));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::PersistenceBlocked(r)) if r == &relay
        )));
        id
    };

    {
        let store = RedbStore::open(&path).unwrap();
        let intent = store.recover_outbox()[0].intent_id;
        let revisions = store.recover_route_revisions(intent).unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].relays, BTreeSet::from([relay.clone()]));
        assert!(store.recover_attempts(intent).unwrap().is_empty());
    }

    let mut recovered = EngineCore::new(
        RedbFailStartStore::open(&path, []),
        Box::new(FixtureDirectory::new()),
        10,
    );
    recovered.recover_on_boot();
    connect_signer(&mut recovered, 0, &relay, author.public_key());
    let effects = release_author_probe(
        &mut recovered,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &relay,
        author.public_key(),
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
                if r == &signer_session(&relay, event.pubkey)))
            .count(),
        1
    );
    assert!(recovered
        .reattach_receipt(receipt, Box::new(CapturingReceiptSink::default()))
        .is_attached());
}

#[test]
fn inbox_route_removal_cannot_erase_durable_lane_and_new_revision_failure_is_volatile() {
    let author = Keys::generate();
    let recipient = Keys::generate();
    let old = RelayUrl::parse("wss://old-inbox.example").unwrap();
    let new = RelayUrl::parse("wss://new-inbox.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("inbox-route.redb");
    let receipt = {
        let directory =
            FixtureDirectory::new().with_read(recipient.public_key().to_hex(), [old.clone()]);
        let mut core = EngineCore::new(
            RedbFailStartStore::open(&path, [old.clone()]),
            Box::new(directory),
            10,
        );
        connect_signer(&mut core, 0, &old, author.public_key());
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&author, 87, "dynamic inbox route")),
                durability: Durability::Durable,
                routing: WriteRouting::ToInboxes(vec![recipient.public_key()]),
                identity_override: None,
                correlation: None,
            },
            Box::new(CapturingReceiptSink::default()),
        ));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        id
    };

    // Directory removal/replacement cannot subtract `old`. Failure to append
    // the newly resolved `new` revision blocks only that volatile lane; the
    // already-durable old obligation may still start and publish.
    {
        let changed =
            FixtureDirectory::new().with_read(recipient.public_key().to_hex(), [new.clone()]);
        let mut core = EngineCore::new(
            RedbFailStartStore::open_with_route_failure(&path),
            Box::new(changed),
            10,
        );
        core.recover_on_boot();
        connect_signer(&mut core, 0, &old, author.public_key());
        let effects = release_author_probe(
            &mut core,
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            &old,
            author.public_key(),
        );
        let old_event = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(session, event, _)
                    if session == &signer_session(&old, event.pubkey) =>
                {
                    Some(event.clone())
                }
                _ => None,
            })
            .expect("durable old lane publishes");
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
                if r == &signer_session(&old, event.pubkey))));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
                if r == &signer_session(&new, event.pubkey))));
        mark_written(&mut core, &effects, &old);
        let acked = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            signer_session(&old, old_event.pubkey),
            RelayFrame::from(RelayMessage::ok(old_event.id, true, "")),
        ));
        assert!(acked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::Acked(r)) if r == &old
        )));
        let replay = CapturingReceiptSink::default();
        assert!(core
            .reattach_receipt(receipt, Box::new(replay.clone()))
            .is_attached());
        assert!(replay
            .0
            .lock()
            .unwrap()
            .contains(&WriteStatus::RoutePersistenceBlocked(new.clone())));
    }

    {
        let store = RedbStore::open(&path).unwrap();
        let intent = store.recover_outbox()[0].intent_id;
        let durable = store
            .recover_route_revisions(intent)
            .unwrap()
            .into_iter()
            .flat_map(|revision| revision.relays)
            .collect::<BTreeSet<_>>();
        assert_eq!(durable, BTreeSet::from([old.clone()]));
    }

    // Once a later boot can persist the changed revision, `new` starts. The
    // old lane is retained in route history but is already terminal (Acked),
    // so it is correctly not published again.
    let changed = FixtureDirectory::new().with_read(recipient.public_key().to_hex(), [new.clone()]);
    let mut core = EngineCore::new(RedbFailStartStore::open(&path, []), Box::new(changed), 10);
    core.recover_on_boot();
    connect_signer(&mut core, 0, &new, author.public_key());
    let effects = release_author_probe(
        &mut core,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &new,
        author.public_key(),
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
            if r == &signer_session(&old, event.pubkey))));
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
            if r == &signer_session(&new, event.pubkey))));
}

#[test]
fn route_revision_failure_emits_no_attempt_or_wire_and_claims_no_crash_durable_url() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://volatile-route.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("route-failure.redb");
    {
        let directory =
            FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(
            RedbFailStartStore::open_with_route_failure(&path),
            Box::new(directory),
            10,
        );
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&author, 88, "volatile route")),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            },
            Box::new(CapturingReceiptSink::default()),
        ));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::RoutePersistenceBlocked(r)) if r == &relay
        )));
    }
    let store = RedbStore::open(&path).unwrap();
    let intent = store.recover_outbox()[0].intent_id;
    assert!(store.recover_route_revisions(intent).unwrap().is_empty());
    assert!(store.recover_attempts(intent).unwrap().is_empty());
    drop(store);

    let mut recovered = EngineCore::new(
        RedbFailStartStore::open(&path, []),
        Box::new(FixtureDirectory::new()),
        10,
    );
    assert!(recovered.recover_on_boot().is_empty());
}

#[test]
fn write_ack_per_relay() {
    let a = Keys::generate();
    let relay_ok = RelayUrl::parse("wss://relay-ok.example.com").unwrap();
    let relay_bad = RelayUrl::parse("wss://relay-bad.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(
        a.public_key().to_hex(),
        [relay_ok.clone(), relay_bad.clone()],
    );
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &relay_ok, a.public_key());
    connect_signer(&mut core, 1, &relay_bad, a.public_key());
    authenticate_signer(&mut core, 0, &relay_ok, &a);
    authenticate_signer(&mut core, 1, &relay_bad, &a);

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "durable ack test")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::PublishEvent(..)))
            .count(),
        2,
        "a durable AuthorOutbox write reaches both of the author's write relays"
    );
    mark_written(&mut core, &effects, &relay_ok);
    mark_written(&mut core, &effects, &relay_bad);

    let ok_frame = RelayFrame::from(RelayMessage::ok(signed.id, true, ""));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay_ok, signed.pubkey),
        ok_frame,
    ));
    assert!(effects.iter().any(
        |e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::Acked(r)) if *rid == id && r == &relay_ok)
    ));

    let nack_frame = RelayFrame::from(RelayMessage::ok(signed.id, false, "blocked: spam"));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        signer_session(&relay_bad, signed.pubkey),
        nack_frame,
    ));
    assert!(effects.iter().any(
        |e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::Rejected(r, msg)) if *rid == id && r == &relay_bad && msg.contains("blocked"))
    ));

    let statuses = sink.0.lock().unwrap();
    assert!(statuses
        .iter()
        .any(|s| matches!(s, WriteStatus::Acked(r) if r == &relay_ok)));
    assert!(statuses
        .iter()
        .any(|s| matches!(s, WriteStatus::Rejected(r, _) if r == &relay_bad)));
}

#[test]
fn uncommitted_attempt_terminal_emits_no_receipt_and_keeps_lane_live() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://finish-failure.example").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = EngineCore::new(
        FailOnceCompensationStore::failing_attempt_finish(),
        Box::new(dir),
        10,
    );
    activate(&mut core, &a);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, a.public_key()),
    ));
    authenticate_signer(&mut core, 0, &relay, &a);
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 2, "finish persistence")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (id, generation, unsigned) = find_sign_request(&effects);
    let signed = unsigned.sign_with_keys(&a).unwrap();
    let scheduled = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    mark_written(&mut core, &scheduled, &relay);
    let frame = || RelayFrame::from(RelayMessage::ok(signed.id, true, ""));
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, signed.pubkey),
        frame(),
    ));
    assert!(!failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitReceipt(_, WriteStatus::Acked(_)))));
    let retried = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, signed.pubkey),
        frame(),
    ));
    assert!(retried.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(receipt, WriteStatus::Acked(r)) if *receipt == id && r == &relay)
    ));
}

#[test]
fn unaccepted_failure_ids_are_distinct_and_disjoint_from_store_receipts() {
    let a = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    let fail = |core: &mut EngineCore<MemoryStore>, seq| {
        core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&a, seq, "unaccepted")),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            },
            Box::new(CapturingReceiptSink::default()),
        ))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(id, WriteStatus::Failed(_)) => Some(id),
            _ => None,
        })
        .unwrap()
    };
    let first = fail(&mut core, 200);
    let second = fail(&mut core, 201);
    assert_ne!(first, second);
    assert!(first.0 >= (1u64 << 63));
    assert!(second.0 >= (1u64 << 63));
}
