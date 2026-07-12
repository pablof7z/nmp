//! Compare repeated single-event commits with one crash-atomic ingest batch
//! using the canonical events from a real persisted store.
//!
//! Usage:
//! `cargo run -p nmp-store --release --example ingest_bench -- <source.redb> [iterations]`

use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nmp_store::{EventStore, RedbStore, RelayObserved};
use nostr::{Filter, RelayUrl};

fn main() {
    let mut args = env::args_os().skip(1);
    let source_path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: ingest_bench <source.redb> [iterations]");
    let iterations: u32 = args
        .next()
        .map(|raw| raw.to_string_lossy().parse().expect("iterations is a u32"))
        .unwrap_or(5);

    let source = RedbStore::open(&source_path).expect("open source redb store");
    let events: Vec<_> = source
        .query(&Filter::new())
        .expect("read real source events")
        .into_iter()
        .map(|stored| stored.event)
        .collect();
    assert!(!events.is_empty(), "source store contains no events");
    let relay = RelayUrl::parse("wss://ingest-benchmark.invalid").unwrap();

    let mut sequential_elapsed = Duration::ZERO;
    let mut batch_elapsed = Duration::ZERO;
    for _ in 0..iterations {
        let sequential_dir = tempfile::tempdir().expect("sequential tempdir");
        let mut sequential =
            RedbStore::open(sequential_dir.path().join("store.redb")).expect("open sequential");
        let started = Instant::now();
        for event in &events {
            sequential
                .insert(
                    event.clone(),
                    RelayObserved::new(relay.clone(), event.created_at),
                )
                .expect("sequential insert");
        }
        sequential_elapsed += started.elapsed();
        assert_eq!(
            sequential.query(&Filter::new()).unwrap().len(),
            events.len()
        );

        let batch_dir = tempfile::tempdir().expect("batch tempdir");
        let mut batch = RedbStore::open(batch_dir.path().join("store.redb")).expect("open batch");
        let input = events
            .iter()
            .cloned()
            .map(|event| {
                let observed = RelayObserved::new(relay.clone(), event.created_at);
                (event, observed)
            })
            .collect();
        let started = Instant::now();
        let outcomes = batch.insert_batch(input).expect("batch insert");
        batch_elapsed += started.elapsed();
        assert_eq!(outcomes.len(), events.len());
        assert_eq!(batch.query(&Filter::new()).unwrap().len(), events.len());
    }

    let sequential_mean = sequential_elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations);
    let batch_mean = batch_elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations);
    println!("source={}", source_path.display());
    println!("events={}", events.len());
    println!("iterations={iterations}");
    println!("sequential_mean_ms={sequential_mean:.3}");
    println!("batch_mean_ms={batch_mean:.3}");
    println!("speedup={:.2}x", sequential_mean / batch_mean);
}
