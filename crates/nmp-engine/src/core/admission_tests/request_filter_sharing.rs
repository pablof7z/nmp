//! Grouped-request execution-evidence allocation proofs.

use super::*;
use std::sync::Arc;

#[test]
fn grouped_handoff_shares_one_immutable_filter_across_every_observation_fact() {
    const OWNERS: usize = 207;
    let relay = RelayUrl::parse("wss://shared-request-filter.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let observations = (0..OWNERS)
        .map(|index| {
            observation_id(&core.handle(EngineMsg::Subscribe(query(
                &relay,
                &format!("owner-{index:03}"),
                Freshness::Live,
            ))))
        })
        .collect::<Vec<_>>();

    let admitted = flush(&mut core);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|operation| matches!(operation, WireOp::Req(_, _)))
            .count(),
        1
    );
    let accepted = accept_first_request(&mut core, &session, 81);
    let filters = accepted
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitObservationEvidence(_, evidence) => Some(evidence),
            _ => None,
        })
        .flatten()
        .filter_map(|evidence| match &evidence.fact {
            ObservationFact::RelayRequest { filter, .. } => Some(filter),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(filters.len(), OWNERS);
    assert!(filters.iter().all(|filter| Arc::ptr_eq(filters[0], filter)));

    for observation in observations {
        core.handle(EngineMsg::Unsubscribe(observation));
    }
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
