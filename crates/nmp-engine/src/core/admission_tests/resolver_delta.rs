//! resolver delta admission proofs.

use super::*;

#[test]
fn one_handle_partial_resolver_closes_touch_only_departing_refcounts_in_both_orders() {
    const ATOMS: usize = 10_000;
    for reverse in [false, true] {
        let relay = RelayUrl::parse("wss://partial-resolver-owner.example").unwrap();
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        let observation = observation_id(&core.handle(EngineMsg::Subscribe(query(
            &relay,
            "shared-owner",
            Freshness::Live,
        ))));
        let handle = core.observations[&observation].branches[0];
        let mut atoms = core.wire.atoms_for_handle(handle);
        let base = atoms
            .iter()
            .next()
            .cloned()
            .expect("the live observation owns one base atom");
        let demand = DemandKey::for_atom(&base);
        let claim = coverage_key(&base);
        let mut departing = Vec::with_capacity(ATOMS - 1);
        for index in 1..ATOMS {
            let mut atom = base.clone();
            atom.routing_evidence.insert(RoutingEvidence {
                relay: RelayUrl::parse(&format!("wss://partial-resolver-{index:05}.example"))
                    .unwrap(),
                origin: nmp_grammar::RoutingEvidenceKind::Hint,
            });
            assert!(atoms.insert(atom.clone()));
            departing.push(atom);
        }
        // Build the fixture through the owner's own doors: indexing derives
        // the per-handle refcounts, and one retain per added atom derives the
        // owner count. Both then equal ATOMS because every atom here shares
        // one DemandKey and one claim -- a state the production path reaches,
        // not one assigned into the maps. `index_handle` refuses re-indexing
        // an already-indexed handle (#1774), so the door sequence is spelled
        // out: unindex, then index the grown set. Nothing needs releasing --
        // `base` is still present in `atoms`, only `departing` is new.
        let previous = core.white_box("wire.unindex_handle", |s| s.wire.unindex_handle(handle));
        debug_assert!(
            previous.is_subset(&atoms),
            "this fixture only grows the handle's atom set"
        );
        core.white_box("wire.index_handle", |s| s.wire.index_handle(handle, atoms));
        for atom in &departing {
            core.white_box("wire.retain", |s| s.wire.retain(atom));
        }
        assert_eq!(core.wire.demand_refs(handle, &demand), ATOMS);
        assert_eq!(core.wire.coverage_refs(handle, &claim), ATOMS);

        if reverse {
            departing.reverse();
        }
        core.resolver_delta_ops_consumed.set(0);
        core.resolver_owner_keys_touched.set(0);
        core.resolver_surviving_atoms_examined.set(0);
        for atom in departing {
            core.white_box("consume_resolver_delta", |s| {
                s.consume_resolver_delta(DemandDelta {
                    ops: vec![DemandOp::Close(atom)],
                })
            });
        }

        assert_eq!(core.resolver_delta_ops_consumed.get(), (ATOMS - 1) as u64);
        assert_eq!(
            core.resolver_owner_keys_touched.get(),
            2 * (ATOMS - 1) as u64
        );
        assert_eq!(core.resolver_surviving_atoms_examined.get(), 0);
        assert_eq!(core.wire.demand_refs(handle, &demand), 1);
        assert_eq!(core.wire.coverage_refs(handle, &claim), 1);
        assert!(core
            .request_targets
            .declared_live_for_demand(&demand)
            .keys()
            .any(|target| target.handle == handle));

        core.handle(EngineMsg::Unsubscribe(observation));
        assert_eq!(
            core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

#[test]
fn one_handle_partial_close_preserves_only_the_distinct_surviving_request_target() {
    for terminal in ["accepted", "refused", "eose"] {
        let relay = RelayUrl::parse("wss://partial-resolver-distinct.example").unwrap();
        let session = RelaySessionKey::public(relay.clone());
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        let observation =
            observation_id(&core.handle(EngineMsg::Subscribe(bounded_query(&relay, "departing"))));
        let handle = core.observations[&observation].branches[0];
        let mut atoms = core.wire.atoms_for_handle(handle);
        let departing = atoms.iter().next().cloned().unwrap();
        let departing_demand = DemandKey::for_atom(&departing);
        let surviving = bounded_atom(&relay, "surviving");
        let surviving_demand = DemandKey::for_atom(&surviving);
        let surviving_claim = coverage_key(&surviving);

        core.white_box("deactivate_request_targets_for_handle", |s| {
            s.deactivate_request_targets_for_handle(handle)
        });
        let departing_target = core
            .request_targets
            .declared_for_handle(handle)
            .keys()
            .next()
            .cloned()
            .unwrap();
        core.white_box("request_targets.declare_for_handle", |s| {
            s.request_targets.declare_for_handle(
                handle,
                ActiveRequestTarget {
                    demand: surviving_demand,
                    scope: departing_target.scope,
                    path: "$.surviving".to_string(),
                    revision: departing_target.revision,
                },
                1,
                None,
            )
        });
        atoms.insert(surviving.clone());
        // `index_handle` refuses re-indexing an already-indexed handle
        // (#1774); the door sequence is spelled out here too. Nothing needs
        // releasing -- `departing` is still present in `atoms`, only
        // `surviving` is new.
        let previous = core.white_box("wire.unindex_handle", |s| s.wire.unindex_handle(handle));
        debug_assert!(
            previous.is_subset(&atoms),
            "this fixture only grows the handle's atom set"
        );
        core.white_box("wire.index_handle", |s| s.wire.index_handle(handle, atoms));
        core.white_box("retain_wire_atom_owner", |s| {
            s.retain_wire_atom_owner(&surviving)
        });
        core.white_box("activate_request_targets_for_handle", |s| {
            s.activate_request_targets_for_handle(handle)
        });

        core.white_box("consume_resolver_delta", |s| {
            s.consume_resolver_delta(DemandDelta {
                ops: vec![DemandOp::Close(departing)],
            })
        });
        let mut close_effects = Vec::new();
        core.white_box("flush_consumed_resolver_closes", |s| {
            s.flush_consumed_resolver_closes(&mut close_effects)
        });
        assert!(!core.request_targets.has_live_demand(&departing_demand));
        assert_eq!(core.request_targets.live_target_count(&surviving_demand), 1);

        let sub_id = SubId::for_wire(
            relay,
            &surviving.filter,
            &surviving.routing,
            surviving.access,
        );
        core.white_box("record_observed_request", |s| {
            s.record_observed_request(RequestSend {
                session: &session,
                sub_id: &sub_id,
                filter: &surviving.filter,
                coverage_claims: BTreeSet::from([surviving_claim]),
                owner_demands: BTreeSet::from([surviving_demand]),
                lanes: BTreeSet::new(),
                replay: false,
                event_failure_target: EventFailureTarget::ThisSend,
            })
        });
        let handoff = if terminal == "refused" {
            refuse_request(&mut core, &session, &sub_id, surviving.filter.hash())
        } else {
            accept_request(
                &mut core,
                &session,
                &sub_id,
                surviving.filter.hash(),
                TransportRelayHandle {
                    slot: 41,
                    generation: 1,
                },
            )
        };
        let handoff_paths: BTreeSet<_> = handoff
            .iter()
            .filter_map(|effect| match effect {
                Effect::EmitObservationEvidence(_, evidence) => Some(evidence),
                _ => None,
            })
            .flatten()
            .filter_map(|evidence| match &evidence.fact {
                ObservationFact::RelayRequest { path, .. }
                | ObservationFact::RequestDeferred { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(handoff_paths, BTreeSet::from(["$.surviving".to_string()]));

        if terminal == "eose" {
            let completed = core.white_box("attribution.attribute_eose_detailed", |s| {
                s.attribution
                    .attribute_eose_detailed(
                        &session,
                        &wire_sub_id_string(&sub_id),
                        Timestamp::from(101u64),
                    )
                    .unwrap()
            });
            let mut settled = Vec::new();
            core.white_box("emit_request_settled", |s| {
                s.emit_request_settled(
                    completed.send_id(),
                    Timestamp::from(101u64),
                    RequestTerminal::Eose,
                    &mut settled,
                )
            });
            let settled_paths: BTreeSet<_> = settled
                .iter()
                .filter_map(|effect| match effect {
                    Effect::EmitObservationEvidence(_, evidence) => Some(evidence),
                    _ => None,
                })
                .flatten()
                .filter_map(|evidence| match &evidence.fact {
                    ObservationFact::RequestSettled { path, .. } => Some(path.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(settled_paths, BTreeSet::from(["$.surviving".to_string()]));
        }

        core.white_box("abandon_sub", |s| s.abandon_sub(&sub_id));
        core.handle(EngineMsg::Unsubscribe(observation));
        assert_eq!(
            core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

#[test]
fn one_added_request_claim_never_revisits_ten_thousand_incumbent_live_claims() {
    let relay = RelayUrl::parse("wss://core-metadata-delta-10k.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let mut atoms = Vec::with_capacity(10_001);
    let mut incumbent_claims = BTreeSet::new();
    for kind in 10_000..20_000 {
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([kind])),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![relay.clone()]),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        incumbent_claims.insert(coverage_key(&atom));
        atoms.push(atom);
    }
    let added_atom = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([20_000])),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let added_claim = coverage_key(&added_atom);
    atoms.push(added_atom.clone());
    core.set_active_demand(&atoms.iter().cloned().collect());

    let request_atom = atoms[0].clone();
    let sub_id = SubId::for_wire(
        relay,
        &request_atom.filter,
        &request_atom.routing,
        request_atom.access,
    );
    core.white_box("attribution.retain_live_request_claims", |s| {
        s.attribution
            .retain_live_request_claims(&sub_id, incumbent_claims.clone())
    });
    core.white_box("record_observed_request", |s| {
        s.record_observed_request(RequestSend {
            session: &session,
            sub_id: &sub_id,
            filter: &request_atom.filter,
            coverage_claims: incumbent_claims,
            owner_demands: BTreeSet::from([DemandKey::for_atom(&request_atom)]),
            lanes: BTreeSet::new(),
            replay: false,
            event_failure_target: EventFailureTarget::ThisSend,
        })
    });

    core.request_claim_entries_examined.set(0);
    core.request_owner_entries_examined.set(0);
    let mut effects = Vec::new();
    core.white_box("apply_request_metadata_updates", |s| {
        s.apply_request_metadata_updates(
            &[nmp_router::RequestMetadataUpdate {
                session,
                sub_id: sub_id.clone(),
                filter_hash: request_atom.filter.hash(),
                added_coverage_claims: BTreeSet::from([added_claim]),
                added_owner_demands: BTreeSet::from([DemandKey::for_atom(&added_atom)]),
            }],
            &mut effects,
        )
    });
    assert!(effects.is_empty());
    assert_eq!(core.request_claim_entries_examined.get(), 1);
    assert_eq!(core.request_owner_entries_examined.get(), 1);

    core.white_box("retire_plan_execution_metadata", |s| {
        s.retire_plan_execution_metadata(&sub_id)
    });
    core.white_box("abandon_sub", |s| s.abandon_sub(&sub_id));
    core.set_active_demand(&BTreeSet::new());
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
