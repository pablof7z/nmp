//! Reproduce the real-corpus ingest and relay-cardinality performance matrix.
//!
//! The source must be a current NMP store. Each matrix cell creates a fresh
//! store from its canonical events, adds observations from the requested
//! number of relays, then measures the busiest NIP-29 room, a complete query,
//! an exact replay, logical/physical size, and the first query after reopen.
//!
//! Usage:
//! `cargo run -p nmp-store --release --example ingest_bench -- <source.redb> [iterations] [relay_counts]`
//!
//! `relay_counts` is comma-separated and defaults to `1,20,100`.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nmp_store::{EventStore, InsertOutcome, RedbStore, RelayObserved};
use nostr::{Alphabet, Event, Filter, Kind, RelayUrl, SingleLetterTag};
use redb::Database;

#[derive(Default)]
struct MatrixStats {
    batch: Duration,
    added_relay_passes: Duration,
    exact_replay: Duration,
    room: Duration,
    complete: Duration,
    reopen_first_room: Duration,
    exact_file_growth: u64,
    physical_bytes: u64,
    logical_bytes: u64,
}

fn relay(index: u32) -> RelayUrl {
    RelayUrl::parse(&format!("wss://ingest-benchmark-relay-{index}.invalid")).unwrap()
}

fn input(events: &[Event], relay: &RelayUrl) -> Vec<(Event, RelayObserved)> {
    events
        .iter()
        .cloned()
        .map(|event| {
            let observed = RelayObserved::new(relay.clone(), event.created_at);
            (event, observed)
        })
        .collect()
}

fn logical_bytes(path: &Path) -> u64 {
    let db = Database::open(path).expect("open store for logical stats");
    let txn = db.begin_write().expect("begin logical stats transaction");
    txn.stats().expect("read logical stats").stored_bytes()
}

fn main() {
    let mut args = env::args_os().skip(1);
    let source_path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: ingest_bench <source.redb> [iterations] [relay_counts]");
    let iterations: u32 = args
        .next()
        .map(|raw| raw.to_string_lossy().parse().expect("iterations is a u32"))
        .unwrap_or(3);
    assert!(iterations > 0, "iterations must be nonzero");
    let relay_counts: Vec<u32> = args
        .next()
        .map(|raw| {
            raw.to_string_lossy()
                .split(',')
                .map(|part| part.parse().expect("relay count is a u32"))
                .collect()
        })
        .unwrap_or_else(|| vec![1, 20, 100]);
    assert!(
        !relay_counts.is_empty() && relay_counts.iter().all(|count| *count > 0),
        "relay counts must be nonzero"
    );

    let source = RedbStore::open(&source_path).expect("open source redb store");
    let events: Vec<_> = source
        .query(&Filter::new())
        .expect("read real source events")
        .into_iter()
        .map(|stored| stored.event)
        .collect();
    assert!(!events.is_empty(), "source store contains no events");

    let kind9 = Kind::from(9u16);
    let h = SingleLetterTag::lowercase(Alphabet::H);
    let mut room_counts: BTreeMap<String, usize> = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == kind9) {
        for tag in event.tags.iter() {
            if tag.single_letter_tag() == Some(h) {
                if let Some(value) = tag.content() {
                    *room_counts.entry(value.to_owned()).or_default() += 1;
                }
            }
        }
    }
    let (room, room_matches) = room_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .expect("source has no kind:9 #h room rows");
    let room_filter = Filter::new()
        .kind(kind9)
        .custom_tag(h, room.clone())
        .limit(200);
    let expected_room = room_matches.min(200);

    let mut sequential = Duration::ZERO;
    let mut matrix: BTreeMap<u32, MatrixStats> = BTreeMap::new();
    for _ in 0..iterations {
        let sequential_dir = tempfile::tempdir().expect("sequential tempdir");
        let mut sequential_store =
            RedbStore::open(sequential_dir.path().join("store.redb")).expect("open sequential");
        let started = Instant::now();
        for event in &events {
            sequential_store
                .insert(
                    event.clone(),
                    RelayObserved::new(relay(0), event.created_at),
                )
                .expect("sequential insert");
        }
        sequential += started.elapsed();

        for relay_count in &relay_counts {
            let stats = matrix.entry(*relay_count).or_default();
            let dir = tempfile::tempdir().expect("matrix tempdir");
            let path = dir.path().join("store.redb");
            let mut store = RedbStore::open(&path).expect("open matrix store");

            let started = Instant::now();
            let outcomes = store
                .insert_batch(input(&events, &relay(0)))
                .expect("batch insert");
            stats.batch += started.elapsed();
            assert_eq!(outcomes.len(), events.len());

            for relay_index in 1..*relay_count {
                let started = Instant::now();
                let outcomes = store
                    .insert_batch(input(&events, &relay(relay_index)))
                    .expect("distinct relay observation pass");
                stats.added_relay_passes += started.elapsed();
                assert!(outcomes.iter().all(|outcome| matches!(
                    outcome,
                    InsertOutcome::Duplicate {
                        provenance_grew: true,
                        satisfied_intents,
                    } if satisfied_intents.is_empty()
                )));
            }

            let before_replay = std::fs::metadata(&path).expect("stat before replay").len();
            let started = Instant::now();
            let outcomes = store
                .insert_batch(input(&events, &relay(0)))
                .expect("exact duplicate replay");
            stats.exact_replay += started.elapsed();
            assert!(outcomes.iter().all(|outcome| matches!(
                outcome,
                InsertOutcome::Duplicate {
                    provenance_grew: false,
                    satisfied_intents,
                } if satisfied_intents.is_empty()
            )));
            let after_replay = std::fs::metadata(&path).expect("stat after replay").len();
            stats.exact_file_growth += after_replay.saturating_sub(before_replay);

            store
                .query_newest(&room_filter, 200)
                .expect("warm room query");
            let started = Instant::now();
            let rows = store
                .query_newest(&room_filter, 200)
                .expect("timed room query");
            stats.room += started.elapsed();
            assert_eq!(rows.len(), expected_room);

            let started = Instant::now();
            let rows = store.query(&Filter::new()).expect("timed complete query");
            stats.complete += started.elapsed();
            assert_eq!(rows.len(), events.len());
            drop(store);

            stats.physical_bytes += std::fs::metadata(&path).expect("stat matrix store").len();
            stats.logical_bytes += logical_bytes(&path);
            let reopened = RedbStore::open(&path).expect("reopen matrix store");
            let started = Instant::now();
            let rows = reopened
                .query_newest(&room_filter, 200)
                .expect("first room query after reopen");
            stats.reopen_first_room += started.elapsed();
            assert_eq!(rows.len(), expected_room);
        }
    }

    let divisor = f64::from(iterations);
    println!("source={}", source_path.display());
    println!("events={}", events.len());
    println!("busiest_room={room}");
    println!("busiest_room_rows={room_matches}");
    println!("iterations={iterations}");
    println!(
        "sequential_mean_ms={:.3}",
        sequential.as_secs_f64() * 1_000.0 / divisor
    );
    for (relay_count, stats) in matrix {
        println!("relay_count={relay_count}");
        println!(
            "  initial_batch_mean_ms={:.3}",
            stats.batch.as_secs_f64() * 1_000.0 / divisor
        );
        if relay_count > 1 {
            let passes = divisor * f64::from(relay_count - 1);
            println!(
                "  added_relay_pass_mean_ms={:.3}",
                stats.added_relay_passes.as_secs_f64() * 1_000.0 / passes
            );
        }
        println!(
            "  exact_replay_mean_ms={:.3}",
            stats.exact_replay.as_secs_f64() * 1_000.0 / divisor
        );
        println!(
            "  room_newest_200_mean_ms={:.3}",
            stats.room.as_secs_f64() * 1_000.0 / divisor
        );
        println!(
            "  complete_query_mean_ms={:.3}",
            stats.complete.as_secs_f64() * 1_000.0 / divisor
        );
        println!(
            "  reopen_first_room_mean_ms={:.3}",
            stats.reopen_first_room.as_secs_f64() * 1_000.0 / divisor
        );
        println!(
            "  exact_replay_file_growth_mean_bytes={}",
            stats.exact_file_growth / u64::from(iterations)
        );
        println!(
            "  physical_mean_bytes={}",
            stats.physical_bytes / u64::from(iterations)
        );
        println!(
            "  logical_mean_bytes={}",
            stats.logical_bytes / u64::from(iterations)
        );
    }
}
