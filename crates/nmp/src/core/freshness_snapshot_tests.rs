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
    let directory = tempfile::tempdir().unwrap();
    let mut store =
        RedbStore::open_benchmark_nondurable(directory.path().join("fresh.redb")).unwrap();
    store
        .record_coverage(&[(
            atom,
            relay.clone(),
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(99_000u64)),
        )])
        .unwrap();
    let mut core = EngineCore::new(store, 8);
    core.handle(EngineMsg::Tick(Timestamp::from(100_000u64)));
    core.bench_reset_coverage_reads();

    let mut demand = Demand::new(
        filter,
        SourceAuthority::Pinned(BTreeSet::from([relay])),
        AccessContext::Public,
    )
    .unwrap();
    demand.freshness = Freshness::MaxAge { seconds: 3_600 };
    let effects = core.handle(EngineMsg::Subscribe(LiveQuery::single(demand)));

    assert_eq!(core.bench_coverage_reads(), 1);
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
