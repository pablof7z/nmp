use std::collections::BTreeSet;

use nmp_grammar::{
    AccessContext, Binding, ConcreteFilter, ContextualAtom, Demand, Filter, Freshness, LiveQuery,
    SourceAuthority,
};
use nmp_store::{CoverageInterval, EventStore, RedbStore};
use nostr::{Keys, RelayUrl, Timestamp};

use super::{Effect, EngineCore, EngineMsg, SourceStatus};

#[test]
fn fresh_max_age_reads_each_coverage_row_once() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://freshness-snapshot.example").unwrap();
    let filter = Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(Binding::Literal(BTreeSet::from([keys
            .public_key()
            .to_hex()]))),
        ..Filter::default()
    };
    let atom = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([0u16])),
            authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .record_coverage(&[(
            atom,
            relay.clone(),
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(99_000u64)),
        )])
        .unwrap();
    let mut core = EngineCore::new(store, 8);
    core.handle(EngineMsg::Tick(Timestamp::from(100_000u64)));
    core.store.reset_coverage_reads();

    let mut demand = Demand::new(
        filter,
        SourceAuthority::Pinned(BTreeSet::from([relay])),
        AccessContext::Public,
    )
    .unwrap();
    demand.freshness = Freshness::MaxAge { seconds: 3_600 };
    let effects = core.handle(EngineMsg::Subscribe(LiveQuery::single(demand)));

    assert_eq!(core.store.coverage_reads(), 1);
    let evidence = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, _, evidence) => Some(evidence),
            _ => None,
        })
        .expect("a fresh observation emits its opening evidence");
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].sources.len(), 1);
    assert_eq!(
        evidence[0].sources[0].status,
        SourceStatus::CoverageSatisfied
    );
    assert_eq!(
        evidence[0].sources[0].reconciled_through,
        Some(Timestamp::from(99_000u64))
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission | Effect::Wire(_))));
}

fn pinned_profile_query(author: &str, relay: RelayUrl, freshness: Freshness) -> LiveQuery {
    let filter = Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(Binding::Literal(BTreeSet::from([author.to_owned()]))),
        ..Filter::default()
    };
    let mut demand = Demand::new(
        filter,
        SourceAuthority::Pinned(BTreeSet::from([relay])),
        AccessContext::Public,
    )
    .unwrap();
    demand.freshness = freshness;
    LiveQuery::single(demand)
}

#[test]
fn max_age_opening_retains_only_its_scoped_candidate_plan() {
    const INCUMBENTS: usize = 207;
    let candidate_keys = Keys::generate();
    let candidate_relay = RelayUrl::parse("wss://freshness-candidate.example").unwrap();
    let candidate_atom = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([0u16])),
            authors: Some(BTreeSet::from([candidate_keys.public_key().to_hex()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([candidate_relay.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .record_coverage(&[(
            candidate_atom,
            candidate_relay.clone(),
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(99_000u64)),
        )])
        .unwrap();
    let mut core = EngineCore::new(store, INCUMBENTS + 8);
    core.handle(EngineMsg::Tick(Timestamp::from(100_000u64)));

    let mut incumbent_observations = Vec::with_capacity(INCUMBENTS);
    for index in 0..INCUMBENTS {
        let relay = RelayUrl::parse(&format!("wss://incumbent-{index}.example")).unwrap();
        let author = Keys::generate().public_key().to_hex();
        let effects = core.handle(EngineMsg::Subscribe(pinned_profile_query(
            &author,
            relay,
            Freshness::Live,
        )));
        incumbent_observations.push(
            effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::EmitRows(observation, _, _) => Some(*observation),
                    _ => None,
                })
                .unwrap(),
        );
    }
    core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(100_000u64)));
    assert_eq!(
        core.router
            .plan()
            .reqs
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        INCUMBENTS
    );
    core.freshness_candidate_atoms.set(0);
    core.freshness_incumbent_demand_edges_visited.set(0);
    core.freshness_plan_request_entries_visited.set(0);
    core.freshness_coalesce_pair_attempts.set(0);

    let effects = core.handle(EngineMsg::Subscribe(pinned_profile_query(
        &candidate_keys.public_key().to_hex(),
        candidate_relay,
        Freshness::MaxAge { seconds: 3_600 },
    )));
    let observation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(observation, _, _) => Some(*observation),
            _ => None,
        })
        .expect("the fresh candidate emits an opening frame");
    let handle = core.observations[&observation].branches[0];
    let evidence = match &core.handles[&handle].acquisition.scopes[0] {
        super::ScopeAcquisition::CoverageSatisfied { evidence } => evidence,
        _ => panic!("fresh coverage must suppress wire work"),
    };

    assert_eq!(evidence.sources.len(), 1);
    assert_eq!(
        evidence.sources[0].relay.as_str(),
        "wss://freshness-candidate.example"
    );
    assert_eq!(core.freshness_candidate_atoms.get(), 1);
    assert_eq!(core.freshness_incumbent_demand_edges_visited.get(), 0);
    assert_eq!(core.freshness_plan_request_entries_visited.get(), 0);
    assert_eq!(core.freshness_coalesce_pair_attempts.get(), 0);
    assert_eq!(
        core.bench_ownership_census()
            .retained_freshness_source_edges,
        1
    );

    core.handle(EngineMsg::Unsubscribe(observation));
    for observation in incumbent_observations {
        core.handle(EngineMsg::Unsubscribe(observation));
    }
    assert_eq!(
        core.bench_ownership_census(),
        super::CoreOwnershipCensus::default()
    );
}

#[test]
fn live_and_cache_only_openings_do_zero_freshness_planning() {
    let relay = RelayUrl::parse("wss://no-freshness-preview.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 256);
    let mut observations = Vec::new();
    for index in 0..207 {
        let freshness = if index % 2 == 0 {
            Freshness::Live
        } else {
            Freshness::CacheOnly
        };
        let effects = core.handle(EngineMsg::Subscribe(pinned_profile_query(
            &Keys::generate().public_key().to_hex(),
            relay.clone(),
            freshness,
        )));
        observations.push(
            effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::EmitRows(observation, _, _) => Some(*observation),
                    _ => None,
                })
                .unwrap(),
        );
    }

    assert_eq!(
        super::CoreFreshnessWork {
            candidate_atoms: core.freshness_candidate_atoms.get(),
            incumbent_demand_edges_visited: core.freshness_incumbent_demand_edges_visited.get(),
            plan_request_entries_visited: core.freshness_plan_request_entries_visited.get(),
            coalesce_pair_attempts: core.freshness_coalesce_pair_attempts.get(),
        },
        super::CoreFreshnessWork::default()
    );
    for observation in observations {
        core.handle(EngineMsg::Unsubscribe(observation));
    }
    assert_eq!(
        core.bench_ownership_census(),
        super::CoreOwnershipCensus::default()
    );
}
