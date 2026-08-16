//! execution targets admission proofs.

use super::*;

#[test]
fn incompatible_requests_visit_only_their_exact_execution_targets() {
    const OWNERS: u16 = 64;
    let relay = RelayUrl::parse("wss://admission-request-targets.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let observations: Vec<_> = (0..OWNERS)
        .map(|index| {
            observation_id(
                &core.handle(EngineMsg::Subscribe(unbounded_incompatible_query(
                    &relay, index,
                ))),
            )
        })
        .collect();

    core.request_target_demand_keys_touched.set(0);
    core.request_target_candidates_examined.set(0);
    let admitted = flush(&mut core);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        OWNERS as usize
    );
    let session = RelaySessionKey::public(relay);
    let requests = core.router.plan().reqs[&session].clone();
    let relay_request_facts = requests
        .iter()
        .flat_map(|request| {
            accept_request(
                &mut core,
                &session,
                &request.sub_id,
                request.filter.hash(),
                TransportRelayHandle {
                    slot: 18,
                    generation: 1,
                },
            )
        })
        .filter_map(|effect| match effect {
            Effect::EmitObservationEvidence(_, evidence) => Some(evidence),
            _ => None,
        })
        .flatten()
        .filter(|evidence| matches!(evidence.fact, ObservationFact::RelayRequest { .. }))
        .count();
    assert_eq!(
        relay_request_facts, OWNERS as usize,
        "no sibling observation gets a frame"
    );
    assert_eq!(core.request_target_demand_keys_touched.get(), OWNERS as u64);
    assert_eq!(
        core.request_target_candidates_examined.get(),
        OWNERS as u64,
        "each immutable request callback inspects only its one current exact observation target"
    );

    for observation in observations {
        core.handle(EngineMsg::Unsubscribe(observation));
    }
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn cache_only_siblings_are_not_execution_targets_of_a_live_request() {
    for live_closes_first in [false, true] {
        let relay = RelayUrl::parse("wss://admission-request-target-owner.example").unwrap();
        let session = RelaySessionKey::public(relay.clone());
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        let live = observation_id(&core.handle(EngineMsg::Subscribe(query(
            &relay,
            "same",
            Freshness::Live,
        ))));
        let cache_only = observation_id(&core.handle(EngineMsg::Subscribe(query(
            &relay,
            "same",
            Freshness::CacheOnly,
        ))));
        assert_eq!(
            wire_ops(&flush(&mut core))
                .into_iter()
                .filter(|op| matches!(op, WireOp::Req(_, _)))
                .count(),
            1
        );
        assert_eq!(
            relay_request_observations(&accept_first_request(&mut core, &session, 19)),
            BTreeSet::from([live])
        );

        let standing_cache = if live_closes_first {
            assert_eq!(
                wire_ops(&core.handle(EngineMsg::Unsubscribe(live)))
                    .into_iter()
                    .filter(|op| matches!(op, WireOp::Close(_)))
                    .count(),
                1
            );
            cache_only
        } else {
            assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(cache_only))).is_empty());
            let reopened_cache = observation_id(&core.handle(EngineMsg::Subscribe(query(
                &relay,
                "same",
                Freshness::CacheOnly,
            ))));
            assert_eq!(
                wire_ops(&core.handle(EngineMsg::Unsubscribe(live)))
                    .into_iter()
                    .filter(|op| matches!(op, WireOp::Close(_)))
                    .count(),
                1
            );
            reopened_cache
        };

        let reopened_live = observation_id(&core.handle(EngineMsg::Subscribe(query(
            &relay,
            "same",
            Freshness::Live,
        ))));
        assert_eq!(
            wire_ops(&flush(&mut core))
                .into_iter()
                .filter(|op| matches!(op, WireOp::Req(_, _)))
                .count(),
            1
        );
        assert_eq!(
            relay_request_observations(&accept_first_request(&mut core, &session, 20)),
            BTreeSet::from([reopened_live])
        );
        core.handle(EngineMsg::Unsubscribe(standing_cache));
        core.handle(EngineMsg::Unsubscribe(reopened_live));
        assert_eq!(
            core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

#[test]
fn a_shared_request_targets_every_wire_active_owner_and_no_cache_only_sibling() {
    let relay = RelayUrl::parse("wss://admission-request-target-shared.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let live_a =
        observation_id(&core.handle(EngineMsg::Subscribe(query(&relay, "same", Freshness::Live))));
    let live_b =
        observation_id(&core.handle(EngineMsg::Subscribe(query(&relay, "same", Freshness::Live))));
    let cache_only = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "same",
        Freshness::CacheOnly,
    ))));

    core.request_target_candidates_examined.set(0);
    flush(&mut core);
    let handed_off = accept_first_request(&mut core, &session, 21);
    assert_eq!(
        relay_request_observations(&handed_off),
        BTreeSet::from([live_a, live_b])
    );
    assert_eq!(core.request_target_candidates_examined.get(), 2);
    assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(live_a))).is_empty());
    assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(cache_only))).is_empty());
    assert_eq!(
        wire_ops(&core.handle(EngineMsg::Unsubscribe(live_b)))
            .into_iter()
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn window_distinct_requests_target_only_their_exact_demand_owners_on_send_and_replay() {
    for limited_closes_first in [false, true] {
        let relay = RelayUrl::parse("wss://admission-request-target-window.example").unwrap();
        let session = RelaySessionKey::public(relay.clone());
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        let unbounded = observation_id(&core.handle(EngineMsg::Subscribe(query(
            &relay,
            "same",
            Freshness::Live,
        ))));
        let limited =
            observation_id(&core.handle(EngineMsg::Subscribe(limited_query(&relay, "same", 1))));
        assert_eq!(
            wire_ops(&flush(&mut core))
                .into_iter()
                .filter(|op| matches!(op, WireOp::Req(_, _)))
                .count(),
            2
        );
        let requests = core.router.plan().reqs[&session].clone();
        for request in &requests {
            let expected = if request.filter.limit.is_some() {
                limited
            } else {
                unbounded
            };
            let effects = accept_request(
                &mut core,
                &session,
                &request.sub_id,
                request.filter.hash(),
                TransportRelayHandle {
                    slot: 22,
                    generation: 1,
                },
            );
            assert_eq!(
                relay_request_observations(&effects),
                BTreeSet::from([expected])
            );
        }

        let replay_handle = TransportRelayHandle {
            slot: 22,
            generation: 2,
        };
        let replay = core.handle(EngineMsg::RelayConnected(replay_handle, session.clone()));
        assert!(replay.iter().any(|effect| matches!(
            effect,
            Effect::Replay(replay_session, replayed)
                if replay_session == &session && replayed.len() == 2
        )));
        for request in &requests {
            let expected = if request.filter.limit.is_some() {
                limited
            } else {
                unbounded
            };
            let effects = accept_request(
                &mut core,
                &session,
                &request.sub_id,
                request.filter.hash(),
                replay_handle,
            );
            assert_eq!(
                relay_request_observations(&effects),
                BTreeSet::from([expected])
            );
        }

        let (first, second) = if limited_closes_first {
            (limited, unbounded)
        } else {
            (unbounded, limited)
        };
        for observation in [first, second] {
            assert_eq!(
                wire_ops(&core.handle(EngineMsg::Unsubscribe(observation)))
                    .into_iter()
                    .filter(|op| matches!(op, WireOp::Close(_)))
                    .count(),
                1
            );
        }
        assert_eq!(
            core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

#[test]
fn nested_same_demand_boundaries_target_only_wire_participating_scopes() {
    for outer_freshness in [Freshness::CacheOnly, Freshness::MaxAge { seconds: 60 }] {
        for nested_closes_first in [false, true] {
            let relay = RelayUrl::parse("wss://admission-request-target-scope.example").unwrap();
            let session = RelaySessionKey::public(relay.clone());
            let author = Keys::generate();
            let mut store = seeded_profiles(&relay, &[&author]);
            if matches!(outer_freshness, Freshness::MaxAge { .. }) {
                store
                    .record_coverage(&[(
                        profile_atom(&relay, author.public_key()),
                        relay.clone(),
                        CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
                    )])
                    .expect("seed fresh outer coverage");
            }
            let mut core = EngineCore::new(store, 20);
            core.handle(EngineMsg::Tick(Timestamp::from(100u64)));

            let nested_query = || {
                nested_same_profile_query(
                    &relay,
                    Binding::Literal(BTreeSet::from([author.public_key().to_hex()])),
                    outer_freshness,
                )
            };
            let nested = observation_id(&core.handle(EngineMsg::Subscribe(nested_query())));
            let plain = observation_id(&core.handle(EngineMsg::Subscribe(profile_query(
                &relay,
                author.public_key(),
            ))));
            assert_eq!(
                wire_ops(&flush(&mut core))
                    .into_iter()
                    .filter(|op| matches!(op, WireOp::Req(_, _)))
                    .count(),
                1,
                "the two live owners share their one exact physical request"
            );
            let immutable_request = core.router.plan().reqs[&session][0].clone();
            assert_eq!(
                relay_request_targets(&accept_first_request(&mut core, &session, 23)),
                BTreeSet::from([
                    (nested, "$.authors.inner".to_string(), 1),
                    (plain, "$".to_string(), 1),
                ]),
                "the CacheOnly/covered outer path must not alias its Live inner DemandKey"
            );

            let (first, survivor) = if nested_closes_first {
                (nested, plain)
            } else {
                (plain, nested)
            };
            assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(first))).is_empty());
            let reopened_query = if nested_closes_first {
                nested_query()
            } else {
                profile_query(&relay, author.public_key())
            };
            let reopened = observation_id(&core.handle(EngineMsg::Subscribe(reopened_query)));
            assert!(
                wire_ops(&core.handle(EngineMsg::Unsubscribe(survivor))).is_empty(),
                "reactivating one exact covered scope keeps the incumbent request alive"
            );
            assert_eq!(
                core.router.plan().reqs[&session],
                vec![immutable_request.clone()],
                "scope churn never rewrites the immutable sent request"
            );

            let replay_handle = TransportRelayHandle {
                slot: 23,
                generation: 2,
            };
            let replay = core.handle(EngineMsg::RelayConnected(replay_handle, session.clone()));
            assert!(replay.iter().any(|effect| matches!(
                effect,
                Effect::Replay(replay_session, replayed)
                    if replay_session == &session && replayed == &vec![immutable_request.clone()]
            )));
            let expected_path = if nested_closes_first {
                "$.authors.inner"
            } else {
                "$"
            };
            assert_eq!(
                relay_request_targets(&accept_request(
                    &mut core,
                    &session,
                    &immutable_request.sub_id,
                    immutable_request.filter.hash(),
                    replay_handle,
                )),
                BTreeSet::from([(reopened, expected_path.to_string(), 1)]),
                "replay targets only the reopened wire-participating occurrence"
            );
            assert_eq!(
                wire_ops(&core.handle(EngineMsg::Unsubscribe(reopened)))
                    .into_iter()
                    .filter(|op| matches!(op, WireOp::Close(_)))
                    .count(),
                1
            );
            assert_eq!(
                core.bench_ownership_census(),
                CoreOwnershipCensus::default()
            );
        }
    }
}

#[test]
fn reactive_nested_same_demand_replaces_only_the_live_scope_target_revision() {
    for outer_freshness in [Freshness::CacheOnly, Freshness::MaxAge { seconds: 60 }] {
        let relay =
            RelayUrl::parse("wss://admission-request-target-scope-revision.example").unwrap();
        let session = RelaySessionKey::public(relay.clone());
        let account_a = Keys::generate();
        let account_b = Keys::generate();
        let mut store = seeded_profiles(&relay, &[&account_a, &account_b]);
        if matches!(outer_freshness, Freshness::MaxAge { .. }) {
            store
                .record_coverage(&[(
                    profile_atom(&relay, account_a.public_key()),
                    relay.clone(),
                    CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
                )])
                .expect("seed fresh outer coverage");
        }
        let mut core = EngineCore::new(store, 20);
        core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
        core.handle(EngineMsg::SetActivePubkey(Some(account_a.public_key())));
        let observation =
            observation_id(&core.handle(EngineMsg::Subscribe(nested_same_profile_query(
                &relay,
                Binding::Reactive(IdentityField::ActivePubkey),
                outer_freshness,
            ))));
        let handle = core.observations[&observation].branches[0];

        let switched = core.handle(EngineMsg::SetActivePubkey(Some(account_b.public_key())));
        assert_eq!(core.request_targets.declared_for_handle(handle).len(), 2);
        assert!(core
            .request_targets
            .declared_for_handle(handle)
            .keys()
            .all(|target| target.revision == 2));
        let active_targets = core.request_targets.live_targets();
        assert_eq!(active_targets.len(), 1);
        assert_eq!(active_targets[0].handle, handle);
        assert_eq!(active_targets[0].path, "$.authors.inner");
        assert_eq!(active_targets[0].revision, 2);

        let switched_requests = wire_ops(&switched)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count();
        let flushed = flush(&mut core);
        let flushed_requests = wire_ops(&flushed)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count();
        assert_eq!(switched_requests + flushed_requests, 1);
        assert_eq!(
            relay_request_targets(&accept_first_request(&mut core, &session, 24)),
            BTreeSet::from([(observation, "$.authors.inner".to_string(), 2)]),
            "the stale root target is replaced, but its frozen non-wire scope never activates"
        );
        assert_eq!(
            wire_ops(&core.handle(EngineMsg::Unsubscribe(observation)))
                .into_iter()
                .filter(|op| matches!(op, WireOp::Close(_)))
                .count(),
            1
        );
        assert_eq!(
            core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}
