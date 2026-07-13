//! Measure initial construction of a bounded `Derived` projection against a
//! populated redb store.
//!
//! Usage:
//! `cargo run -p nmp-resolver --release --example derived_limit_bench -- <store.redb> [room] [limit] [iterations]`

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nmp_grammar::{Binding, Demand, Derived, Filter, IndexedTagName, Selector};
use nmp_resolver::{Engine, LiveQuery};
use nmp_store::RedbStore;

fn profile_query(room: &str, limit: usize) -> LiveQuery {
    let inner = Filter {
        kinds: Some(BTreeSet::from([9u16])),
        tags: BTreeMap::from([(
            IndexedTagName::new('h').expect("indexed h tag"),
            Binding::Literal(BTreeSet::from([room.to_owned()])),
        )]),
        limit: Some(limit),
        ..Filter::default()
    };
    LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(inner),
            project: Selector::Authors,
        }))),
        limit: Some(500),
        ..Filter::default()
    })
}

fn percentile(samples: &mut [Duration], numerator: usize, denominator: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * numerator / denominator]
}

fn main() {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: derived_limit_bench <store.redb> [room] [limit] [iterations]");
    let room = args
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nmp-scale-hot-room".to_owned());
    let limit: usize = args
        .next()
        .map(|value| value.to_string_lossy().parse().expect("limit is usize"))
        .unwrap_or(200);
    let iterations: u32 = args
        .next()
        .map(|value| value.to_string_lossy().parse().expect("iterations is u32"))
        .unwrap_or(10);

    let mut samples = Vec::with_capacity(iterations as usize);
    let mut demand_atoms = 0usize;
    let mut graph_nodes = 0usize;
    for _ in 0..iterations {
        let store = RedbStore::open(&path).expect("open benchmark store");
        let mut engine = Engine::new(store);
        let started = Instant::now();
        let (_handle, _delta) = engine
            .subscribe(profile_query(&room, limit))
            .expect("subscribe bounded Derived query");
        samples.push(started.elapsed());
        demand_atoms = engine.active_demand().len();
        graph_nodes = engine.graph_snapshot().nodes.len();
    }

    let p50 = percentile(&mut samples.clone(), 50, 100);
    let p95 = percentile(&mut samples, 95, 100);
    println!("store={}", path.display());
    println!("room={room}");
    println!("inner_limit={limit}");
    println!("iterations={iterations}");
    println!("derived_open_p50_ms={:.3}", p50.as_secs_f64() * 1_000.0);
    println!("derived_open_p95_ms={:.3}", p95.as_secs_f64() * 1_000.0);
    println!("demand_atoms={demand_atoms}");
    println!("graph_nodes={graph_nodes}");
}
