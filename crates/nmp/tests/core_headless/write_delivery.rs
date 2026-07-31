use super::*;

// ---- durable write delivery and recovery -------------------------------

/// An explicit route naming no relays is refused AT THE DOOR: "reject it
/// immediately". `Failed` is the first and only fact -- never `Accepted`,
/// never a sign request, never a journal row, and never a quiet degradation
/// into `Auto`.
///
/// This is deliberately stricter than the route it replaces. That route
/// accepted an empty set and failed closed later, at resolution, so
/// emptiness was a sentence an app read off a receipt. The sentence now
/// lives where it can explain itself, and the empty route stops being
/// acceptable at all.
#[test]
fn an_explicit_route_with_no_relays_is_refused_before_acceptance() {
    let a = Keys::generate();
    // The directory is empty on purpose: there is no write relay anywhere
    // for a refusal-turned-fallback to leak into, so a passing assertion
    // here is about the door, not about luck.
    let dir = FixtureRoutingFacts::new();
    let mut core = new_core(dir);
    activate(&mut core, &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "nowhere")),
        durability: Durability::Durable,
        routing: WriteRouting::Explicit(Vec::new()),
        identity: Identity::Active,
        correlation: None,
    }));

    let receipts: Vec<&WriteStatus> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::EmitReceipt(_, status) => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(receipts.len(), 1, "exactly one fact: {receipts:?}");
    assert!(
        matches!(receipts.first(), Some(WriteStatus::Failed(_))),
        "Failed must be the first and only fact -- never Accepted: {receipts:?}"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::RequestSign(..))),
        "no signer is asked for anything on a refused publish"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..) | Effect::EnsureWriteRelay(..))),
        "no relay is contacted, and no lane is opened, for a refused publish"
    );
}

/// The sibling of the refusal above, and the reason it is safe to be this
/// strict: emptiness is a property of the REQUEST, knowable at the door,
/// while reachability is a property of the world and is not. A write aimed
/// at a relay nobody can reach is accepted and routed verbatim to exactly
/// that relay -- it fails per relay afterwards, never at the door.
#[test]
fn an_unreachable_explicit_relay_is_accepted_because_the_door_cannot_know() {
    let a = Keys::generate();
    let nowhere = RelayUrl::parse("wss://non-existent.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "hello")),
        durability: Durability::Durable,
        routing: WriteRouting::Explicit(vec![nowhere.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(_, WriteStatus::Accepted))),
        "acceptance cannot validate that a relay exists, so it does not try"
    );

    // Routing happened and named exactly the caller's relay: the lane the
    // engine opens is for that session and no other. Nothing reaches the
    // wire, because nothing can -- which is the point. Failing per relay is
    // what an unreachable target gets, not a refusal at the door.
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    let opened: Vec<RelaySessionKey> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::EnsureWriteRelay(session) => Some(session.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        opened,
        vec![signer_session(&nowhere, a.public_key())],
        "the write is routed verbatim to the relay the caller named"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(_, WriteStatus::Failed(_)))),
        "an unreachable relay is a per-relay outcome, never a whole-intent refusal"
    );
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
    let mut core = EngineCore::new(store, 10);
    connect_signer(&mut core, 0, &good, author.public_key());
    connect_signer(&mut core, 1, &blocked, author.public_key());
    authenticate_signer(&mut core, 0, &good, &author);
    authenticate_signer(&mut core, 1, &blocked, &author);

    let (id, _, effects) = publish_explicit(&mut core, &author, [good.clone(), blocked.clone()]);
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
    let replay = core.reattach_receipt(id);
    assert!(replay.is_attached());
    assert!(replay
        .facts
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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
    let mut core = new_core(dir);
    connect_signer(&mut core, 0, &relay, author.public_key());
    authenticate_signer(&mut core, 0, &relay, &author);

    let (id, _signed, effects) = publish_explicit(&mut core, &author, [relay.clone()]);

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(_, WriteStatus::Sent { .. }))),
        "Sent must never fire synchronously at enqueue time, got {effects:?}"
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

    let reattached = core.reattach_receipt(id);
    assert!(reattached.is_attached());
    assert!(
        !reattached
            .facts
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
    assert!(core
        .reattach_receipt(id)
        .facts
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
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &author);
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(93, "ephemeral handoff")),
        durability: Durability::Ephemeral,
        routing: WriteRouting::Explicit(vec![relay_a.clone(), relay_b.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(&author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::EmitReceipt(_, WriteStatus::Sent { .. }))));
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
}

/// The exact handoff class is public receipt truth: `NotHandedOff` waits for
/// the relay without claiming an attempt is sent, while `Ambiguous` carries
/// the persisted ordinal/time and is never collapsed into `Sent`.
#[test]
fn not_handed_off_and_ambiguous_project_distinct_truth_without_sent() {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(author.public_key(), [relay_a.clone(), relay_b.clone()]);
    let mut core = new_core(dir);
    connect_signer(&mut core, 0, &relay_a, author.public_key());
    connect_signer(&mut core, 1, &relay_b, author.public_key());
    authenticate_signer(&mut core, 0, &relay_a, &author);
    authenticate_signer(&mut core, 1, &relay_b, &author);

    let (id, _signed, effects) =
        publish_explicit(&mut core, &author, [relay_a.clone(), relay_b.clone()]);
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
        !not_handed_off
            .iter()
            .chain(&ambiguous)
            .any(|effect| matches!(effect, Effect::EmitReceipt(_, WriteStatus::Sent { .. }))),
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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
    let mut core = new_core(dir);
    let _ = publish_explicit(&mut core, &author, [relay]);

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
    let mut core = EngineCore::new(store, 10);
    connect_signer(&mut core, 0, &a, author.public_key());
    connect_signer(&mut core, 1, &b, author.public_key());
    authenticate_signer(&mut core, 0, &a, &author);
    authenticate_signer(&mut core, 1, &b, &author);

    let (id, _, effects) = publish_explicit(&mut core, &author, [a.clone(), b.clone()]);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(..)))
            .count(),
        0
    );
    let statuses = receipt_statuses(&effects);
    assert!(statuses.contains(&WriteStatus::PersistenceBlocked(a.clone())));
    assert!(statuses.contains(&WriteStatus::PersistenceBlocked(b.clone())));
    let replay = core.reattach_receipt(id);
    assert!(replay.is_attached());
    let replayed = replay.facts;
    assert!(replayed.contains(&WriteStatus::PersistenceBlocked(a)));
    assert!(replayed.contains(&WriteStatus::PersistenceBlocked(b)));
}

#[test]
fn ack_of_persisted_lane_does_not_terminalize_mixed_blocked_obligation() {
    let author = Keys::generate();
    let good = RelayUrl::parse("wss://ack-persisted.example").unwrap();
    let blocked = RelayUrl::parse("wss://still-blocked.example").unwrap();
    let store = SharedFailStartStore::new([blocked.clone()]);
    let mut core = EngineCore::new(store, 10);
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
    let (id, signed, scheduled) =
        publish_explicit(&mut core, &author, [good.clone(), blocked.clone()]);
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
    let replay = core.reattach_receipt(id);
    assert!(replay.is_attached());
    assert!(replay
        .facts
        .contains(&WriteStatus::PersistenceBlocked(blocked)));
}

#[test]
fn restart_rediscovers_unstarted_lane_and_persists_it_before_recovery_publish() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://recover-blocked.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("start-failure.redb");
    let receipt = {
        let mut first = EngineCore::new(RedbFailStartStore::open(&path, [relay.clone()]), 10);
        connect_signer(&mut first, 0, &relay, author.public_key());
        authenticate_signer(&mut first, 0, &relay, &author);
        let (id, _, effects) = publish_explicit(&mut first, &author, [relay.clone()]);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        id
    };

    let mut still_blocked = EngineCore::new(RedbFailStartStore::open(&path, [relay.clone()]), 10);
    assert!(still_blocked
        .recover_on_boot()
        .iter()
        .any(|effect| matches!(effect, Effect::EnsureWriteRelay(r)
            if r == &signer_session(&relay, author.public_key()))));
    connect_signer(&mut still_blocked, 0, &relay, author.public_key());
    authenticate_signer(&mut still_blocked, 0, &relay, &author);
    let replay = still_blocked.reattach_receipt(receipt);
    assert!(replay.is_attached());
    assert!(replay
        .facts
        .contains(&WriteStatus::PersistenceBlocked(relay.clone())));
    drop(still_blocked);

    let mut recovered = EngineCore::new(RedbFailStartStore::open(&path, []), 10);
    let boot = recovered.recover_on_boot();
    assert!(boot
        .iter()
        .any(|effect| matches!(effect, Effect::EnsureWriteRelay(r)
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
    let intent = store.recover_delivery().expect("recover delivery")[0].intent_id;
    let attempts = store.recover_attempts(intent).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].relay, relay);
    assert_eq!(attempts[0].outcome, DeliveryAttemptOutcome::Started);
}

#[test]
fn author_outbox_failed_attempt_survives_restart_with_empty_directory() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://durable-author-route.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("author-route.redb");
    let receipt = {
        let directory =
            FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbFailStartStore::open(&path, [relay.clone()]),
            directory,
            10,
        );
        connect_signer(&mut core, 0, &relay, author.public_key());
        authenticate_signer(&mut core, 0, &relay, &author);
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(86, "dynamic author route")),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
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
        let intent = store.recover_delivery().expect("recover delivery")[0].intent_id;
        let revisions = store.recover_route_revisions(intent).unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].relays, BTreeSet::from([relay.clone()]));
        assert!(store.recover_attempts(intent).unwrap().is_empty());
    }

    let mut recovered = EngineCore::new(RedbFailStartStore::open(&path, []), 10);
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
    assert!(recovered.reattach_receipt(receipt).is_attached());
}

#[test]
fn author_route_removal_cannot_erase_durable_lane_and_new_revision_failure_is_volatile() {
    let author = Keys::generate();
    let old = RelayUrl::parse("wss://old-outbox.example").unwrap();
    let new = RelayUrl::parse("wss://new-outbox.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("author-route.redb");
    let receipt = {
        let directory =
            FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [old.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbFailStartStore::open(&path, [old.clone()]),
            directory,
            10,
        );
        connect_signer(&mut core, 0, &old, author.public_key());
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(87, "dynamic author route")),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
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
            FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [new.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbFailStartStore::open_with_route_failure(&path),
            changed,
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
        let replay = core.reattach_receipt(receipt);
        assert!(replay.is_attached());
        assert!(replay
            .facts
            .contains(&WriteStatus::RoutePersistenceBlocked(new.clone())));
    }

    {
        let store = RedbStore::open(&path).unwrap();
        let intent = store.recover_delivery().expect("recover delivery")[0].intent_id;
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
    let changed =
        FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [new.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(
        RedbFailStartStore::open(&path, []),
        changed,
        10,
    );
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
            FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbFailStartStore::open_with_route_failure(&path),
            directory,
            10,
        );
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(88, "volatile route")),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
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
    let intent = store.recover_delivery().expect("recover delivery")[0].intent_id;
    assert!(store.recover_route_revisions(intent).unwrap().is_empty());
    assert!(store.recover_attempts(intent).unwrap().is_empty());
    drop(store);

    let mut recovered = EngineCore::new(RedbFailStartStore::open(&path, []), 10);
    let effects = recovered.recover_on_boot();
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::AuthorRouteNeedsChanged(needs)]
                if needs == &BTreeSet::from([author.public_key()])
        ),
        "the failed volatile route must produce exactly its one provider-need effect: {effects:?}"
    );
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::PublishEvent(..) | Effect::EnsureWriteRelay(..)
        )),
        "redeclaring discovery need cannot claim a crash-durable route, attempt, or wire send"
    );
}

#[test]
fn write_ack_per_relay() {
    let a = Keys::generate();
    let relay_ok = RelayUrl::parse("wss://relay-ok.example.com").unwrap();
    let relay_bad = RelayUrl::parse("wss://relay-bad.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay_ok.clone(), relay_bad.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &relay_ok, a.public_key());
    connect_signer(&mut core, 1, &relay_bad, a.public_key());
    authenticate_signer(&mut core, 0, &relay_ok, &a);
    authenticate_signer(&mut core, 1, &relay_bad, &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "durable ack test")),
        durability: Durability::Durable,
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
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
        "a durable Auto-routed write reaches both of the author's write relays"
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

    let statuses = core.reattach_receipt(id).facts;
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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(
        FailOnceCompensationStore::failing_attempt_finish(),
        dir,
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
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(2, "finish persistence")),
        durability: Durability::Durable,
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
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
    let mut core = new_core(FixtureRoutingFacts::new());
    let fail = |core: &mut EngineCore<MemoryStore>, seq| {
        core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(seq, "unaccepted")),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }))
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
