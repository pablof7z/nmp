//! Compare repeated single-event commits with one crash-atomic ingest batch
//! using the canonical events from a real persisted store.
//!
//! Usage:
//! `cargo run -p nmp-store --release --example ingest_bench -- <source.redb> [iterations]`

use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nmp_store::{EventStore, InsertOutcome, RedbStore, RelayObserved};
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
    let mut duplicate_elapsed = Duration::ZERO;
    let mut duplicate_file_growth = 0u64;
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
        let batch_path = batch_dir.path().join("store.redb");
        let mut batch = RedbStore::open(&batch_path).expect("open batch");
        let make_input = || {
            events
                .iter()
                .cloned()
                .map(|event| {
                    let observed = RelayObserved::new(relay.clone(), event.created_at);
                    (event, observed)
                })
                .collect()
        };
        let started = Instant::now();
        let outcomes = batch.insert_batch(make_input()).expect("batch insert");
        batch_elapsed += started.elapsed();
        assert_eq!(outcomes.len(), events.len());
        assert_eq!(batch.query(&Filter::new()).unwrap().len(), events.len());

        let file_len_before = std::fs::metadata(&batch_path)
            .expect("stat batch store before duplicate replay")
            .len();
        let started = Instant::now();
        let duplicate_outcomes = batch
            .insert_batch(make_input())
            .expect("exact duplicate replay");
        duplicate_elapsed += started.elapsed();
        assert!(duplicate_outcomes.iter().all(|outcome| matches!(
            outcome,
            InsertOutcome::Duplicate {
                provenance_grew: false,
                satisfied_intents,
            } if satisfied_intents.is_empty()
        )));
        let file_len_after = std::fs::metadata(&batch_path)
            .expect("stat batch store after duplicate replay")
            .len();
        duplicate_file_growth += file_len_after.saturating_sub(file_len_before);
    }

    let sequential_mean = sequential_elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations);
    let batch_mean = batch_elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations);
    let duplicate_mean = duplicate_elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations);
    println!("source={}", source_path.display());
    println!("events={}", events.len());
    println!("iterations={iterations}");
    println!("sequential_mean_ms={sequential_mean:.3}");
    println!("batch_mean_ms={batch_mean:.3}");
    println!("exact_duplicate_batch_mean_ms={duplicate_mean:.3}");
    println!(
        "exact_duplicate_file_growth_mean_bytes={}",
        duplicate_file_growth / u64::from(iterations)
    );
    println!("speedup={:.2}x", sequential_mean / batch_mean);
}
