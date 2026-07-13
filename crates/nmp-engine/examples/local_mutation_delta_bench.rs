//! Compare #228's exact local-mutation projection with the former
//! recompile-and-full-refresh path over an existing populated redb store.
//!
//! The supplied fixture must be writable and retain the frozen million-row
//! scale corpus' expiration schedule. The benchmark adds ordinary local
//! pending rows and short-lived signed rows; it never mutates the frozen
//! source fixture directly.
//!
//! Usage:
//! `cargo run -p nmp-engine --release --features bench-instrumentation --example local_mutation_delta_bench -- <writable-store.redb> [iterations]`

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nmp_engine::core::{Effect, EngineCore, EngineMsg, RowDelta, RowSink};
use nmp_grammar::{Binding, Filter, IndexedTagName};
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::FixtureDirectory;
use nmp_store::{
    sentinel_signature, AcceptWrite, IntentSigState, RedbStore, RelayObserved, WriteDurability,
};
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Tag, Timestamp};

struct CountingAllocator;

// SAFETY: every allocation is delegated unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;
static ALLOCATION_OPS: AtomicU64 = AtomicU64::new(0);

const FIRST_MUTATION_AT: u64 = 1_704_000_100;
// The scale corpus contains its own canonical NIP-40 rows with deadlines
// above the 1_700_000_000 event-shape seed. Keep benchmark-owned expiry
// deadlines below that range so each measured sweep drains exactly one row.
const FIRST_EXPIRY_AT: u64 = 1_600_000_000;

#[derive(Clone, Copy)]
enum Scenario {
    Global,
    HotRoom,
    Author,
    Kind,
    AuthorKind,
}

impl Scenario {
    const ALL: [Self; 5] = [
        Self::Global,
        Self::HotRoom,
        Self::Author,
        Self::Kind,
        Self::AuthorKind,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::HotRoom => "hot_h",
            Self::Author => "author",
            Self::Kind => "kind",
            Self::AuthorKind => "author_kind",
        }
    }
}

struct NullSink;

impl RowSink for NullSink {
    fn on_rows(&self, _rows: Vec<RowDelta>) {}
}

#[derive(Default)]
struct Samples {
    elapsed: Vec<Duration>,
    allocations: Vec<u64>,
    index_rows: Vec<u64>,
    event_values: Vec<u64>,
    materialized_rows: Vec<u64>,
}

impl Samples {
    fn push(&mut self, elapsed: Duration, allocations: u64, work: (u64, u64, u64)) {
        self.elapsed.push(elapsed);
        self.allocations.push(allocations);
        self.index_rows.push(work.0);
        self.event_values.push(work.1);
        self.materialized_rows.push(work.2);
    }
}

fn deterministic_keys(index: usize) -> Keys {
    Keys::parse(&format!("{:064x}", index + 1)).expect("small nonzero scalar")
}

fn scenario_filter(scenario: Scenario, author: &Keys) -> Filter {
    let authors = || {
        Some(Binding::Literal(BTreeSet::from([author
            .public_key()
            .to_hex()])))
    };
    match scenario {
        Scenario::Global => Filter::default(),
        Scenario::HotRoom => Filter {
            tags: BTreeMap::from([(
                IndexedTagName::new('h').unwrap(),
                Binding::Literal(BTreeSet::from(["nmp-scale-hot-room".to_owned()])),
            )]),
            ..Filter::default()
        },
        Scenario::Author => Filter {
            authors: authors(),
            ..Filter::default()
        },
        Scenario::Kind => Filter {
            kinds: Some(BTreeSet::from([9u16])),
            ..Filter::default()
        },
        Scenario::AuthorKind => Filter {
            kinds: Some(BTreeSet::from([9u16])),
            authors: authors(),
            ..Filter::default()
        },
    }
}

fn scenario_event(
    scenario: Scenario,
    author: &Keys,
    fallback: &Keys,
    content: String,
    created_at: u64,
    expiration: Option<u64>,
) -> Event {
    let (keys, kind, mut tags) = match scenario {
        Scenario::Global => (fallback, Kind::TextNote, Vec::new()),
        Scenario::HotRoom => (
            fallback,
            Kind::from(9u16),
            vec![Tag::parse(["h", "nmp-scale-hot-room"]).unwrap()],
        ),
        Scenario::Author => (author, Kind::TextNote, Vec::new()),
        Scenario::Kind => (fallback, Kind::from(9u16), Vec::new()),
        Scenario::AuthorKind => (author, Kind::from(9u16), Vec::new()),
    };
    if let Some(expiration) = expiration {
        tags.push(Tag::expiration(Timestamp::from(expiration)));
    }
    EventBuilder::new(kind, content)
        .tags(tags)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

fn frozen_from_signed(signed: &Event) -> Event {
    Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    )
}

fn accept_write(signed: &Event, accepted_at: u64) -> AcceptWrite {
    AcceptWrite {
        frozen: frozen_from_signed(signed),
        replaceable_base: None,
        expected_pubkey: signed.pubkey,
        signing_identity_ref: "local-mutation-benchmark".to_owned(),
        durability: WriteDurability::Durable,
        routing: "local-mutation-benchmark".to_owned(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(accepted_at),
    }
}

fn initial_snapshot(effects: Vec<Effect>) -> (HandleId, usize) {
    effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, rows, _) => Some((id, rows.len())),
            _ => None,
        })
        .expect("subscribe emits initial snapshot")
}

fn projected_rows(effects: &[Effect]) -> Vec<&RowDelta> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(_, rows, _) => Some(rows),
            _ => None,
        })
        .flatten()
        .collect()
}

fn assert_single_added(effects: &[Effect], expected: &Event) {
    let rows = projected_rows(effects);
    assert!(matches!(
        rows.as_slice(),
        [RowDelta::Added(row)] if row.event.id == expected.id
    ));
}

fn assert_single_removed(effects: &[Effect], expected: &Event) {
    let rows = projected_rows(effects);
    assert!(matches!(
        rows.as_slice(),
        [RowDelta::Removed(id)] if *id == expected.id
    ));
}

fn percentile(samples: &[Duration], numerator: usize, denominator: usize) -> Duration {
    let mut samples = samples.to_vec();
    samples.sort_unstable();
    let rank = (samples.len() * numerator).div_ceil(denominator);
    samples[rank.saturating_sub(1).min(samples.len() - 1)]
}

fn mean(samples: &[u64]) -> f64 {
    samples.iter().sum::<u64>() as f64 / samples.len() as f64
}

fn print_samples(prefix: &str, samples: &Samples) {
    println!(
        "{prefix}_p50_ms={:.3}",
        percentile(&samples.elapsed, 50, 100).as_secs_f64() * 1_000.0
    );
    println!(
        "{prefix}_p95_ms={:.3}",
        percentile(&samples.elapsed, 95, 100).as_secs_f64() * 1_000.0
    );
    println!(
        "{prefix}_mean_allocation_ops={:.1}",
        mean(&samples.allocations)
    );
    println!("{prefix}_mean_index_rows={:.1}", mean(&samples.index_rows));
    println!(
        "{prefix}_mean_event_values={:.1}",
        mean(&samples.event_values)
    );
    println!(
        "{prefix}_mean_materialized_rows={:.1}",
        mean(&samples.materialized_rows)
    );
}

fn measure(
    core: &mut EngineCore<RedbStore>,
    mutation: impl FnOnce(&mut EngineCore<RedbStore>) -> Vec<Effect>,
) -> (Vec<Effect>, Duration, u64, (u64, u64, u64)) {
    core.bench_reset_query_work();
    let allocations_before = ALLOCATION_OPS.load(Ordering::Relaxed);
    let started = Instant::now();
    let effects = mutation(core);
    let elapsed = started.elapsed();
    let allocations = ALLOCATION_OPS
        .load(Ordering::Relaxed)
        .saturating_sub(allocations_before);
    (effects, elapsed, allocations, core.bench_query_work())
}

fn main() {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: local_mutation_delta_bench <writable-store.redb> [iterations]");
    let iterations: u64 = args
        .next()
        .map(|value| value.to_string_lossy().parse().expect("iterations is u64"))
        .unwrap_or(3);
    assert!(iterations > 0);

    let author = deterministic_keys(0);
    let fallback = deterministic_keys(2_047);
    let relay = RelayUrl::parse("wss://local-mutation.benchmark.invalid").unwrap();
    println!("store={}", path.display());
    println!("iterations={iterations}");

    for (scenario_index, scenario) in Scenario::ALL.into_iter().enumerate() {
        let store = RedbStore::open(&path).expect("open benchmark store");
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let (_handle, initial_rows) = initial_snapshot(core.handle(EngineMsg::Subscribe(
            LiveQuery::from_filter(scenario_filter(scenario, &author)),
            Box::new(NullSink),
        )));
        let mut local_direct = Samples::default();
        let mut local_forced = Samples::default();
        let mut expiry_direct = Samples::default();
        let mut expiry_forced = Samples::default();

        for iteration in 0..iterations {
            let base = FIRST_MUTATION_AT + scenario_index as u64 * 10_000 + iteration * 10;
            let expiry_base = FIRST_EXPIRY_AT + scenario_index as u64 * 10_000 + iteration * 10;
            let direct = scenario_event(
                scenario,
                &author,
                &fallback,
                format!("#228 local direct {} {iteration}", scenario.name()),
                base,
                None,
            );
            let (effects, elapsed, allocations, work) = measure(&mut core, |core| {
                core.bench_accept_local(accept_write(&direct, base + 1))
            });
            assert_single_added(&effects, &direct);
            local_direct.push(elapsed, allocations, work);

            let forced = scenario_event(
                scenario,
                &author,
                &fallback,
                format!("#228 local forced {} {iteration}", scenario.name()),
                base + 1,
                None,
            );
            let (effects, elapsed, allocations, work) = measure(&mut core, |core| {
                core.bench_accept_local_with_forced_refresh(accept_write(&forced, base + 2))
            });
            assert_single_added(&effects, &forced);
            local_forced.push(elapsed, allocations, work);

            let expiring_direct = scenario_event(
                scenario,
                &author,
                &fallback,
                format!("#228 expiry direct {} {iteration}", scenario.name()),
                expiry_base + 2,
                Some(expiry_base + 4),
            );
            let seed_effects = core.bench_ingest_observed(vec![(
                expiring_direct.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(expiry_base + 3)),
            )]);
            assert_single_added(&seed_effects, &expiring_direct);
            let (effects, elapsed, allocations, work) = measure(&mut core, |core| {
                core.bench_expire_due(Timestamp::from(expiry_base + 4))
            });
            assert_single_removed(&effects, &expiring_direct);
            expiry_direct.push(elapsed, allocations, work);

            let expiring_forced = scenario_event(
                scenario,
                &author,
                &fallback,
                format!("#228 expiry forced {} {iteration}", scenario.name()),
                expiry_base + 5,
                Some(expiry_base + 7),
            );
            let seed_effects = core.bench_ingest_observed(vec![(
                expiring_forced.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(expiry_base + 6)),
            )]);
            assert_single_added(&seed_effects, &expiring_forced);
            let (effects, elapsed, allocations, work) = measure(&mut core, |core| {
                core.bench_expire_due_with_forced_refresh(Timestamp::from(expiry_base + 7))
            });
            assert_single_removed(&effects, &expiring_forced);
            expiry_forced.push(elapsed, allocations, work);
        }

        println!("scenario={}", scenario.name());
        println!("initial_rows={initial_rows}");
        print_samples("local_direct", &local_direct);
        print_samples("local_forced_full_refresh", &local_forced);
        print_samples("expiry_direct", &expiry_direct);
        print_samples("expiry_forced_full_refresh", &expiry_forced);
    }
}
