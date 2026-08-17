//! completion transfer admission proofs.

use super::*;
use nmp_store::testing;

#[test]
#[ignore = "known violation #1341: split physical requests do not yet aggregate one wide logical coverage proof"]
fn split_request_pieces_commit_wide_coverage_only_after_every_piece_finishes() {
    let relay = RelayUrl::parse("wss://split-request-coverage.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let alice = Keys::generate().public_key().to_hex();
    let bob = Keys::generate().public_key().to_hex();
    let carol = Keys::generate().public_key().to_hex();
    let atom = |authors: BTreeSet<String>| ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(authors),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let whole = atom(BTreeSet::from([alice.clone(), bob.clone(), carol.clone()]));
    let incumbent_piece = atom(BTreeSet::from([alice, bob]));
    let residual_piece = atom(BTreeSet::from([carol]));
    let whole_claim = coverage_key(&whole);
    let incumbent_claim = coverage_key(&incumbent_piece);
    let residual_claim = coverage_key(&residual_piece);
    let owner = DemandKey::for_atom(&whole);
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    core.set_active_demand(&BTreeSet::from([
        whole.clone(),
        incumbent_piece.clone(),
        residual_piece.clone(),
    ]));
    let transport = TransportRelayHandle {
        slot: 81,
        generation: 1,
    };
    core.white_box("slot_to_relay.insert", |s| {
        s.slot_to_relay
            .insert(transport.slot, (transport, session.clone()))
    });

    let open_piece = |core: &mut EngineCore, piece: &ContextualAtom| {
        let sub_id = SubId::for_wire(relay.clone(), &piece.filter, &piece.routing, piece.access);
        let claim = coverage_key(piece);
        core.white_box("attribution.retain_live_request_claims", |s| {
            s.attribution
                .retain_live_request_claims(&sub_id, BTreeSet::from([claim]))
        });
        core.white_box("record_observed_request", |s| {
            s.record_observed_request(RequestSend {
                session: &session,
                sub_id: &sub_id,
                filter: &piece.filter,
                coverage_claims: BTreeSet::from([claim]),
                owner_demands: BTreeSet::from([owner]),
                lanes: BTreeSet::new(),
                replay: false,
                event_failure_target: EventFailureTarget::ThisSend,
            })
        });
        accept_request(core, &session, &sub_id, piece.filter.hash(), transport);
        sub_id
    };
    let incumbent_sub = open_piece(&mut core, &incumbent_piece);
    let residual_sub = open_piece(&mut core, &residual_piece);

    core.white_box("clock", |s| s.clock = Timestamp::from(180u64));
    core.white_box("on_relay_frame", |s| {
        s.on_relay_frame(
            transport,
            session.clone(),
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                nostr::SubscriptionId::new(wire_sub_id_string(&incumbent_sub)),
            ))),
        )
    });
    assert_eq!(
        core.store.get_coverage(incumbent_claim, &relay).unwrap(),
        Some(CoverageInterval::new(
            Timestamp::from(0),
            Timestamp::from(180)
        ))
    );
    assert_eq!(core.store.get_coverage(whole_claim, &relay).unwrap(), None);

    core.white_box("clock", |s| s.clock = Timestamp::from(200u64));
    core.white_box("on_relay_frame", |s| {
        s.on_relay_frame(
            transport,
            session.clone(),
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                nostr::SubscriptionId::new(wire_sub_id_string(&residual_sub)),
            ))),
        )
    });
    assert_eq!(
        core.store.get_coverage(residual_claim, &relay).unwrap(),
        Some(CoverageInterval::new(
            Timestamp::from(0),
            Timestamp::from(200)
        ))
    );
    assert_eq!(
        core.store.get_coverage(whole_claim, &relay).unwrap(),
        Some(CoverageInterval::new(
            Timestamp::from(0),
            Timestamp::from(180)
        )),
        "only the complete fragment set may mint the wide logical coverage proof"
    );

    for sub_id in [incumbent_sub, residual_sub] {
        core.white_box("live_wire_requests.remove", |s| {
            s.live_wire_requests
                .remove(&(session.clone(), sub_id.clone()))
        });
        core.white_box("retire_plan_execution_metadata", |s| {
            s.retire_plan_execution_metadata(&sub_id)
        });
        core.white_box("abandon_sub", |s| s.abandon_sub(&sub_id));
    }
    core.white_box("slot_to_relay.remove", |s| {
        s.slot_to_relay.remove(&transport.slot)
    });
    core.set_active_demand(&BTreeSet::new());
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn replacement_and_close_cancel_the_exact_pending_post_eose_transfer() {
    for replacement in [false, true] {
        let relay = RelayUrl::parse("wss://post-eose-transfer-cancel.example").unwrap();
        let session = RelaySessionKey::public(relay.clone());
        let incumbent = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1, 2])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![relay.clone()]),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let added = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![relay.clone()]),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let incumbent_claim = coverage_key(&incumbent);
        let added_claim = coverage_key(&added);
        let directory = tempfile::tempdir().expect("coverage corruption directory");
        let path = directory.path().join("pending-transfer-cancel.redb");
        {
            let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
            store
                .record_coverage(&[(
                    added.clone(),
                    relay.clone(),
                    CoverageInterval::new(Timestamp::from(0), Timestamp::from(99)),
                )])
                .expect("seed exact coverage row");
        }
        testing::corrupt_coverage(&path, added_claim, &relay)
            .expect("store-owned coverage corruption");
        let mut core = EngineCore::new(
            RedbStore::open(&path).expect("reopen corrupted Redb fixture"),
            20,
        );
        core.set_active_demand(&BTreeSet::from([incumbent.clone(), added.clone()]));
        let sub_id = SubId::for_wire(
            relay,
            &incumbent.filter,
            &incumbent.routing,
            incumbent.access,
        );
        core.white_box("attribution.retain_live_request_claims", |s| {
            s.attribution
                .retain_live_request_claims(&sub_id, BTreeSet::from([incumbent_claim]))
        });
        core.white_box("live_wire_requests.insert", |s| {
            s.live_wire_requests.insert(
                (session.clone(), sub_id.clone()),
                LiveWireRequest {
                    filter: incumbent.filter.clone(),
                    evidence_sub_id: sub_id.clone(),
                    handle: TransportRelayHandle {
                        slot: 78,
                        generation: 1,
                    },
                    stored_events: super::observation::StoredEvents::Finished {
                        request_revision: 10,
                        committed_interval: Some(CoverageInterval::new(
                            Timestamp::from(100),
                            Timestamp::from(200),
                        )),
                    },
                    returns: Default::default(),
                },
            )
        });
        core.white_box("apply_request_metadata_updates", |s| {
            s.apply_request_metadata_updates(
                &[nmp_router::RequestMetadataUpdate {
                    session: session.clone(),
                    sub_id: sub_id.clone(),
                    filter_hash: incumbent.filter.hash(),
                    added_coverage_claims: BTreeSet::from([added_claim]),
                    added_owner_demands: BTreeSet::from([DemandKey::for_atom(&added)]),
                }],
                &mut Vec::new(),
            )
        });
        assert_eq!(core.pending_request_claim_transfers.len(), 1);

        let op = if replacement {
            let mut replacement_filter = incumbent.filter.clone();
            replacement_filter.kinds = Some(BTreeSet::from([3]));
            WireOp::Req(sub_id.clone(), replacement_filter)
        } else {
            WireOp::Close(sub_id.clone())
        };
        core.white_box("reconcile_request_claim_transfers_for_wire_delta", |s| {
            s.reconcile_request_claim_transfers_for_wire_delta(&WireDelta {
                ops: vec![(session.clone(), vec![op])],
            })
        });
        assert!(core.pending_request_claim_transfers.is_empty());

        core.white_box("live_wire_requests.remove", |s| {
            s.live_wire_requests.remove(&(session, sub_id.clone()))
        });
        core.white_box("retire_plan_execution_metadata", |s| {
            s.retire_plan_execution_metadata(&sub_id)
        });
        core.set_active_demand(&BTreeSet::new());
        assert_eq!(
            core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

#[test]
fn repeated_same_filter_failed_generations_coalesce_into_one_current_transfer_job() {
    const GENERATIONS: u16 = 1_000;
    let relay = RelayUrl::parse("wss://post-eose-transfer-bounded.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let incumbent = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some((1_000..2_001).collect()),
            since: Some(100),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let incumbent_claim = coverage_key(&incumbent);
    let sub_id = SubId::for_wire(
        relay.clone(),
        &incumbent.filter,
        &incumbent.routing,
        incumbent.access,
    );
    let added_for_generation = |generation: u16| ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1_000 + generation])),
            since: Some(100),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![sub_id.0.clone()]),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let first_added_claim = coverage_key(&added_for_generation(1));
    let directory = tempfile::tempdir().expect("bounded coverage transfer directory");
    let path = directory.path().join("post-eose-transfer-bounded.redb");
    let store = RedbStore::open_with_failed_coverage_write(&path, first_added_claim, relay)
        .expect("persistent exact coverage-write failure fixture");
    let mut core = EngineCore::new(store, 20);
    let added_atoms: Vec<_> = (1..=GENERATIONS).map(added_for_generation).collect();
    let mut demand = BTreeSet::from([incumbent.clone()]);
    demand.extend(added_atoms.iter().cloned());
    core.set_active_demand(&demand);
    core.white_box("attribution.retain_live_request_claims", |s| {
        s.attribution
            .retain_live_request_claims(&sub_id, BTreeSet::from([incumbent_claim]))
    });
    core.white_box("live_wire_requests.insert", |s| {
        s.live_wire_requests.insert(
            (session.clone(), sub_id.clone()),
            LiveWireRequest {
                filter: incumbent.filter.clone(),
                evidence_sub_id: sub_id.clone(),
                handle: TransportRelayHandle {
                    slot: 79,
                    generation: 1,
                },
                returns: Default::default(),
                stored_events: super::observation::StoredEvents::Finished {
                    request_revision: 1,
                    committed_interval: Some(CoverageInterval::new(
                        Timestamp::from(100),
                        Timestamp::from(200),
                    )),
                },
            },
        )
    });

    for (index, added) in added_atoms.iter().enumerate() {
        let generation = index as u16 + 1;
        let claim = coverage_key(added);
        core.white_box("live_wire_requests.get_mut", |s| {
            if let Some(live) = s
                .live_wire_requests
                .get_mut(&(session.clone(), sub_id.clone()))
            {
                live.stored_events = super::observation::StoredEvents::Finished {
                    request_revision: generation as u64,
                    committed_interval: Some(CoverageInterval::new(
                        Timestamp::from(100),
                        Timestamp::from(200 + generation as u64),
                    )),
                };
            }
        });
        core.white_box("apply_request_metadata_updates", |s| {
            s.apply_request_metadata_updates(
                &[nmp_router::RequestMetadataUpdate {
                    session: session.clone(),
                    sub_id: sub_id.clone(),
                    filter_hash: incumbent.filter.hash(),
                    added_coverage_claims: BTreeSet::from([claim]),
                    added_owner_demands: BTreeSet::from([DemandKey::for_atom(added)]),
                }],
                &mut Vec::new(),
            )
        });
    }

    assert_eq!(core.pending_request_claim_transfers.len(), 1);
    let pending = core
        .pending_request_claim_transfers
        .values()
        .next()
        .unwrap();
    assert_eq!(pending.request_revision, GENERATIONS as u64);
    assert_eq!(pending.claims.len(), GENERATIONS as usize);
    assert_eq!(pending.interval.through, Timestamp::from(1_200u64));
    assert_eq!(core.request_claim_transfer_attempts.get(), 1);
    assert_eq!(core.request_claim_transfer_claims_attempted.get(), 1);
    assert_eq!(core.request_claim_transfer_failures.get(), 1);

    core.white_box("retry_scheduler_blocked", |s| {
        s.retry_scheduler_blocked = true
    });
    let due = core
        .next_deadline()
        .unwrap()
        .expect("the one accumulated transfer owns one retry deadline");
    core.tick(due);
    assert!(core.pending_request_claim_transfers.is_empty());
    assert_eq!(core.request_claim_transfer_attempts.get(), 2);
    assert_eq!(
        core.request_claim_transfer_claims_attempted.get(),
        u64::from(GENERATIONS) + 1
    );
    assert_eq!(core.request_claim_transfer_failures.get(), 1);
    assert_eq!(core.request_claim_transfer_commits.get(), 1);

    core.white_box("reconcile_request_claim_transfers_for_wire_delta", |s| {
        s.reconcile_request_claim_transfers_for_wire_delta(&WireDelta {
            ops: vec![(session.clone(), vec![WireOp::Close(sub_id.clone())])],
        })
    });
    core.white_box("live_wire_requests.remove", |s| {
        s.live_wire_requests.remove(&(session, sub_id.clone()))
    });
    core.white_box("retire_plan_execution_metadata", |s| {
        s.retire_plan_execution_metadata(&sub_id)
    });
    core.set_active_demand(&BTreeSet::new());
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
