//! Admission-window and surgical lifecycle falsifiers for #1341/#1342.

use super::*;
use nmp_grammar::{
    Binding, ConcreteFilter, ContextualAtom, Demand, Derived, Filter, IdentityField,
    IndexedTagName, Selector,
};
use nmp_router::DemandKey;
use nmp_store::{coverage_key, CoverageInterval, RedbStore, RelayObserved};
use nostr::{EventBuilder, Keys, Kind};
use std::borrow::Cow;

fn query(relay: &RelayUrl, value: &str, freshness: Freshness) -> LiveQuery {
    let mut demand = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        tags: BTreeMap::from([(
            IndexedTagName::new('p').unwrap(),
            Binding::Literal(BTreeSet::from([value.to_owned()])),
        )]),
        ..Filter::default()
    });
    demand.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    demand.freshness = freshness;
    LiveQuery::single(demand)
}

fn bounded_query(relay: &RelayUrl, value: &str) -> LiveQuery {
    limited_query(relay, value, 25)
}

fn limited_query(relay: &RelayUrl, value: &str, limit: usize) -> LiveQuery {
    let mut demand = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        tags: BTreeMap::from([(
            IndexedTagName::new('p').unwrap(),
            Binding::Literal(BTreeSet::from([value.to_owned()])),
        )]),
        limit: Some(limit),
        ..Filter::default()
    });
    demand.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    demand.freshness = Freshness::Live;
    LiveQuery::single(demand)
}

fn bounded_atom(relay: &RelayUrl, value: &str) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([0u16])),
            tags: BTreeMap::from([(
                IndexedTagName::new('p').unwrap(),
                BTreeSet::from([value.to_owned()]),
            )]),
            limit: Some(25),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn query_atom(relay: &RelayUrl, value: &str) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([0u16])),
            tags: BTreeMap::from([(
                IndexedTagName::new('p').unwrap(),
                BTreeSet::from([value.to_owned()]),
            )]),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn unbounded_incompatible_query(relay: &RelayUrl, index: u16) -> LiveQuery {
    let mut demand = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([1_000 + index])),
        tags: BTreeMap::from([(
            IndexedTagName::new('p').unwrap(),
            Binding::Literal(BTreeSet::from([format!("owner-{index:04}")])),
        )]),
        ..Filter::default()
    });
    demand.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    demand.freshness = Freshness::Live;
    LiveQuery::single(demand)
}

fn profile_query(relay: &RelayUrl, author: PublicKey) -> LiveQuery {
    let mut demand = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(Binding::Literal(BTreeSet::from([author.to_hex()]))),
        ..Filter::default()
    });
    demand.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    demand.freshness = Freshness::Live;
    LiveQuery::single(demand)
}

fn nested_same_profile_query(
    relay: &RelayUrl,
    inner_authors: Binding,
    outer_freshness: Freshness,
) -> LiveQuery {
    let mut inner = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(inner_authors),
        ..Filter::default()
    });
    inner.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    inner.freshness = Freshness::Live;

    let mut outer = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner,
            project: Selector::Authors,
        }))),
        ..Filter::default()
    });
    outer.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    outer.freshness = outer_freshness;
    LiveQuery::single(outer)
}

fn profile_atom(relay: &RelayUrl, author: PublicKey) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([0u16])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn seeded_profiles(relay: &RelayUrl, authors: &[&Keys]) -> RedbStore {
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    for (index, author) in authors.iter().enumerate() {
        let observed_at = 10 + index as u64;
        store
            .insert(
                EventBuilder::new(Kind::Metadata, "{}")
                    .custom_created_at(Timestamp::from(observed_at))
                    .sign_with_keys(author)
                    .expect("sign fixture profile"),
                RelayObserved::new(relay.clone(), Timestamp::from(observed_at)),
            )
            .expect("seed fixture profile");
    }
    store
}

fn routeless_outbox_query(author: PublicKey) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([1u16])),
                authors: Some(Binding::Literal(BTreeSet::from([author.to_hex()]))),
                ..Filter::default()
            },
            SourceAuthority::AuthorOutboxes,
            AccessContext::Public,
        )
        .unwrap(),
    )
}

fn routeless_outbox_atom(author: PublicKey) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn observation_id(effects: &[Effect]) -> ObservationId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .expect("an observation open returns its immediate cache seed")
}

fn source_statuses(effects: &[Effect], observation: ObservationId) -> Vec<SourceStatus> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(id, _, evidence) if *id == observation => Some(evidence),
            _ => None,
        })
        .flat_map(|evidence| evidence.iter())
        .flat_map(|evidence| evidence.sources.iter())
        .map(|source| source.status)
        .collect()
}

fn current_source_statuses(core: &EngineCore, observation: ObservationId) -> Vec<SourceStatus> {
    core.observations[&observation]
        .last_evidence
        .as_ref()
        .into_iter()
        .flatten()
        .flat_map(|evidence| evidence.sources.iter())
        .map(|source| source.status)
        .collect()
}

#[test]
fn fresh_max_age_is_coverage_satisfied_alone_and_never_borrows_live_placement() {
    let relay = RelayUrl::parse("wss://max-age-evidence.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let value = "fresh-owner";
    let atom = query_atom(&relay, value);
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .record_coverage(&[(
            atom,
            relay.clone(),
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
        )])
        .unwrap();
    let mut core = EngineCore::new(store, 8);
    core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
    let handle = TransportRelayHandle {
        slot: 31,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));

    let max_opened = core.handle(EngineMsg::Subscribe(query(
        &relay,
        value,
        Freshness::MaxAge { seconds: 60 },
    )));
    let max = observation_id(&max_opened);
    assert_eq!(
        source_statuses(&max_opened, max),
        vec![SourceStatus::CoverageSatisfied]
    );
    assert!(wire_ops(&flush(&mut core)).is_empty());

    let live_opened = core.handle(EngineMsg::Subscribe(query(&relay, value, Freshness::Live)));
    let live = observation_id(&live_opened);
    let admitted = flush(&mut core);
    assert_eq!(
        current_source_statuses(&core, max),
        vec![SourceStatus::CoverageSatisfied]
    );
    assert_eq!(
        source_statuses(&admitted, live),
        vec![SourceStatus::AwaitingRequest]
    );
    let accepted = accept_first_request(&mut core, &session, handle.slot);
    assert_eq!(
        current_source_statuses(&core, max),
        vec![SourceStatus::CoverageSatisfied]
    );
    assert_eq!(
        source_statuses(&accepted, live),
        vec![SourceStatus::Requesting]
    );

    assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(max))).is_empty());
    assert_eq!(
        wire_ops(&core.handle(EngineMsg::Unsubscribe(live)))
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

fn wire_ops(effects: &[Effect]) -> Vec<&WireOp> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter().flat_map(|(_, ops)| ops))
        .collect()
}

fn flush(core: &mut EngineCore) -> Vec<Effect> {
    core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)))
}

fn current_attempt(
    core: &EngineCore,
    session: &RelaySessionKey,
    sub_id: &SubId,
    filter_hash: DescriptorHash,
) -> RequestAttemptId {
    core.pending_request_evidence[&(session.clone(), sub_id.clone())]
        .iter()
        .rev()
        .find(|request| request.filter.hash() == filter_hash)
        .expect("the test request owns a current attempt")
        .attempt_id
}

fn accept_request(
    core: &mut EngineCore,
    session: &RelaySessionKey,
    sub_id: &SubId,
    filter_hash: DescriptorHash,
    handle: TransportRelayHandle,
) -> Vec<Effect> {
    let attempt_id = current_attempt(core, session, sub_id, filter_hash);
    core.on_wire_request_handoff(RequestHandoffOutcome::Accepted { attempt_id, handle })
}

fn refuse_request(
    core: &mut EngineCore,
    session: &RelaySessionKey,
    sub_id: &SubId,
    filter_hash: DescriptorHash,
) -> Vec<Effect> {
    let attempt_id = current_attempt(core, session, sub_id, filter_hash);
    core.on_wire_request_handoff(RequestHandoffOutcome::Refused {
        attempt_id,
        cause: LocalSendRefusal::SessionUnavailable,
    })
}

fn accept_first_request(
    core: &mut EngineCore,
    session: &RelaySessionKey,
    slot: u32,
) -> Vec<Effect> {
    let request = core.router.plan().reqs[session][0].clone();
    accept_request(
        core,
        session,
        &request.sub_id,
        request.filter.hash(),
        TransportRelayHandle {
            slot,
            generation: 1,
        },
    )
}

fn relay_request_observations(effects: &[Effect]) -> BTreeSet<ObservationId> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitObservationEvidence(observation, evidence)
                if evidence
                    .iter()
                    .any(|fact| matches!(fact.fact, ObservationFact::RelayRequest { .. })) =>
            {
                Some(*observation)
            }
            _ => None,
        })
        .collect()
}

fn relay_request_targets(effects: &[Effect]) -> BTreeSet<(ObservationId, String, u64)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitObservationEvidence(observation, evidence) => {
                Some((*observation, evidence.as_slice()))
            }
            _ => None,
        })
        .flat_map(|(observation, evidence)| {
            evidence.iter().filter_map(move |fact| match &fact.fact {
                ObservationFact::RelayRequest {
                    path,
                    filter_revision,
                    ..
                } => Some((observation, path.clone(), *filter_revision)),
                _ => None,
            })
        })
        .collect()
}

#[path = "admission_tests/attach_ordering.rs"]
mod attach_ordering;
#[path = "admission_tests/author_route_needs_rebuild_agreement.rs"]
mod author_route_needs_rebuild_agreement;
#[path = "admission_tests/claim_detach.rs"]
mod claim_detach;
#[path = "admission_tests/claim_transfer_retry.rs"]
mod claim_transfer_retry;
#[path = "admission_tests/clock.rs"]
mod clock;
#[path = "admission_tests/cohort.rs"]
mod cohort;
#[path = "admission_tests/completion_transfer.rs"]
mod completion_transfer;
#[path = "admission_tests/diagnostics_scale.rs"]
mod diagnostics_scale;
#[path = "admission_tests/execution_targets.rs"]
mod execution_targets;
#[path = "admission_tests/lifecycle.rs"]
mod lifecycle;
#[path = "admission_tests/plan_index_mirror.rs"]
mod plan_index_mirror;
#[path = "admission_tests/request_filter_sharing.rs"]
mod request_filter_sharing;
#[path = "admission_tests/resolver_delta.rs"]
mod resolver_delta;
#[path = "admission_tests/routing_evidence.rs"]
mod routing_evidence;
#[path = "admission_tests/scale_teardown.rs"]
mod scale_teardown;
#[path = "admission_tests/stalled_write_census_freshness.rs"]
mod stalled_write_census_freshness;
#[path = "admission_tests/wire_rebuild_agreement.rs"]
mod wire_rebuild_agreement;
