//! Measure a real persisted store's busiest NIP-29 room-open query.
//!
//! Usage:
//! `cargo run -p nmp-store --release --example query_bench -- <store.redb> [iterations]`

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nmp_store::{EventStore, RedbStore};
use nostr::{Alphabet, Filter, Kind, SingleLetterTag};

fn main() {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: query_bench <store.redb> [iterations]");
    let iterations: u32 = args
        .next()
        .map(|raw| raw.to_string_lossy().parse().expect("iterations is a u32"))
        .unwrap_or(30);

    let store = RedbStore::open(&path).expect("open redb store");
    let kind = Kind::from(9u16);
    let all_kind9 = store
        .query(&Filter::new().kind(kind))
        .expect("scan kind:9 rows to choose a real room");

    let mut room_counts: BTreeMap<String, usize> = BTreeMap::new();
    for stored in &all_kind9 {
        for tag in stored.event.tags.iter() {
            if tag.single_letter_tag() == Some(SingleLetterTag::lowercase(Alphabet::H)) {
                if let Some(value) = tag.content() {
                    *room_counts.entry(value.to_owned()).or_default() += 1;
                }
            }
        }
    }
    let (room, corpus_matches) = room_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .expect("database has no kind:9 #h room rows");

    let filter = Filter::new()
        .kind(kind)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), room.clone())
        .limit(200);
    let expected = corpus_matches.min(200);
    let warm = store
        .query_newest(&filter, 200)
        .expect("warm bounded room query");
    assert_eq!(warm.len(), expected);

    let mut elapsed = Duration::ZERO;
    for _ in 0..iterations {
        let started = Instant::now();
        let rows = store.query_newest(&filter, 200).expect("timed room query");
        elapsed += started.elapsed();
        assert_eq!(rows.len(), expected);
    }

    println!("store={}", path.display());
    println!("kind9_rows={}", all_kind9.len());
    println!("room={room}");
    println!("room_rows={corpus_matches}");
    println!("returned_rows={expected}");
    println!("iterations={iterations}");
    println!("total_ms={:.3}", elapsed.as_secs_f64() * 1_000.0);
    println!(
        "mean_ms={:.3}",
        elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations)
    );
}
