use nmp_grammar::RelaySessionKey;
use super::*;
use nmp_store::testing;

// ---- fallible persistence doors and recovery indexing ------------------

/// Door-level falsifier (issue #122): the real Redb `insert` transaction
/// surfaces a persistence I/O failure as `Err(PersistenceError)` rather than
/// panicking, and the failed generation stays closed rather than answering
/// the read as an absence.
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
    store
        .insert(event, from)
        .expect_err("an ingest-path I/O failure must surface as Err");
    // The refusal precedes the commit, so the whole transaction rolled back:
    // the store holds nothing the caller was told did not land.
    assert!(store
        .query(&nostr::Filter::new().id(event_id))
        .expect("a rolled-back ingest leaves the store readable")
        .is_empty());
}

/// Engine-level falsifier (issue #122): a relay EVENT frame whose store
/// `insert` fails on I/O never panics the reducer. The failed frame delivers
/// no phantom rows, reports nothing about the failure, and the engine stays
/// usable for later messages.
#[test]
fn ingest_io_failure_never_panics_and_fabricates_no_rows() {
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

    // A failed ingest fabricates no rows.
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "a failed ingest must not deliver phantom rows, got {effects:?}"
    );
    // The reducer keeps handling messages afterwards.
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

    let _ = core.handle(EngineMsg::RelayFrame(
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
        core.get_coverage(&atom, &RelaySessionKey::unauthenticated(relay.clone())).expect("coverage peek"),
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
        .get_coverage(&healthy_atom, &RelaySessionKey::unauthenticated(healthy_relay.clone()))
        .expect("coverage peek")
        .is_some());
}

/// The event-failure key is the full physical relay session, not the relay
/// URL. Two identical pinned selections therefore keep independent coverage
/// authority when one runs on Public and the other on `Nip42(author)`.
#[test]
fn failed_event_commit_isolated_by_session_identity_on_the_same_relay() {
    let public_author = Keys::generate();
    let protected_author = Keys::generate();
    let relay = RelayUrl::parse("wss://access-isolation.example.com").unwrap();
    let source = ReadRouting::Explicit(vec![relay.clone()]);
    let selection = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        ..Filter::default()
    };
    let public_query = LiveQuery::single(
        nmp_grammar::Demand::new(selection.clone(), source.clone())
            .expect("public pinned demand"),
    );
    let protected_query = {
        let mut demand = nmp_grammar::Demand::new(selection, source.clone())
            .expect("protected pinned demand");
        demand.authenticate_as = Some(protected_author.public_key());
        LiveQuery::single(demand)
    };
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
    assert_req_reaches_the_wire(&protected_connected, &protected_session);
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
    let _ = core.handle(EngineMsg::RelayFrame(
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
        routing: source.clone(),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let protected_atom = ContextualAtom {
        filter: concrete,
        routing: source,
        authenticate_as: Some(protected_author.public_key()),
        routing_evidence: BTreeSet::new(),
    };
    assert_eq!(
        core.get_coverage(&public_atom, &RelaySessionKey::unauthenticated(relay.clone()))
            .expect("coverage peek"),
        None
    );
    assert!(
        core.get_coverage(
            &protected_atom,
            &signer_session(&relay, protected_author.public_key())
        )
        .expect("coverage peek")
        .is_some(),
        "the identity that proved this coverage is the session it is filed under"
    );
    assert_eq!(
        core.get_coverage(&protected_atom, &RelaySessionKey::unauthenticated(relay.clone()))
            .expect("coverage peek"),
        None,
        "and the anonymous view of the same relay proved nothing"
    );
}

/// A failed EVENT commit poisons only the immutable request that delivered
/// it: that request's later EOSE earns no coverage, while its siblings and
/// any later request are untouched.
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
    let _ = core.handle(EngineMsg::RelayFrame(
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
    let third = core.handle_and_flush(EngineMsg::Subscribe(literal_query(
        &[1],
        &c.public_key().to_hex(),
    )));
    let third_sub = req_for(&third, &relay).0.clone();
    assert_ne!(first_sub, third_sub);
    assert_ne!(second_sub, third_sub);

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
            core.get_coverage(atom, &RelaySessionKey::unauthenticated(relay.clone())).expect("coverage peek"),
            None,
            "the poisoned request's stale EOSE must not mint coverage"
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
    assert!(
        core.get_coverage(&atom_c, &RelaySessionKey::unauthenticated(relay.clone()))
            .expect("coverage peek")
            .is_some(),
        "the fresh successor earns coverage for its own atom"
    );
    // The poisoned request is never rebuilt, so its atom never earns
    // coverage. That is the whole cost of the failed commit: this read has
    // to be asked again, and nothing pretends it was answered.
    assert_eq!(
        core.get_coverage(&atom_a, &RelaySessionKey::unauthenticated(relay.clone()))
            .expect("coverage peek"),
        None,
        "a poisoned request's atom stays uncovered rather than silently \
         inheriting a sibling's authority"
    );
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
            inner: Demand {
                selection: Filter {
                    kinds: Some(BTreeSet::from([1u16])),
                    authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                    limit: Some(1),
                    ..Filter::default()
                },
                ..Demand::default()
            },
            project: nmp_grammar::Selector::Authors,
        })),
    );
    let derived_from_latest_note = LiveQuery::single(Demand {
        selection: derived_filter,
        ..Demand::default()
    });
    let _initial = core.handle_and_flush(EngineMsg::Subscribe(derived_from_latest_note));
    let kind5 = LiveQuery::single(
        nmp_grammar::Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([5u16])),
                ..Filter::default()
            },
            ReadRouting::Explicit(vec![relay.clone()])
        )
        .expect("a literal kind:5 request can be pinned to the fixture relay"),
    );
    let _kind5_open = core.handle_and_flush(EngineMsg::Subscribe(kind5));
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
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame(&wire, event),
    ));

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
        ReadRouting::Explicit(vec![relay.clone()]),
    );
    assert!(core
        .get_coverage(&atom, &RelaySessionKey::unauthenticated(relay.clone()))
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
                RelaySessionKey::unauthenticated(failed_relay.clone()),
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
        .get_coverage(&atom_a, &RelaySessionKey::unauthenticated(failed_relay.clone()))
        .expect_err("the corrupt coverage row must remain unreadable");
    assert!(
        corrupt_error.message().contains("decode coverage row"),
        "unexpected coverage refusal: {}",
        corrupt_error.message()
    );
    assert_eq!(
        core.get_coverage(&atom_b, &RelaySessionKey::unauthenticated(failed_relay.clone()))
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
        .get_coverage(&healthy_atom, &RelaySessionKey::unauthenticated(healthy_relay.clone()))
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
// re-reads the intents actually routed through that relay. The falsifiers
// below exercise that narrow path.

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
        }));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::RequestSign(..))));
    }

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(20_000, "healthy")),
        routing: WriteRouting::Explicit(vec![relay]),
        identity: Identity::Active,
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

/// A failed `bootstrap_publish_queue_lanes` costs exactly the intent whose
/// bootstrap failed, and nothing else. There is one wake path — the narrow
/// `receipts_by_lane_relay` index — so a sibling intent whose bootstrap DID
/// commit is in that index and still wakes and publishes normally (no missed
/// wakeup). The failed intent contributes no lanes to wake because it
/// committed none: that is the progress a store failure costs, and the next
/// boot rebuilds it from the durable rows. The read count below pins that
/// there is no wider fallback scan hiding behind the narrow read.
#[test]
fn a_failed_lane_bootstrap_never_costs_a_sibling_intent_its_wakeup() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://wake-degraded.example.com").unwrap();

    let mut core = EngineCore::new(
        RedbStore::temporary_with_failed_lane_bootstrap()
            .expect("temporary Redb lane-bootstrap failure fixture"),
        10,
    );
    activate(&mut core, &author);

    // Intent #1: its `bootstrap_publish_queue_lanes` call is the injected
    // failure, so its lanes never enter the projection and nothing is
    // reported about them.
    let accepted1 = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(200, "degraded 1")),
        routing: WriteRouting::Explicit(vec![relay.clone()]),
        identity: Identity::Active,
    }));
    let (id1, gen1, u1) = find_sign_request(&accepted1);
    let signed1 = u1.sign_with_keys(&author).unwrap();
    let event_id1 = signed1.id;
    let signed_effects1 = core.handle(EngineMsg::SignerCompleted(id1, gen1, Ok(signed1)));
    let _ = event_id1;
    assert!(
        !signed_effects1
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(rid, WriteFact::Relay { relay: r, .. })
                if *rid == id1 && r == &relay)),
        "a failed lane bootstrap reports nothing about the relay it could not \
         reach, got {signed_effects1:?}"
    );

    // Intent #2: an ordinary write to the SAME relay accepted right after --
    // The construction arm is one-shot, so this one bootstraps normally and
    // the index DOES learn its lane.
    let accepted2 = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(201, "degraded 2")),
        routing: WriteRouting::Explicit(vec![relay.clone()]),
        identity: Identity::Active,
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

    // One store read, down from four. The narrow index names exactly the one
    // receipt whose bootstrap committed, and `schedule_ready` now answers
    // from the reducer's own projection instead of re-reading every intent's
    // lanes -- deleting the degraded full-scan fallback took its two reads
    // with it. Intent #1 contributes nothing because its lanes never
    // committed: that is the progress a store failure costs, and the next
    // boot rebuilds it from the durable rows.
    assert_eq!(
        core.publish_queue_lane_recovery_reads(),
        1,
        "expected exactly the narrow index read"
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

/// #763 under deferred relay admission: the cache seed is accepted before a
/// relay plan exists. If the post-admission coverage projection fails, the
/// engine degrades without replacing that seed with fabricated relay proof.
///
/// `AcquisitionEvidence::sources[..].reconciled_through.is_none()` is a claim
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
                RelaySessionKey::unauthenticated(relay.clone()),
                CoverageInterval::new(Timestamp::from(10u64), Timestamp::from(20u64)),
            )])
            .expect("seed exact coverage row");
    }
    let wrong_atom = ctx_atom(cf(&[2], &[&author.public_key().to_hex()]));
    testing::corrupt_coverage(&path, nmp_store::coverage_key(&wrong_atom), &relay)
        .expect_err("a different coverage key must be refused");
    testing::corrupt_coverage(&path, key.clone(), &absent_relay)
        .expect_err("an absent relay row must be refused");
    {
        let store = RedbStore::open(&path).expect("inspect refused corruption controls");
        assert_eq!(
            store.get_coverage(key.clone(), &RelaySessionKey::unauthenticated(relay.clone())).unwrap(),
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
    assert_eq!(
        seed.iter()
            .filter(|effect| matches!(effect, Effect::EmitRows(..)))
            .count(),
        1,
        "the immediate cache seed is delivered before relay admission"
    );

    let admission = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
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
                    RelaySessionKey::unauthenticated(relay.clone()),
                    CoverageInterval::new(Timestamp::from(10u64), Timestamp::from(20u64)),
                ),
                (
                    atom_b.clone(),
                    RelaySessionKey::unauthenticated(relay.clone()),
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
    let opened = core.handle_and_flush(EngineMsg::Subscribe(LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        ..Demand::default()
    })));
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
            .all(|effect| !matches!(effect, Effect::EmitRows(..))),
        "a failed reactive coverage read must not republish evidence"
    );
}

/// The claim: **an accepted write is never lost -- only its progress is.**
///
/// This is the whole contract that survives deleting NMP's modelling of local
/// disk failure. There is no classification, no degraded mode, no reopen and
/// no latched fault; a store write that fails, fails. What bounds the damage
/// is acceptance atomicity -- `accept_write` commits the intent, the receipt,
/// the frozen body and the canonical pending row in ONE transaction, and
/// `publish()` returning `Ok` is only constructible after that commit. So
/// every failure AFTER acceptance can destroy progress and nothing else, and
/// the next process to open the file finds the write and resumes it.
///
/// Both falsifiers below drive a REAL redb store on a REAL file through a
/// real post-acceptance commit failure, then drop the engine entirely and
/// open a fresh one over the same file -- a process restart in everything but
/// name. Neither reuses the failed engine, because there is no door that
/// would let them.
///
/// A post-acceptance store failure emits no app-facing fact and
/// costs only progress: a fresh engine over the same file recovers the
/// receipt, its frozen bytes and its route set, and the write resumes.
#[test]
fn a_failed_lane_attempt_commit_loses_progress_and_a_fresh_engine_resumes_the_write() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://lane-start-failure.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lane-attempt-failure.redb");

    let (receipt, signed) = {
        // 1. A real persistent redb engine whose lane-attempt commit refuses
        //    for this exact relay.
        let store = RedbStore::open_with_failed_lane_starts(&path, [relay.clone()])
            .expect("open the lane-start failure fixture");
        let mut core = EngineCore::new(store, 10);
        connect_signer(&mut core, 0, &relay, author.public_key());
        authenticate_signer(&mut core, 0, &relay, &author);

        // 2. Publish. Acceptance is a separate, already-committed transaction,
        //    so this takes custody and answers with a receipt id.
        let (receipt, signed, effects) = publish_explicit(&mut core, &author, [relay.clone()]);

        // 3. The lane attempt did not commit, so nothing reached the wire.
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::PublishEvent(..))),
            "the failed attempt commit must not emit a wire EVENT: {effects:?}"
        );

        // 4. No app-facing fault fact, and the engine keeps serving.
        assert!(
            no_relay_fact_for(&receipt_statuses(&effects), &relay),
            "a post-acceptance store failure produces no app-facing fact: {effects:?}"
        );
        let replay = core.reattach_receipt(receipt);
        assert!(replay.is_attached(), "the write is still live and reattachable");
        assert!(
            no_relay_fact_for(&replay.facts, &relay),
            "and its durable prefix invents no fact either: {:?}",
            replay.facts
        );
        // Still serving: an ordinary message is handled, not refused.
        let _ = core.handle(EngineMsg::Tick(Timestamp::from(1u64)));
        assert!(core.reattach_receipt(receipt).is_attached());

        (receipt, signed)
    };
    // 5. The engine is gone. Nothing in this process holds the store.

    // 6. A FRESH engine over the same file recovers the write and resumes it.
    let mut restarted = EngineCore::new(
        RedbStore::open(&path).expect("a fresh generation opens the same file"),
        10,
    );
    let boot = restarted.recover_on_boot();
    assert!(
        boot.iter().any(|effect| matches!(effect, Effect::EnsureWriteRelay(session)
            if session == &signer_session(&relay, author.public_key()))),
        "boot must re-arm the exact relay the failed attempt never reached: {boot:?}"
    );

    // The receipt survived, and it is the SAME write -- its frozen bytes are
    // the ones that were signed, not a coincidentally similar event.
    let replay = restarted.reattach_receipt(receipt);
    assert!(replay.is_attached(), "the accepted write survived the restart");
    assert_eq!(
        replay.facts.first(),
        Some(&WriteFact::Signing(SigningState::Signed {
            event_id: signed.id
        })),
        "the recovered receipt must carry the exact frozen event id: {:?}",
        replay.facts
    );

    // Its route set survived too: the durable route revision committed at
    // acceptance time, which is why boot could name the relay above.
    let entry = restarted
        .publish_queue_entries(None, 8)
        .expect("enumerate the recovered queue")
        .into_iter()
        .find(|entry| entry.receipt_id == receipt)
        .expect("the recovered write is enumerable");
    assert_eq!(entry.event_id, signed.id, "the frozen body is the recovered one");
    assert!(
        entry.relays.contains(&relay),
        "the durable route set is recovered: {:?}",
        entry.relays
    );
    assert!(
        entry.outcome.is_none(),
        "the write resumes rather than having been terminalized by the failure: {:?}",
        entry.outcome
    );

    // And it actually sends: the same relay connects and the write goes out.
    connect_signer(&mut restarted, 0, &relay, author.public_key());
    let sent = authenticate_signer(&mut restarted, 0, &relay, &author);
    assert!(
        sent.iter().any(|effect| matches!(
            effect,
            Effect::PublishEvent(session, event, _)
                if session == &signer_session(&relay, event.pubkey) && event.id == signed.id
        )),
        "the resumed write must reach the wire on the healthy generation: {sent:?}"
    );
}

/// The same claim against the other post-acceptance commit: the append-only
/// route revision. Here the failure destroys MORE progress -- not even the
/// resolved relay URL is durable -- so boot has to re-resolve from the
/// intent's surviving routing strategy rather than read the route back.
///
/// This is also what makes `route_complete` false on a failed route commit
/// load-bearing: routing that named relays but persisted none is UNFINISHED
/// work, and reporting it as complete would let the empty durable route set
/// read as the terminal `NoDestination` verdict -- which would lose the
/// write, not merely its progress.
#[test]
fn a_failed_route_revision_commit_loses_progress_and_a_fresh_engine_resumes_the_write() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://route-revision-failure.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("route-revision-failure.redb");

    let (receipt, signed) = {
        let store = RedbStore::open_with_route_revision_write_failure(&path)
            .expect("open the route-revision failure fixture");
        let mut core = EngineCore::new(store, 10);
        connect_signer(&mut core, 0, &relay, author.public_key());
        authenticate_signer(&mut core, 0, &relay, &author);

        let (receipt, signed, effects) = publish_explicit(&mut core, &author, [relay.clone()]);

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::PublishEvent(..))),
            "a route revision that did not commit mints no lane: {effects:?}"
        );
        assert!(
            no_relay_fact_for(&receipt_statuses(&effects), &relay),
            "a post-acceptance store failure produces no app-facing fact: {effects:?}"
        );

        // The write must NOT be terminalized. An empty durable route set plus
        // a "complete" routing answer is exactly the shape that would read as
        // `NoDestination`, and that verdict is unrecoverable.
        let statuses = receipt_statuses(&effects);
        assert!(
            !statuses.iter().any(|fact| matches!(
                fact,
                WriteFact::Outcome(WriteOutcome::NoDestination)
            )),
            "a route revision that did not persist must never terminalize the write as \
             NoDestination -- that would lose it, not just its progress: {statuses:?}"
        );

        assert!(core.reattach_receipt(receipt).is_attached());
        let _ = core.handle(EngineMsg::Tick(Timestamp::from(1u64)));
        assert!(core.reattach_receipt(receipt).is_attached());

        (receipt, signed)
    };

    let mut restarted = EngineCore::new(
        RedbStore::open(&path).expect("a fresh generation opens the same file"),
        10,
    );
    let boot = restarted.recover_on_boot();

    let replay = restarted.reattach_receipt(receipt);
    assert!(replay.is_attached(), "the accepted write survived the restart");
    assert_eq!(
        replay.facts.first(),
        Some(&WriteFact::Signing(SigningState::Signed {
            event_id: signed.id
        })),
        "the recovered receipt must carry the exact frozen event id: {:?}",
        replay.facts
    );

    // The routing STRATEGY survived acceptance even though its answer did
    // not, so boot re-resolves it, commits the revision this time, and re-arms
    // the relay.
    assert!(
        boot.iter().any(|effect| matches!(effect, Effect::EnsureWriteRelay(session)
            if session == &signer_session(&relay, author.public_key()))),
        "boot must re-resolve the intent's surviving routing and re-arm its relay: {boot:?}"
    );
    let entry = restarted
        .publish_queue_entries(None, 8)
        .expect("enumerate the recovered queue")
        .into_iter()
        .find(|entry| entry.receipt_id == receipt)
        .expect("the recovered write is enumerable");
    assert!(
        entry.relays.contains(&relay),
        "the re-resolved route set is durable on the healthy generation: {:?}",
        entry.relays
    );
    assert!(entry.outcome.is_none(), "the write resumes: {:?}", entry.outcome);

    connect_signer(&mut restarted, 0, &relay, author.public_key());
    let sent = authenticate_signer(&mut restarted, 0, &relay, &author);
    assert!(
        sent.iter().any(|effect| matches!(
            effect,
            Effect::PublishEvent(session, event, _)
                if session == &signer_session(&relay, event.pubkey) && event.id == signed.id
        )),
        "the resumed write must reach the wire on the healthy generation: {sent:?}"
    );
}
