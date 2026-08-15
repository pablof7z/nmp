use super::*;
use nmp_store::testing;

// ---- fallible persistence doors and recovery indexing ------------------

pub(super) fn recover_after_observation_io(core: &mut EngineCore) -> Vec<Effect> {
    let (fault, effects) = core
        .recover_requested_redb_store_for_test()
        .expect("the same Redb target reconstructs")
        .expect("observation I/O must request reconstruction");
    assert_eq!(fault, PersistenceFault::Io);
    assert!(matches!(
        effects.last(),
        Some(Effect::EmitDiagnostics(snapshot)) if snapshot.store_degraded.is_none()
    ));
    assert!(
        effects.iter().all(|effect| matches!(
            effect,
            Effect::EmitDiagnostics(_)
                | Effect::DiagnosticsChanged
                | Effect::EmitRows(..)
                | Effect::Wire(_)
        )),
        "unexpected reconstruction effect: {effects:?}"
    );
    effects
}

/// Door-level falsifier (issue #122): the real Redb `insert` transaction
/// surfaces a persistence I/O failure as `Err(PersistenceError)` rather than
/// panicking, closes that failed generation, and reconstructs the same target.
///
/// It also pins #895's classification across the crate boundary: the fault
/// and its durability outcome reach `nmp` as types, so a consumer
/// never has to read the message to learn whether the write may have landed.
#[test]
fn ingest_door_surfaces_io_failure_as_persistence_error_not_panic() {
    let a = Keys::generate();
    let mut store = RedbStore::temporary_with_observation_precommit_io()
        .expect("temporary Redb observation-I/O fixture");
    let event = nmp_resolver_testkit::kind1(&a, "disk is full", 1_000);
    let event_id = event.id;
    let from = RelayObserved::new(
        RelayUrl::parse("wss://relay.example.com").unwrap(),
        Timestamp::from(1_000u64),
    );
    let outcome = store.insert(event, from);
    let error = outcome.expect_err("an ingest-path I/O failure must surface as Err");
    assert_eq!(error.fault(), PersistenceFault::Io);
    assert_eq!(
        error.durability(),
        DurabilityOutcome::Unknown,
        "an I/O failure never claims the write is absent"
    );
    assert!(error.fault().requires_reopen());
    let latched = store
        .query(&nostr::Filter::new().id(event_id))
        .expect_err("the failed Redb generation must stay closed");
    assert_eq!(latched.fault(), PersistenceFault::Latched);
    store
        .reopen_after_failure()
        .expect("the same temporary Redb target must reconstruct");
    assert!(store
        .query(&nostr::Filter::new().id(event_id))
        .unwrap()
        .is_empty());
}

/// Engine-level falsifier (issue #122): a relay EVENT frame whose store
/// `insert` fails on I/O DEGRADES the engine to read-only (a `store_degraded`
/// diagnostic is emitted) and never panics the reducer. The failed frame
/// delivers no phantom rows, and the engine stays usable for later messages.
#[test]
fn ingest_io_failure_degrades_read_only_without_panicking() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay.clone()]);
    let store = RedbStore::temporary_with_observation_precommit_io()
        .expect("temporary Redb observation-I/O fixture");
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);

    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let _ = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
    ));

    // The real relay ingest path — the exact call that used to `.expect()`
    // panic on a disk-full redb `insert`.
    let event = nmp_resolver_testkit::kind1(&a, "disk is full", 1_000);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame("s", event),
    ));

    // Degrade, don't panic: the read-only signal reaches the diagnostics
    // surface.
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitDiagnostics(snap) if snap.store_degraded.is_some())),
        "an ingest I/O failure must surface a `store_degraded` diagnostic, got {effects:?}"
    );
    // A failed ingest fabricates no rows.
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "a failed ingest must not deliver phantom rows, got {effects:?}"
    );
    let _ = recover_after_observation_io(&mut core);
    // The reducer survives reconstruction and keeps handling messages.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(1u64)));
}

/// #816's ordinary-REQ failure falsifier: an EVENT that fails its durable
/// commit poisons only the exact request revision named by that frame. Its
/// later EOSE therefore cannot manufacture a coverage fact for data the
/// store never accepted.
#[test]
fn failed_event_commit_prevents_its_exact_request_from_recording_coverage() {
    let author = Keys::generate();
    let healthy_author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let healthy_relay = RelayUrl::parse("wss://healthy-relay.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(author.public_key(), [relay.clone()])
        .with_outbound_routes(healthy_author.public_key(), [healthy_relay.clone()]);
    let store = RedbStore::temporary_with_observation_precommit_io()
        .expect("temporary Redb observation-I/O fixture");
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);

    let _ = connect(&mut core, 0, &relay);
    let _ = connect(&mut core, 1, &healthy_relay);
    let failed_subscribed = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &author.public_key().to_hex(),
    )));
    let (request, request_filter) = {
        let (sub_id, filter) = req_for_kind(&failed_subscribed, &relay, 1);
        (sub_id.clone(), filter.clone())
    };
    let healthy_subscribed = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[2],
        &healthy_author.public_key().to_hex(),
    )));
    let (healthy_request, healthy_filter) = {
        let (sub_id, filter) = req_for_kind(&healthy_subscribed, &healthy_relay, 2);
        (sub_id.clone(), filter.clone())
    };
    let failed_attempt = failed_subscribed
        .iter()
        .find_map(|effect| match effect {
            Effect::Wire(delta) => {
                Some(delta.attempt_id(&public_session(&relay), &request, &request_filter))
            }
            _ => None,
        })
        .expect("the failed request carries its exact attempt identity");
    let healthy_attempt = healthy_subscribed
        .iter()
        .find_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta.attempt_id(
                &public_session(&healthy_relay),
                &healthy_request,
                &healthy_filter,
            )),
            _ => None,
        })
        .expect("the healthy request carries its exact attempt identity");
    let failed_request_accepted = core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: failed_attempt,
        handle: RelayHandle {
            slot: 0,
            generation: 1,
        },
    });
    let healthy_request_accepted = core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: healthy_attempt,
        handle: RelayHandle {
            slot: 1,
            generation: 1,
        },
    });
    assert!(
        failed_request_accepted
            .iter()
            .any(|effect| matches!(effect, Effect::EmitObservationEvidence(..))),
        "the failed request must cross the same accepted handoff edge as runtime: {failed_request_accepted:?}"
    );
    assert!(
        healthy_request_accepted
            .iter()
            .any(|effect| matches!(effect, Effect::EmitObservationEvidence(..))),
        "the healthy request must cross the same accepted handoff edge as runtime: {healthy_request_accepted:?}"
    );
    let wire = wire_sub_string(&request);
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));

    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(
            &wire,
            nmp_resolver_testkit::kind1(&author, "must not earn coverage", 100),
        ),
    ));
    assert!(failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot)
            if snapshot.store_degraded.is_some())));
    let _ = recover_after_observation_io(&mut core);

    let completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire),
    ));
    assert!(
        !completed.iter().any(|effect| match effect {
            Effect::EmitObservationEvidence(_, evidence) => evidence
                .iter()
                .any(|item| { matches!(item.fact, ObservationFact::RequestSettled { .. }) }),
            _ => false,
        }),
        "a failed local EVENT commit must not become protocol absence evidence: {completed:?}"
    );
    let atom = ctx_atom(cf(&[1], &[&author.public_key().to_hex()]));
    assert_eq!(
        core.get_coverage(&atom, &relay).expect("coverage peek"),
        None
    );

    let healthy_wire = wire_sub_string(&healthy_request);
    let healthy_event = nostr::EventBuilder::new(Kind::Custom(2), "healthy")
        .custom_created_at(Timestamp::from(101u64))
        .sign_with_keys(&healthy_author)
        .expect("fixture signing");
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&healthy_relay),
        event_frame(&healthy_wire, healthy_event),
    ));
    let healthy_completed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&healthy_relay),
        eose_frame(&healthy_wire),
    ));
    assert!(
        healthy_completed.iter().any(|effect| match effect {
            Effect::EmitObservationEvidence(_, evidence) => evidence.iter().any(|item| {
                matches!(
                    item.fact,
                    ObservationFact::RequestSettled {
                        terminal: RequestTerminal::Eose,
                        ..
                    }
                )
            }),
            _ => false,
        }),
        "a healthy persisted completion must expose its EOSE settlement: {healthy_completed:#?}"
    );
    let healthy_atom = ctx_atom(cf(&[2], &[&healthy_author.public_key().to_hex()]));
    assert!(core
        .get_coverage(&healthy_atom, &healthy_relay)
        .expect("coverage peek")
        .is_some());
}

/// The event-failure key is the full physical relay session, not the relay
/// URL. Two identical pinned selections therefore keep independent coverage
/// authority when one runs on Public and the other on `Nip42(author)`.
#[test]
fn failed_event_commit_isolated_by_access_context_on_the_same_relay() {
    let public_author = Keys::generate();
    let protected_author = Keys::generate();
    let relay = RelayUrl::parse("wss://access-isolation.example.com").unwrap();
    let source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    let selection = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        ..Filter::default()
    };
    let public_query = LiveQuery::single(
        nmp_grammar::Demand::new(selection.clone(), source.clone(), AccessContext::Public)
            .expect("public pinned demand"),
    );
    let protected_query = LiveQuery::single(
        nmp_grammar::Demand::new(
            selection,
            source.clone(),
            AccessContext::Nip42(protected_author.public_key()),
        )
        .expect("protected pinned demand"),
    );
    let store = RedbStore::temporary_with_observation_precommit_io()
        .expect("temporary Redb observation-I/O fixture");
    let mut core =
        EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10);

    let _ = core.handle_and_flush(EngineMsg::Subscribe(public_query));
    let _ = core.handle_and_flush(EngineMsg::Subscribe(protected_query));

    let public_connected = connect(&mut core, 0, &relay);
    let public_request = public_connected
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&relay) => {
                requests.first().map(|request| request.sub_id.clone())
            }
            _ => None,
        })
        .expect("public session replays its pinned request");

    let protected_session = signer_session(&relay, protected_author.public_key());
    let protected_connected = connect_signer(&mut core, 1, &relay, protected_author.public_key());
    assert_no_protected_req(&protected_connected, &protected_session);
    let protected_ready = authenticate_signer(&mut core, 1, &relay, &protected_author);
    let protected_request = protected_ready
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &protected_session => {
                requests.first().map(|request| request.sub_id.clone())
            }
            _ => None,
        })
        .expect("authenticated session replays its identical pinned request");

    let public_filter = public_connected
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&relay) => {
                requests.first().map(|request| request.filter.clone())
            }
            _ => None,
        })
        .expect("public replay carries its filter");
    let protected_filter = protected_ready
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &protected_session => {
                requests.first().map(|request| request.filter.clone())
            }
            _ => None,
        })
        .expect("protected replay carries its filter");
    assert_eq!(
        public_filter, protected_filter,
        "selection and source are identical; access context is the only partition"
    );

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(
            &wire_sub_string(&public_request),
            nmp_resolver_testkit::kind1(&public_author, "the public transaction fails", 100),
        ),
    ));
    assert!(failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot)
            if snapshot.store_degraded.is_some())));
    let _ = recover_after_observation_io(&mut core);

    let protected_event =
        nmp_resolver_testkit::kind1(&protected_author, "the protected transaction commits", 101);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        protected_session.clone(),
        event_frame(&wire_sub_string(&protected_request), protected_event),
    ));

    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&public_request)),
    ));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        protected_session,
        eose_frame(&wire_sub_string(&protected_request)),
    ));

    let concrete = ConcreteFilter {
        kinds: Some(BTreeSet::from([1u16])),
        ..ConcreteFilter::default()
    };
    let public_atom = ContextualAtom {
        filter: concrete.clone(),
        source: source.clone(),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let protected_atom = ContextualAtom {
        filter: concrete,
        source,
        access: AccessContext::Nip42(protected_author.public_key()),
        routing_evidence: BTreeSet::new(),
    };
    assert_eq!(
        core.get_coverage(&public_atom, &relay)
            .expect("coverage peek"),
        None
    );
    assert!(core
        .get_coverage(&protected_atom, &relay)
        .expect("coverage peek")
        .is_some());
}

/// A failed EVENT commit poisons only the immutable request that delivered
/// it. Real store reconstruction retires that request; its stale EOSE earns
/// nothing, while the fresh successor can still earn coverage.
#[test]
fn failed_event_commit_poisons_only_its_immutable_request() {
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();
    let relay = RelayUrl::parse("wss://fifo-isolation.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay.clone()])
        .with_outbound_routes(b.public_key(), [relay.clone()])
        .with_outbound_routes(c.public_key(), [relay.clone()]);
    let store = RedbStore::temporary_with_observation_precommit_io()
        .expect("temporary Redb observation-I/O fixture");
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);
    connect(&mut core, 0, &relay);

    let first = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let first_sub = req_for(&first, &relay).0.clone();
    let second = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let second_sub = req_for(&second, &relay).0.clone();
    assert_ne!(first_sub, second_sub, "sent requests are immutable");
    let first_wire = wire_sub_string(&first_sub);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(
            &first_wire,
            nmp_resolver_testkit::kind1(&a, "failed immutable request", 100),
        ),
    ));
    assert!(failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot)
            if snapshot.store_degraded.is_some())));
    let recovery = recover_after_observation_io(&mut core);
    let recovered_sub = req_for(&recovery, &relay).0.clone();
    assert_ne!(first_sub, recovered_sub);
    assert_ne!(second_sub, recovered_sub);

    let third = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &c.public_key().to_hex(),
    )));
    let third_sub = req_for(&third, &relay).0.clone();
    assert_ne!(first_sub, third_sub);
    assert_ne!(second_sub, third_sub);
    assert_ne!(recovered_sub, third_sub);

    let atom_a = ctx_atom(cf(&[1], &[&a.public_key().to_hex()]));
    let atom_b = ctx_atom(cf(&[1], &[&b.public_key().to_hex()]));
    let atom_c = ctx_atom(cf(&[1], &[&c.public_key().to_hex()]));
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(600u64)));
    let stale_first = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&first_wire),
    ));
    assert!(
        !stale_first.iter().any(|effect| match effect {
            Effect::EmitObservationEvidence(_, evidence) => evidence
                .iter()
                .any(|item| matches!(item.fact, ObservationFact::RequestSettled { .. })),
            _ => false,
        }),
        "the retired failed request must not settle: {stale_first:?}"
    );
    for atom in [&atom_a, &atom_b, &atom_c] {
        assert_eq!(
            core.get_coverage(atom, &relay).expect("coverage peek"),
            None,
            "stale EOSE must not mint coverage after reconstruction"
        );
    }

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(800u64)));
    let current = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire_sub_string(&third_sub)),
    ));
    assert!(
        current
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(..))),
        "only the fresh successor may advance acquisition evidence: {current:?}"
    );
    for atom in [&atom_a, &atom_b, &atom_c] {
        assert!(
            core.get_coverage(atom, &relay)
                .expect("coverage peek")
                .is_some(),
            "the fresh successor must retain coverage authority"
        );
    }
}

/// A projection read can fail only after the EVENT transaction has committed.
/// That phase still degrades diagnostics, but it must not revoke the exact
/// request's coverage authority: doing so would confuse a failed local view
/// refresh with a missing durable fact.
#[test]
fn post_commit_projection_failure_does_not_poison_request_coverage() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://projection-failure.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
    let directory = tempfile::tempdir().expect("projection corruption directory");
    let path = directory
        .path()
        .join("postcommit-projection-corruption.redb");
    let corrupt_older = nmp_resolver_testkit::kind1(&author, "corrupt older row", 100);
    let healthy_newer = nmp_resolver_testkit::kind1(&author, "healthy newest row", 200);
    {
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        for event in [corrupt_older.clone(), healthy_newer.clone()] {
            store
                .insert(
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(201u64)),
                )
                .expect("seed projection source row");
        }
    }
    testing::corrupt_canonical_event(&path, corrupt_older.id)
        .expect("store-owned canonical-event corruption");
    let store = RedbStore::open(&path).expect("reopen persistent Redb fixture");
    let top = store
        .query_newest(
            &nostr::Filter::new()
                .kind(Kind::TextNote)
                .author(author.public_key()),
            1,
        )
        .expect("the top-1 store read stops before the corrupt older row");
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].event.id, healthy_newer.id);
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);
    let _ = core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    let mut derived_filter = Filter {
        kinds: Some(BTreeSet::from([7u16])),
        ..Filter::default()
    };
    derived_filter.tags.insert(
        nmp_grammar::IndexedTagName::new('p').expect("indexed p tag"),
        Binding::Derived(Box::new(nmp_grammar::Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([1u16])),
                authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                limit: Some(1),
                ..Filter::default()
            }),
            project: nmp_grammar::Selector::Authors,
        })),
    );
    let derived_from_latest_note = LiveQuery::from_filter(derived_filter);
    let initial = core.handle_and_flush(EngineMsg::Subscribe(derived_from_latest_note));
    assert!(
        initial.iter().all(|effect| !matches!(
            effect,
            Effect::EmitDiagnostics(snapshot) if snapshot.store_degraded.is_some()
        )),
        "the initial top-1 projection must select only the healthy newest row: {initial:?}"
    );
    let kind5 = LiveQuery::single(
        nmp_grammar::Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([5u16])),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            AccessContext::Public,
        )
        .expect("a literal kind:5 request can be pinned to the fixture relay"),
    );
    let kind5_open = core.handle_and_flush(EngineMsg::Subscribe(kind5));
    assert!(
        kind5_open.iter().all(|effect| !matches!(
            effect,
            Effect::EmitDiagnostics(snapshot) if snapshot.store_degraded.is_some()
        )),
        "the independent kind:5 request must open before the post-commit failure: {kind5_open:?}"
    );
    let connected = connect(&mut core, 0, &relay);
    let request = connected
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&relay) => requests
                .iter()
                .find(|request| {
                    request
                        .filter
                        .kinds
                        .as_ref()
                        .is_some_and(|kinds| kinds.contains(&5))
                })
                .map(|request| request.sub_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("connect replays the independent kind:5 request: {connected:?}"));
    let wire = wire_sub_string(&request);
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));

    let event = nostr::EventBuilder::new(Kind::EventDeletion, "")
        .tag(nostr::Tag::event(healthy_newer.id))
        .custom_created_at(Timestamp::from(300u64))
        .sign_with_keys(&author)
        .expect("valid deletion event");
    let failed_projection = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(&wire, event),
    ));
    assert!(failed_projection
        .iter()
        .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot)
            if snapshot.store_degraded.is_some())));

    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        eose_frame(&wire),
    ));
    let atom = ctx_atom_with(
        ConcreteFilter {
            kinds: Some(BTreeSet::from([5u16])),
            ..ConcreteFilter::default()
        },
        SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
    );
    assert!(core
        .get_coverage(&atom, &relay)
        .expect("coverage peek")
        .is_some());

    drop(core);
    let store = RedbStore::open(&path).expect("inspect committed deletion");
    assert!(
        store
            .query(&nostr::Filter::new().id(healthy_newer.id))
            .expect("the deleted healthy row has an exact readable id path")
            .is_empty(),
        "the projection failure happens after the deletion transaction commits"
    );
}

/// #816's request-atomic coverage falsifier. Two narrow atoms coalesced into
/// one wire request cross the real Redb boundary as one batch; a corrupt
/// existing row refuses the merge and leaves the other claim absent. A
/// separate request that was already in flight remains eligible and commits
/// normally afterward, proving the failure is not a process-wide latch.
#[test]
fn coverage_failure_is_atomic_for_one_request_and_isolated_from_another() {
    let a = Keys::generate();
    let b = Keys::generate();
    let healthy = Keys::generate();
    let failed_relay = RelayUrl::parse("wss://failed-coverage.example.com").unwrap();
    let healthy_relay = RelayUrl::parse("wss://healthy-coverage.example.com").unwrap();
    let atom_a = ctx_atom(cf(&[1], &[&a.public_key().to_hex()]));
    let atom_b = ctx_atom(cf(&[1], &[&b.public_key().to_hex()]));
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [failed_relay.clone()])
        .with_outbound_routes(b.public_key(), [failed_relay.clone()])
        .with_outbound_routes(healthy.public_key(), [healthy_relay.clone()]);
    let tempdir = tempfile::tempdir().expect("coverage fixture tempdir");
    let path = tempdir.path().join("coverage-write-corruption.redb");
    {
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .record_coverage(&[(
                atom_a.clone(),
                failed_relay.clone(),
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
            )])
            .expect("seed exact coverage row");
    }
    testing::corrupt_coverage(&path, nmp_store::coverage_key(&atom_a), &failed_relay)
        .expect("store-owned coverage corruption");
    let store = RedbStore::open(&path).expect("reopen Redb fixture");
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);

    let _ = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &a.public_key().to_hex(),
    )));
    let _ = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &b.public_key().to_hex(),
    )));
    let _ = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &healthy.public_key().to_hex(),
    )));
    let failed_connect = connect(&mut core, 0, &failed_relay);
    let failed_request = failed_connect
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&failed_relay) => {
                requests
                    .iter()
                    .find(|request| {
                        request
                            .filter
                            .kinds
                            .as_ref()
                            .is_some_and(|kinds| kinds.contains(&1))
                    })
                    .cloned()
            }
            _ => None,
        })
        .expect("failed relay replays its coalesced request");
    assert_eq!(
        failed_request.coverage_claims,
        BTreeSet::from([
            nmp_store::coverage_key(&atom_a),
            nmp_store::coverage_key(&atom_b),
        ]),
        "the one request must carry exactly both narrow coverage atoms"
    );

    let healthy_connect = connect(&mut core, 1, &healthy_relay);
    let healthy_request = healthy_connect
        .iter()
        .find_map(|effect| match effect {
            Effect::Replay(session, requests) if session == &public_session(&healthy_relay) => {
                requests
                    .iter()
                    .find(|request| {
                        request
                            .filter
                            .kinds
                            .as_ref()
                            .is_some_and(|kinds| kinds.contains(&1))
                    })
                    .map(|request| request.sub_id.clone())
            }
            _ => None,
        })
        .expect("healthy relay replays its independent request");
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));

    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&failed_relay),
        eose_frame(&wire_sub_string(&failed_request.sub_id)),
    ));
    let corrupt_error = core
        .get_coverage(&atom_a, &failed_relay)
        .expect_err("the corrupt coverage row must remain unreadable");
    assert_eq!(
        corrupt_error.fault(),
        PersistenceFault::Invariant,
        "stored-row decoding is an invariant failure"
    );
    assert!(
        corrupt_error.message().contains("decode coverage row"),
        "unexpected coverage refusal: {}",
        corrupt_error.message()
    );
    assert_eq!(
        core.get_coverage(&atom_b, &failed_relay)
            .expect("coverage peek"),
        None
    );

    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&healthy_relay),
        eose_frame(&wire_sub_string(&healthy_request)),
    ));
    let healthy_atom = ctx_atom(cf(&[1], &[&healthy.public_key().to_hex()]));
    assert!(core
        .get_coverage(&healthy_atom, &healthy_relay)
        .expect("coverage peek")
        .is_some());
}

// ---- epic #507 finding E5: wake_relay_lanes lane-relay index -----------
//
// `EngineCore::recover_all_lanes` used to be the ONLY way `wake_relay_lanes`
// (called on every relay connect/disconnect/auth event) could find a
// relay's lanes: a full `O(pending)` store re-read, filtered down to one
// relay afterward, and then run a SECOND time inside `schedule_ready` at the
// end of the same call. The fix adds two reducer-owned indexes
// (`intent_receipts`, `receipts_by_lane_relay`) so a single relay event only
// re-reads the intents actually routed through that relay, with a
// `lane_relay_index_degraded` safety valve that falls back to the exact old
// full-scan behavior whenever the index cannot be proven complete. The
// falsifiers below exercise both the narrow path and the degraded fallback.

/// A large durable backlog can contain obligations that own no physical lane
/// at all (for example, writes still waiting for a signer). Scheduling one
/// healthy routed write must read only that write's lane state, not perform
/// one empty store lookup for every unrelated obligation.
#[test]
fn schedule_ready_skips_lane_less_obligations() {
    const PARKED: usize = 207;
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://lane-less-scheduler.example.com").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
    activate(&mut core, &author);

    for i in 0..PARKED {
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(10_000 + i as u64, "parked")),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::RequestSign(..))));
    }

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(20_000, "healthy")),
        routing: WriteRouting::Explicit(vec![relay]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    core.reset_publish_queue_lane_recovery_reads();
    let signed = unsigned.sign_with_keys(&author).unwrap();
    core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    assert_eq!(
        core.publish_queue_lane_recovery_reads(),
        0,
        "the one current routed lane is already reducer-owned; {PARKED} unrelated signer waits and the healthy write must cost zero recovery reads"
    );
}

/// Falsifier (epic #507 finding E5): a single relay-connected event for
/// relay X must trigger `recover_publish_queue_lanes` only for X's own intent on
/// the wake path, not for every outstanding durable write. Composition of
/// the expected count: `schedule_ready` uses reducer-owned current lane rows
/// and performs zero recovery reads; the wake scan itself collapses from `N`
/// reads (the old `recover_all_lanes` + relay filter) down to exactly `1`
/// (only the receipt actually routed through the woken relay).
#[test]
fn wake_relay_lanes_only_rereads_the_woken_relays_own_intent() {
    const N: usize = 3;
    let author = Keys::generate();
    let relays: Vec<RelayUrl> = (0..N)
        .map(|i| RelayUrl::parse(&format!("wss://wake-falsifier-{i}.example.com")).unwrap())
        .collect();

    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
    activate(&mut core, &author);

    // N distinct durable writes, each routed to its OWN distinct relay, none
    // connected yet -- every one lands in `WaitingConnection`.
    for (i, relay) in relays.iter().enumerate() {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(100 + i as u64, &format!("falsifier {i}"))),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let (id, generation, u) = find_sign_request(&accepted);
        let signed = u.sign_with_keys(&author).unwrap();
        let _ = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    }

    // Reset the counter right before the event under test -- everything
    // above (N acceptances, each running its own `schedule_ready`) already
    // produced its own, unrelated `recover_publish_queue_lanes` traffic.
    let woken = relays[0].clone();
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&woken, author.public_key()),
    ));
    // The event under test is the bounded AUTH-discovery release (#8 U4):
    // connect itself now only parks the lane behind the probe; the wake that
    // actually publishes is `AuthProbeReleased`, with the same read
    // composition the old connect-time wake had.
    core.reset_publish_queue_lane_recovery_reads();
    let effects = core.handle(EngineMsg::AuthProbeReleased(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&woken, author.public_key()),
    ));

    assert_eq!(
        core.publish_queue_lane_recovery_reads(),
        1,
        "expected zero recovery reads from schedule_ready plus 1 read from \
         the exact wake scan (collapsed from N={N}) -- strictly less than \
         the old 2*N={}",
        2 * N,
    );

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(r, _, _) if r == &signer_session(&woken, author.public_key()))),
        "the woken relay's own write must still actually wake and publish, got {effects:?}"
    );
}

/// BDD falsifier for #985: answering whether an unchanged durable write still
/// owns its relay worker is reducer state, not recovery. `RelayOpenFailed`
/// asks that question once to decide whether the failure is still relevant
/// and pruning asks it again after recording the fact. Neither answer may
/// range-scan or decode the durable lane table after signing/bootstrap has
/// already established the exact lane state.
#[test]
fn unchanged_worker_demand_reads_zero_publish_queue_lanes() {
    const N: usize = 3;
    let author = Keys::generate();
    let relays: Vec<RelayUrl> = (0..N)
        .map(|i| RelayUrl::parse(&format!("wss://worker-projection-{i}.example.com")).unwrap())
        .collect();

    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
    activate(&mut core, &author);

    for (i, relay) in relays.iter().enumerate() {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(300 + i as u64, &format!("worker projection {i}"))),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let signed_effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(
            signed_effects.iter().any(|effect| matches!(
                effect,
                Effect::EnsureWriteRelay(session)
                    if session == &signer_session(relay, author.public_key())
            )),
            "bootstrap must establish real worker demand before the read-count assertion"
        );
    }

    core.reset_publish_queue_lane_recovery_reads();
    let required = signer_session(&relays[0], author.public_key());
    // REPEATED unchanged passes, not one: the residual #985 names is that the
    // count grew by `N` on EVERY dispatch pass, so a single pass could hide a
    // per-pass cost behind a one-off.
    for pass in 0..5 {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            required.clone(),
            "injected worker-open failure".to_string(),
        ));

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
            "the reducer must still recognize the projected worker as owned on pass {pass}"
        );
        assert_eq!(
            core.publish_queue_lane_recovery_reads(),
            0,
            "unchanged worker-demand checks must use reducer memory; durable lane \
             reads belong to bootstrap/recovery and actual lane transitions. After \
             {} passes over {N} pending intents the old body would have read {}",
            pass + 1,
            (pass + 1) * N
        );
    }
}

/// The #968 parking property that #985's sequencing comment asks for, stated
/// now so it is testable before parking itself lands: `N` intents that have
/// never routed own no lane, so they contribute zero worker demand and cost
/// zero per-dispatch store reads.
///
/// An accepted-but-unsigned durable write is exactly the shape `AwaitingRoute`
/// will have -- a live obligation in `pending`, owning a durable intent row,
/// having minted no lane -- so the property is asserted against that shape
/// rather than against a lifecycle that does not exist yet. When #968 lands, a
/// parked write that somehow acquired worker demand fails here.
#[test]
fn route_parked_intents_add_no_worker_demand_and_no_store_reads() {
    const PARKED: usize = 6;
    let author = Keys::generate();
    let routed_relay = RelayUrl::parse("wss://parked-routed.example.com").unwrap();
    let parked_relay = RelayUrl::parse("wss://parked-unrouted.example.com").unwrap();

    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
    activate(&mut core, &author);

    // One ordinary routed write, so the assertions below distinguish "parked
    // writes contribute nothing" from "this core computes nothing at all".
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(400, "parked control")),
        routing: WriteRouting::Explicit(vec![routed_relay.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned_event) = find_sign_request(&accepted);
    let signed = unsigned_event.sign_with_keys(&author).unwrap();
    let signed_effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    assert!(
        signed_effects.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session)
                if session == &signer_session(&routed_relay, author.public_key())
        )),
        "the routed control write must establish real worker demand"
    );

    for i in 0..PARKED {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(500 + i as u64, &format!("parked {i}"))),
            routing: WriteRouting::Explicit(vec![parked_relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        assert!(
            accepted
                .iter()
                .any(|effect| matches!(effect, Effect::RequestSign(..))),
            "the parked-shape fixture must actually be accepted as a durable obligation"
        );
        assert!(
            !accepted
                .iter()
                .any(|effect| matches!(effect, Effect::EnsureWriteRelay(_))),
            "a write that has not routed yet must not claim a relay worker: {accepted:?}"
        );
    }

    core.reset_publish_queue_lane_recovery_reads();
    let parked_session = signer_session(&parked_relay, author.public_key());
    for pass in 0..5 {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            parked_session.clone(),
            "injected parked-relay open failure".to_string(),
        ));
        assert!(
            effects.is_empty(),
            "{PARKED} parked intents own no lane at {parked_relay}, so an open \
             failure there is not the engine's concern (pass {pass}): {effects:?}"
        );
    }

    let routed_session = signer_session(&routed_relay, author.public_key());
    for pass in 0..5 {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            routed_session.clone(),
            "injected routed-relay open failure".to_string(),
        ));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
            "the one routed write still owns its worker on pass {pass}"
        );
    }

    assert_eq!(
        core.publish_queue_lane_recovery_reads(),
        0,
        "{PARKED} parked intents plus ten unchanged dispatch passes must cost \
         zero recover_publish_queue_lanes calls"
    );
}

/// #985's hardest fail-closed shape: a lane CREATION whose post-state was
/// never observed. Retaining the previous projection is not enough, because
/// the lanes that may or may not have committed are NEW -- so every relay the
/// attempted bootstrap could have minted a lane for stays conservatively
/// owned until an explicit recovery proves otherwise. A false-positive worker
/// can be retired later; a false negative strands a durable obligation
/// forever.
///
/// `bootstrap_publish_queue_lanes` is both the create-if-missing mutation and the
/// one complete read that establishes the projection, so even a provably
/// `Absent` outcome (the injected fault here) does not prove that OLDER lanes
/// were absent.
#[test]
fn an_unknown_lane_creation_failure_retains_every_candidate_worker() {
    let author = Keys::generate();
    let relays: Vec<RelayUrl> = (0..3)
        .map(|i| RelayUrl::parse(&format!("wss://unproven-creation-{i}.example.com")).unwrap())
        .collect();

    let mut core = EngineCore::new(
        RedbStore::temporary_with_failed_lane_bootstrap()
            .expect("temporary Redb lane-bootstrap failure fixture"),
        10,
    );
    activate(&mut core, &author);

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(600, "unproven lane creation")),
        routing: WriteRouting::Explicit(relays.to_vec()),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned_event) = find_sign_request(&accepted);
    let signed = unsigned_event.sign_with_keys(&author).unwrap();
    let event_id = signed.id;
    let signed_effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    // Non-vacuity: the injected bootstrap failure really is the path taken,
    // so the ownership below cannot be coming from an ordinary lane. The one
    // delivery owner publishes every receipt fact as an effect, so the whole
    // accept-and-sign sequence is the exact status stream a receipt observer
    // would have seen.
    let statuses: Vec<WriteFact> = receipt_statuses(&accepted)
        .into_iter()
        .chain(receipt_statuses(&signed_effects))
        .collect();
    for relay in &relays {
        assert!(
            statuses
                .iter()
                .any(|status| status == &attempt_stalled(event_id, relay)),
            "the fixture must actually take the failed-creation path for {relay}: {statuses:?}"
        );
    }

    for relay in &relays {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            signer_session(relay, author.public_key()),
            "injected open failure after an unprovable lane creation".to_string(),
        ));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
            "{relay} was a candidate of the failed bootstrap, so it must stay \
             owned rather than lose its worker on an unproven creation"
        );
    }
}

/// Manual before/after harness for #985. Run in release mode on the base and
/// candidate revisions with the same constants:
///
/// `cargo test -p nmp --release --test core_headless
/// relay_worker_projection_redb_benchmark -- --ignored --nocapture`
///
/// No wall-clock threshold lives in CI; the behavioral regression above owns
/// correctness. This harness supplies empirical magnitude through the real
/// redb lane representation and the real `RelayOpenFailed` ownership path.
#[test]
#[ignore = "manual before/after performance qualification"]
fn relay_worker_projection_redb_benchmark() {
    const INTENTS: usize = 64;
    const PASSES: usize = 200;

    let author = Keys::generate();
    let relays: Vec<RelayUrl> = (0..INTENTS)
        .map(|i| RelayUrl::parse(&format!("wss://worker-benchmark-{i}.example.com")).unwrap())
        .collect();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker-projection-benchmark.redb");
    let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), INTENTS + 1);
    activate(&mut core, &author);

    for (i, relay) in relays.iter().enumerate() {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(
                10_000 + i as u64,
                &format!("worker benchmark {i}"),
            )),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session)
                if session == &signer_session(relay, author.public_key())
        )));
    }

    let required = signer_session(&relays[0], author.public_key());
    let started = Instant::now();
    let mut diagnostic_batches = 0usize;
    for _ in 0..PASSES {
        let effects = core.handle(EngineMsg::RelayOpenFailed(
            required.clone(),
            "injected benchmark worker-open failure".to_string(),
        ));
        diagnostic_batches += effects
            .iter()
            .filter(|effect| matches!(effect, Effect::EmitDiagnostics(_)))
            .count();
        std::hint::black_box(effects);
    }
    let elapsed = started.elapsed();

    assert_eq!(diagnostic_batches, PASSES);
    println!(
        "relay_worker_projection_redb_benchmark intents={INTENTS} passes={PASSES} elapsed_us={}",
        elapsed.as_micros()
    );
}

/// Degraded-mode safety valve (epic #507 finding E5): when
/// `bootstrap_publish_queue_lanes` fails for one intent, the reverse index can no
/// longer be proven a superset of live lanes, so `wake_relay_lanes` must
/// fall back to the full `recover_all_lanes` scan rather than trust a
/// possibly-incomplete index. Proven two ways: an unrelated intent's lane
/// still correctly wakes and publishes (no missed wakeup), and the wake
/// event's `recover_publish_queue_lanes` call count matches the FULL-scan
/// composition rather than the narrower indexed one.
#[test]
fn degraded_index_falls_back_to_full_scan_and_never_misses_a_wakeup() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://wake-degraded.example.com").unwrap();

    let mut core = EngineCore::new(
        RedbStore::temporary_with_failed_lane_bootstrap()
            .expect("temporary Redb lane-bootstrap failure fixture"),
        10,
    );
    activate(&mut core, &author);

    // Intent #1: its `bootstrap_publish_queue_lanes` call is the injected failure
    // -- the reducer must degrade rather than pretend it has no lanes.
    let accepted1 = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(200, "degraded 1")),
        routing: WriteRouting::Explicit(vec![relay.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id1, gen1, u1) = find_sign_request(&accepted1);
    let signed1 = u1.sign_with_keys(&author).unwrap();
    let event_id1 = signed1.id;
    let signed_effects1 = core.handle(EngineMsg::SignerCompleted(id1, gen1, Ok(signed1)));
    assert!(
        signed_effects1
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(rid, fact)
                if *rid == id1 && fact == &attempt_stalled(event_id1, &relay))),
        "the injected bootstrap failure must surface as a persistence stall, got \
         {signed_effects1:?}"
    );

    // Intent #2: an ordinary write to the SAME relay accepted right after --
    // The construction arm is one-shot, so this one bootstraps normally and
    // the index DOES learn its lane.
    let accepted2 = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(201, "degraded 2")),
        routing: WriteRouting::Explicit(vec![relay.clone()]),
        identity: Identity::Active,
        correlation: None,
    }));
    let (id2, gen2, u2) = find_sign_request(&accepted2);
    let signed2 = u2.sign_with_keys(&author).unwrap();
    let signed_effects2 = core.handle(EngineMsg::SignerCompleted(id2, gen2, Ok(signed2)));
    assert!(
        signed_effects2.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteFact::Relay { relay: r, state: RelayState::Waiting(RelayWaiting::NotConnected), .. })
                if *rid == id2 && r == &relay
        )),
        "the second write must bootstrap normally and land in WaitingConnection, \
         got {signed_effects2:?}"
    );

    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, author.public_key()),
    ));
    // Same #8 U4 shift as `wake_relay_lanes_only_rereads_...`: the wake that
    // publishes is the bounded AUTH-discovery release, not connect itself.
    core.reset_publish_queue_lane_recovery_reads();
    let effects = core.handle(EngineMsg::AuthProbeReleased(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, author.public_key()),
    ));

    // No missed wakeup: intent #2's lane -- the only one the index could
    // ever have learned -- still wakes and publishes.
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(r, _, _) if r == &signer_session(&relay, author.public_key()))),
        "a degraded index must never cost a missed wakeup, got {effects:?}"
    );

    // #1537's concrete Redb-door count proves the FULL scan ran, not the
    // narrow index: 2 pending intents this event; the degraded wake reads
    // both directly (2) plus
    // `schedule_ready`'s own unchanged full scan (2) = 4. The non-degraded
    // composition here would have been 1 (index has exactly 1 receipt for
    // this relay) + 2 (schedule_ready) = 3.
    assert_eq!(
        core.publish_queue_lane_recovery_reads(),
        4,
        "expected the full-scan composition (2 wake + 2 schedule_ready), \
         proving the degraded flag drove this wake rather than the (here \
         incomplete) index"
    );
}

/// `receipt_for_intent` resolves correctly after `recover_on_boot` rebuilds
/// `intent_receipts` from scratch (epic #507 finding E5): two durable
/// writes, each on its own relay, are driven to `AwaitingAck` with
/// deliberately staggered deadlines before a simulated crash; after
/// reopening the store and recovering, each due deadline must still resolve
/// back to its OWN correct receipt id -- not the other's, and not silently
/// dropped (a broken index skips the status notification instead of
/// crashing, so this must be checked positively, not just for panics).
#[test]
fn receipt_for_intent_resolves_correctly_after_boot_recovery() {
    // Two DISTINCT authors: `publish_explicit` freezes a fixed (seq, content)
    // pair, so reusing one author for both calls on the same core would
    // freeze the identical event twice and collide as an exact duplicate
    // instead of creating two independent intents.
    let author_a = Keys::generate();
    let author_b = Keys::generate();
    let relay_a = RelayUrl::parse("wss://receipt-index-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://receipt-index-b.example.com").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipt-index.redb");

    let (receipt_a, receipt_b) = {
        let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
        connect_signer(&mut core, 0, &relay_a, author_a.public_key());
        connect_signer(&mut core, 1, &relay_b, author_b.public_key());
        release_author_probe(
            &mut core,
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            &relay_a,
            author_a.public_key(),
        );
        release_author_probe(
            &mut core,
            RelayHandle {
                slot: 1,
                generation: 1,
            },
            &relay_b,
            author_b.public_key(),
        );

        let _ = core.handle(EngineMsg::Tick(Timestamp::from(10)));
        let (receipt_a, _event_a, scheduled_a) =
            publish_explicit(&mut core, &author_a, [relay_a.clone()]);
        mark_written(&mut core, &scheduled_a, &relay_a); // AckTimeout deadline = 10 + 30

        let _ = core.handle(EngineMsg::Tick(Timestamp::from(20)));
        let (receipt_b, _event_b, scheduled_b) =
            publish_explicit(&mut core, &author_b, [relay_b.clone()]);
        mark_written(&mut core, &scheduled_b, &relay_b); // AckTimeout deadline = 20 + 30

        (receipt_a, receipt_b)
    };

    let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
    core.recover_on_boot();

    // relay_a's deadline (40) is due; relay_b's (50) is not yet.
    let effects_a = core.handle(EngineMsg::Tick(Timestamp::from(40)));
    assert!(
        effects_a.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteFact::Relay { relay, state: RelayState::Waiting(RelayWaiting::BackingOff { attempt: 1, .. }), .. })
                if *rid == receipt_a && relay == &relay_a
        )),
        "receipt_for_intent must resolve intent_a's due AckTimeout back to \
         receipt_a (not receipt_b, not silently dropped) after boot \
         recovery, got {effects_a:?}"
    );
    assert!(
        !effects_a.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteFact::Relay { relay, state: RelayState::Waiting(RelayWaiting::BackingOff { .. }), .. })
                if relay == &relay_b || *rid == receipt_b
        )),
        "relay_b's deadline is not yet due -- it must not fire early, got {effects_a:?}"
    );

    let effects_b = core.handle(EngineMsg::Tick(Timestamp::from(50)));
    assert!(
        effects_b.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteFact::Relay { relay, state: RelayState::Waiting(RelayWaiting::BackingOff { attempt: 1, .. }), .. })
                if *rid == receipt_b && relay == &relay_b
        )),
        "receipt_for_intent must resolve intent_b's due AckTimeout back to \
         receipt_b after boot recovery, got {effects_b:?}"
    );
}

/// `receipt_for_intent` for a still-open intent is unaffected by an
/// earlier, unrelated `pending` removal (epic #507 finding E5): closing one
/// durable write's obligation (a real removal, which walks
/// `forget_pending_indexes`) must not corrupt the `intent_receipts` entry
/// of a completely different, still-open write.
#[test]
fn receipt_for_intent_unaffected_by_an_earlier_pending_removal() {
    // Two DISTINCT authors, same reason as the boot-recovery test above:
    // `publish_explicit` freezes a fixed (seq, content) pair per call, so
    // reusing one author for both writes on the same core would collide as
    // an exact duplicate instead of creating two independent intents.
    let author1 = Keys::generate();
    let author2 = Keys::generate();
    let relay1 = RelayUrl::parse("wss://receipt-index-removal-1.example.com").unwrap();
    let relay2 = RelayUrl::parse("wss://receipt-index-removal-2.example.com").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    connect_signer(&mut core, 0, &relay1, author1.public_key());
    connect_signer(&mut core, 1, &relay2, author2.public_key());
    release_author_probe(
        &mut core,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &relay1,
        author1.public_key(),
    );
    release_author_probe(
        &mut core,
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        &relay2,
        author2.public_key(),
    );

    // Write #1: drive it all the way to a real, permanent `pending` removal
    // -- a successful ACK closes the intent once its one lane is terminal.
    let (_receipt1, event1, first1) = publish_explicit(&mut core, &author1, [relay1.clone()]);
    mark_written(&mut core, &first1, &relay1);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay1, event1.pubkey),
        RelayFrame::from(RelayMessage::ok(event1.id, true, "")),
    ));

    // Write #2: a completely separate, still-open intent.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(5)));
    let (receipt2, _event2, first2) = publish_explicit(&mut core, &author2, [relay2.clone()]);
    mark_written(&mut core, &first2, &relay2); // AckTimeout deadline = 5 + 30 = 35

    let effects = core.handle(EngineMsg::Tick(Timestamp::from(35)));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteFact::Relay { relay, state: RelayState::Waiting(RelayWaiting::BackingOff { attempt: 1, .. }), .. })
                if *rid == receipt2 && relay == &relay2
        )),
        "an earlier, unrelated pending removal (write #1's close) must not \
         corrupt receipt_for_intent's resolution of write #2's own due \
         deadline, got {effects:?}"
    );
}

// ---- #763: the deadline and coverage peeks -----------------------------

/// #763 falsifier: a failing expiration peek is a value the driver can act on,
/// not a panic and not a false `None`.
///
/// `EngineCore::next_deadline` is what the runtime loop arms its wait from,
/// on the embedder's own thread. A closed Redb generation must leave that door
/// as a typed failure; `Ok(None)` means only honest absence.
#[test]
fn a_failing_store_read_makes_the_next_deadline_a_typed_error_not_a_false_none() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
    let store = RedbStore::temporary_with_observation_precommit_io()
        .expect("temporary Redb observation-I/O fixture");
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);

    // Honest absence from a healthy store: nothing expiring, nothing due.
    assert_eq!(
        core.next_deadline().expect("a healthy peek answers"),
        None,
        "a fresh core genuinely has no deadline"
    );

    let _ = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &author.public_key().to_hex(),
    )));
    let _ = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
    ));
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(
            "s",
            nmp_resolver_testkit::kind1(&author, "close Redb generation", 1_000),
        ),
    ));
    assert!(
        failed.iter().any(
            |effect| matches!(effect, Effect::EmitDiagnostics(snapshot) if snapshot.store_degraded.is_some())
        ),
        "the real observation I/O refusal must degrade the store: {failed:?}"
    );

    let error = core
        .next_deadline()
        .expect_err("the closed Redb generation must not be reported as `no deadline`");
    assert_eq!(
        error.fault(),
        PersistenceFault::Latched,
        "the deadline read must preserve the real closed-handle fault: {}",
        error.message()
    );

    let _ = recover_after_observation_io(&mut core);
    assert_eq!(
        core.next_deadline()
            .expect("the reconstructed Redb generation answers"),
        None,
        "the same deadline door is healthy after reconstruction"
    );
}

/// #763 delivery falsifier: a corrupt durable delivery deadline is a typed
/// read failure, never false absence that parks the runtime forever.
#[test]
fn a_failing_publish_queue_deadline_read_is_a_typed_error_not_a_false_none() {
    let directory = tempfile::tempdir().expect("delivery deadline directory");
    let path = directory.path().join("delivery-deadline-corruption.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://delivery-deadline-corruption.example").unwrap();
    let deadline = Timestamp::from(1_033u64);
    let attempt = {
        let mut store = RedbStore::open(&path).expect("open persistent Redb fixture");
        let signed = nostr::EventBuilder::new(Kind::TextNote, "delivery deadline")
            .custom_created_at(Timestamp::from(1_000u64))
            .sign_with_keys(&keys)
            .expect("sign fixture event");
        let frozen = nostr::Event::new(
            signed.id,
            signed.pubkey,
            signed.created_at,
            signed.kind,
            signed.tags.clone(),
            signed.content.clone(),
            nmp_store::sentinel_signature(),
        );
        let accepted = store
            .accept_write(AcceptWrite {
                payload: nmp_store::AcceptWritePayload::Event {
                    frozen: Box::new(frozen),
                    replaceable_base: None,
                    monotonic_stamp: false,
                    routing: "delivery-deadline-proof".into(),
                    sig_state: nmp_store::IntentSigState::Pending,
                },
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "delivery-deadline-proof".into(),
                accepted_at: Timestamp::from(1_000u64),
                correlation: None,
            })
            .expect("accept fixture intent");
        let intent = accepted.journaled_intent_id().expect("journaled intent");
        store
            .promote_signed(
                nmp_store::PromotionTarget::Event(intent),
                nmp_store::VerifiedSignature::verify(&signed).expect("verify fixture signature"),
            )
            .expect("promote fixture intent");
        store
            .record_route_revision(intent, BTreeSet::from([relay]))
            .expect("record route");
        let lane = store
            .bootstrap_publish_queue_lanes(intent)
            .expect("bootstrap lane")
            .remove(0);
        let eligible = store
            .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_001u64))
            .expect("make lane eligible");
        let (attempt, started) = store
            .start_lane_attempt(
                &eligible.key,
                eligible.revision,
                signed,
                Timestamp::from(1_002u64),
            )
            .expect("start attempt");
        store
            .record_lane_handoff(
                &started.key,
                started.revision,
                started.last_ordinal,
                nmp_store::PublishQueueAttemptHandoff {
                    at: Timestamp::from(1_003u64),
                    result: nmp_store::HandoffEvidence::Written,
                },
                nmp_store::PublishQueuePostHandoffState::AwaitingAck { deadline },
            )
            .expect("record awaiting-ack handoff");
        assert_eq!(store.next_expiration().unwrap(), None);
        assert_eq!(store.next_publish_queue_deadline().unwrap(), Some(deadline));
        attempt
    };

    testing::corrupt_publish_queue_deadline(&path, &attempt)
        .expect("corrupt the exact ordered deadline row");
    let store = RedbStore::open(&path).expect("reopen corrupted fixture");
    assert_eq!(store.next_expiration().unwrap(), None);
    let core = EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10);
    let error = core
        .next_deadline()
        .expect_err("the corrupt delivery deadline must not become false absence");
    assert_eq!(error.fault(), PersistenceFault::Invariant);
    assert!(
        error.message().contains("decode publish queue deadline"),
        "the exact deadline codec owns the error: {}",
        error.message()
    );
}

/// #763 under deferred relay admission: the cache seed is accepted before a
/// relay plan exists. If the post-admission coverage projection fails, the
/// engine degrades without replacing that seed with fabricated relay proof.
///
/// `AcquisitionEvidence::sources[..].reconciled_through == None` is a claim
/// about what a relay has proven. A store that could not be read has
/// established no such thing, so it must not be able to render as one.
#[test]
fn a_failing_post_admission_coverage_peek_keeps_the_immediate_seed() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let absent_relay = RelayUrl::parse("wss://absent.example.com").unwrap();
    let atom = ctx_atom(cf(&[1], &[&author.public_key().to_hex()]));
    let key = nmp_store::coverage_key(&atom);
    let directory = tempfile::tempdir().expect("coverage corruption directory");
    let path = directory.path().join("post-admission-coverage.redb");
    {
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .record_coverage(&[(
                atom.clone(),
                relay.clone(),
                CoverageInterval::new(Timestamp::from(10u64), Timestamp::from(20u64)),
            )])
            .expect("seed exact coverage row");
    }
    let wrong_atom = ctx_atom(cf(&[2], &[&author.public_key().to_hex()]));
    assert_eq!(
        testing::corrupt_coverage(&path, nmp_store::coverage_key(&wrong_atom), &relay)
            .expect_err("a different coverage key must be refused")
            .fault(),
        PersistenceFault::Invariant
    );
    assert_eq!(
        testing::corrupt_coverage(&path, key, &absent_relay)
            .expect_err("an absent relay row must be refused")
            .fault(),
        PersistenceFault::Invariant
    );
    {
        let store = RedbStore::open(&path).expect("inspect refused corruption controls");
        assert_eq!(
            store.get_coverage(key, &relay).unwrap(),
            Some(CoverageInterval::new(
                Timestamp::from(10u64),
                Timestamp::from(20u64)
            )),
            "wrong-target refusals must leave the exact row healthy"
        );
    }
    testing::corrupt_coverage(&path, key, &relay).expect("corrupt exact coverage row");

    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
    let store = RedbStore::open(&path).expect("reopen corrupt coverage fixture");
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);
    let _ = connect(&mut core, 0, &relay);

    let seed = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &author.public_key().to_hex(),
    )));
    assert!(
        seed.iter().all(
            |effect| !matches!(effect, Effect::EmitDiagnostics(snap) if snap.store_degraded.is_some())
        ),
        "the immediate seed must precede the deferred coverage read: {seed:?}"
    );
    assert_eq!(
        seed.iter()
            .filter(|effect| matches!(effect, Effect::EmitRows(..)))
            .count(),
        1,
        "the immediate cache seed is delivered before relay admission"
    );

    let admission = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
    assert!(
        admission
            .iter()
            .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot) if snapshot.store_degraded.as_deref().is_some_and(|message| message.contains("decode coverage row")))),
        "the exact coverage decode failure must surface after admission: {admission:?}"
    );
    assert_eq!(
        admission
            .iter()
            .filter(|effect| matches!(effect, Effect::EmitRows(..)))
            .count(),
        0,
        "failed post-admission evidence must not replace the delivered seed"
    );
}

/// #763 falsifier: a coverage peek that fails while REFRESHING evidence
/// leaves the last delivered evidence standing.
///
/// The open above had nowhere to fall back to; a live observation does. The
/// established #122 rule for a failed read on a live handle is "leave the
/// last delivered state alone and degrade" (`refresh_observation` says so
/// for rows), and evidence follows it: re-emitting a frame computed from a
/// failed read would publish "nothing proven" over a watermark this reducer
/// merely could not see.
#[test]
fn a_failing_coverage_peek_never_republishes_live_evidence_as_unproven() {
    let account_a = Keys::generate();
    let account_b = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let atom_a = ctx_atom(cf(&[1], &[&account_a.public_key().to_hex()]));
    let atom_b = ctx_atom(cf(&[1], &[&account_b.public_key().to_hex()]));
    let directory = tempfile::tempdir().expect("reactive coverage corruption directory");
    let path = directory.path().join("reactive-coverage.redb");
    {
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .record_coverage(&[
                (
                    atom_a,
                    relay.clone(),
                    CoverageInterval::new(Timestamp::from(10u64), Timestamp::from(20u64)),
                ),
                (
                    atom_b.clone(),
                    relay.clone(),
                    CoverageInterval::new(Timestamp::from(10u64), Timestamp::from(20u64)),
                ),
            ])
            .expect("seed healthy A and target B coverage");
    }
    testing::corrupt_coverage(&path, nmp_store::coverage_key(&atom_b), &relay)
        .expect("corrupt B coverage row");

    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(account_a.public_key(), [relay.clone()])
        .with_outbound_routes(account_b.public_key(), [relay.clone()]);
    let store = RedbStore::open(&path).expect("reopen reactive coverage fixture");
    let mut core = EngineCore::new_with_fixture_routing_facts(store, dir, 10);
    let _ = connect(&mut core, 0, &relay);
    let _ = core.handle(EngineMsg::SetActivePubkey(Some(account_a.public_key())));
    let opened = core.handle_and_flush(EngineMsg::Subscribe(LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
        ..Filter::default()
    })));
    assert!(
        opened.iter().all(
            |effect| !matches!(effect, Effect::EmitDiagnostics(snapshot) if snapshot.store_degraded.is_some())
        ),
        "A's coverage row must be healthy before the reactive switch: {opened:?}"
    );
    assert!(
        opened
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(..))),
        "a healthy A open delivers its initial evidence frame"
    );

    let effects = core.handle(EngineMsg::SetActivePubkey(Some(account_b.public_key())));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::EmitDiagnostics(snapshot) if snapshot.store_degraded.as_deref().is_some_and(|message| message.contains("decode coverage row")))),
        "B's first post-open coverage dereference must surface the decode failure: {effects:?}"
    );
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::EmitRows(..))),
        "a failed reactive coverage read must not republish evidence"
    );
}
