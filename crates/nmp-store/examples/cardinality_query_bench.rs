//! Query-latency falsifier for sampled planner cardinalities.
//!
//! Usage: `cardinality_query_bench <events.jsonl> [iterations]`

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Instant;

use nmp_store::{EventStore, RedbStore, RelayObserved};
use nostr::{Event, Filter, JsonUtil, Kind, PublicKey, RelayUrl, SingleLetterTag, Timestamp};
use serde::Serialize;

#[derive(Serialize)]
struct Measurement {
    name: &'static str,
    expected_rows: usize,
    p50_us: u64,
    p95_us: u64,
}

#[derive(Serialize)]
struct Report {
    corpus: String,
    input_events: usize,
    canonical_events: usize,
    iterations: usize,
    import_ms: u64,
    measurements: Vec<Measurement>,
}

fn load_events(path: &PathBuf) -> Vec<Event> {
    BufReader::new(File::open(path).expect("open corpus"))
        .lines()
        .filter_map(|line| {
            let line = line.expect("read corpus row");
            (!line.trim().is_empty()).then(|| Event::from_json(line).expect("parse corpus row"))
        })
        .collect()
}

fn measure(name: &'static str, iterations: usize, mut query: impl FnMut() -> usize) -> Measurement {
    let expected_rows = query();
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        assert_eq!(query(), expected_rows, "{name} result changed");
        samples.push(started.elapsed().as_micros() as u64);
    }
    samples.sort_unstable();
    let p50_us = samples[samples.len() / 2];
    let p95_us = samples[((samples.len() * 95).div_ceil(100)).saturating_sub(1)];
    Measurement {
        name,
        expected_rows,
        p50_us,
        p95_us,
    }
}

fn main() {
    let mut args = env::args_os().skip(1);
    let corpus = args
        .next()
        .map(PathBuf::from)
        .expect("usage: cardinality_query_bench <events.jsonl> [iterations]");
    let iterations = args
        .next()
        .map(|value| {
            value
                .to_string_lossy()
                .parse()
                .expect("iterations is usize")
        })
        .unwrap_or(30usize);
    assert!(iterations > 0, "iterations must be nonzero");

    let events = load_events(&corpus);
    let input_events = events.len();
    let scratch = tempfile::tempdir().expect("query benchmark scratch");
    let path = scratch.path().join("store.redb");
    let relay = RelayUrl::parse("wss://cardinality-query.invalid").unwrap();
    let observed = Timestamp::from(
        events
            .iter()
            .map(|event| event.created_at.as_secs())
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    );
    let mut store = RedbStore::open(&path).expect("open store");
    let started = Instant::now();
    for batch in events.chunks(4_096) {
        store
            .insert_batch(
                batch
                    .iter()
                    .cloned()
                    .map(|event| (event, RelayObserved::new(relay.clone(), observed)))
                    .collect(),
            )
            .expect("import corpus batch");
    }
    let import_ms = started.elapsed().as_millis() as u64;

    let rows = store.query(&Filter::new()).expect("query canonical rows");
    let canonical_events = rows.len();
    assert!(canonical_events >= 43, "corpus needs at least 43 live rows");

    let mut kind_counts: BTreeMap<Kind, usize> = BTreeMap::new();
    let mut author_counts: BTreeMap<PublicKey, usize> = BTreeMap::new();
    let mut author_kind_counts: BTreeMap<(PublicKey, Kind), usize> = BTreeMap::new();
    let mut tag_counts: BTreeMap<(SingleLetterTag, String), usize> = BTreeMap::new();
    let mut per_event_tags = Vec::with_capacity(rows.len());
    for row in &rows {
        *kind_counts.entry(row.event.kind).or_default() += 1;
        *author_counts.entry(row.event.pubkey).or_default() += 1;
        *author_kind_counts
            .entry((row.event.pubkey, row.event.kind))
            .or_default() += 1;
        let tags: BTreeSet<_> = row
            .event
            .tags
            .iter()
            .filter_map(|tag| Some((tag.single_letter_tag()?, tag.content()?.to_owned())))
            .collect();
        for tag in &tags {
            *tag_counts.entry(tag.clone()).or_default() += 1;
        }
        per_event_tags.push(tags);
    }

    let kind = kind_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .expect("corpus has a kind")
        .0;
    let author = author_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .expect("corpus has an author")
        .0;
    let (author_kind_author, author_kind) = author_kind_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .expect("corpus has an author-kind pair")
        .0;
    let busiest_tag = tag_counts
        .iter()
        .max_by_key(|(_, count)| **count)
        .expect("corpus has an indexed tag")
        .0
        .clone();

    let mut best_pair = None;
    for tags in &per_event_tags {
        let tags: Vec<_> = tags.iter().collect();
        for (left_index, left) in tags.iter().enumerate() {
            for right in tags.iter().skip(left_index + 1) {
                if left.0 == right.0 {
                    continue;
                }
                let left_count = tag_counts[*left];
                let right_count = tag_counts[*right];
                let score = left_count.max(right_count) / left_count.min(right_count).max(1);
                if best_pair
                    .as_ref()
                    .is_none_or(|(best_score, _, _)| score > *best_score)
                {
                    best_pair = Some((score, (*left).clone(), (*right).clone()));
                }
            }
        }
    }
    let (_, pair_left, pair_right) = best_pair.expect("corpus has two distinct tag letters");

    let authors_43: BTreeSet<_> = rows
        .iter()
        .map(|row| row.event.pubkey)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(43)
        .collect();

    let kind_filter = Filter::new().kind(kind);
    let author_filter = Filter::new().author(author);
    let author_kind_filter = Filter::new().author(author_kind_author).kind(author_kind);
    let tag_filter = Filter::new().custom_tag(busiest_tag.0, busiest_tag.1);
    let tag_pair_filter = Filter::new()
        .custom_tag(pair_left.0, pair_left.1)
        .custom_tag(pair_right.0, pair_right.1);
    let authors_43_filter = Filter::new().authors(authors_43);

    let measurements = vec![
        measure("complete_kind", iterations, || {
            store.query(&kind_filter).unwrap().len()
        }),
        measure("complete_author", iterations, || {
            store.query(&author_filter).unwrap().len()
        }),
        measure("complete_author_kind", iterations, || {
            store.query(&author_kind_filter).unwrap().len()
        }),
        measure("complete_tag", iterations, || {
            store.query(&tag_filter).unwrap().len()
        }),
        measure("complete_tag_pair", iterations, || {
            store.query(&tag_pair_filter).unwrap().len()
        }),
        measure("complete_authors_43", iterations, || {
            store.query(&authors_43_filter).unwrap().len()
        }),
        measure("bounded_kind", iterations, || {
            store.query_newest(&kind_filter, 200).unwrap().len()
        }),
        measure("bounded_tag_pair", iterations, || {
            store.query_newest(&tag_pair_filter, 200).unwrap().len()
        }),
    ];

    println!(
        "{}",
        serde_json::to_string_pretty(&Report {
            corpus: corpus.display().to_string(),
            input_events,
            canonical_events,
            iterations,
            import_ms,
            measurements,
        })
        .unwrap()
    );
}
