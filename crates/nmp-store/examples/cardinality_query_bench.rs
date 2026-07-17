//! Query-latency falsifier for sampled planner cardinalities.
//!
//! Usage: `cardinality_query_bench <exact|sampled> <events.jsonl> [iterations]`

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use nmp_store::{set_bench_exact_cardinality, EventStore, RedbStore, RelayObserved};
use nostr::{Event, Filter, JsonUtil, Kind, PublicKey, RelayUrl, SingleLetterTag, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct Measurement {
    name: String,
    expected_rows: usize,
    p50_ns: u64,
    p95_ns: u64,
}

#[derive(Deserialize, Serialize)]
struct Report {
    mode: String,
    nmp_commit: String,
    git_dirty: bool,
    corpus: String,
    input_events: usize,
    canonical_events: usize,
    iterations: usize,
    import_ms: u64,
    measurements: Vec<Measurement>,
}

#[derive(Serialize)]
struct MatrixReport {
    schema: &'static str,
    command: String,
    repetitions: usize,
    alternating_order: bool,
    runs: Vec<Report>,
}

fn git_output(args: &[&str]) -> String {
    String::from_utf8(
        Command::new("git")
            .args(args)
            .output()
            .expect("run git")
            .stdout,
    )
    .expect("git output is utf8")
    .trim()
    .to_owned()
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
        samples.push(started.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    let p50_ns = samples[samples.len() / 2];
    let p95_ns = samples[((samples.len() * 95).div_ceil(100)).saturating_sub(1)];
    Measurement {
        name: name.to_owned(),
        expected_rows,
        p50_ns,
        p95_ns,
    }
}

fn main() {
    let mut args = env::args_os().skip(1);
    let mode = args
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .expect("usage: cardinality_query_bench <exact|sampled|matrix> ...");
    if mode == "matrix" {
        let corpus = PathBuf::from(args.next().expect("matrix requires corpus path"));
        let output = PathBuf::from(args.next().expect("matrix requires output path"));
        let iterations = args
            .next()
            .map(|value| {
                value
                    .to_string_lossy()
                    .parse()
                    .expect("iterations is usize")
            })
            .unwrap_or(30usize);
        let repetitions = args
            .next()
            .map(|value| {
                value
                    .to_string_lossy()
                    .parse()
                    .expect("repetitions is usize")
            })
            .unwrap_or(5usize);
        let current_exe = env::current_exe().expect("current query benchmark executable");
        let mut runs = Vec::new();
        for repetition in 0..repetitions {
            let modes = if repetition % 2 == 0 {
                ["exact", "sampled"]
            } else {
                ["sampled", "exact"]
            };
            for child_mode in modes {
                eprintln!("repetition={repetition} mode={child_mode}");
                let child = Command::new(&current_exe)
                    .arg(child_mode)
                    .arg(&corpus)
                    .arg(iterations.to_string())
                    .output()
                    .expect("run query benchmark child");
                assert!(
                    child.status.success(),
                    "{child_mode} child failed: {}",
                    String::from_utf8_lossy(&child.stderr)
                );
                runs.push(serde_json::from_slice::<Report>(&child.stdout).expect("decode child"));
            }
        }
        let first = runs.first().expect("matrix has runs");
        assert!(runs.iter().all(|run| {
            run.nmp_commit == first.nmp_commit
                && run.git_dirty == first.git_dirty
                && run.corpus == first.corpus
                && run.input_events == first.input_events
        }));
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).expect("create output directory");
        }
        let record = MatrixReport {
            schema: "nmp-cardinality-query-matrix-v1",
            command: format!(
                "cargo run -p nmp-store --release --features bench-instrumentation --example cardinality_query_bench -- matrix {} {} {iterations} {repetitions}",
                corpus.display(),
                output.display()
            ),
            repetitions,
            alternating_order: true,
            runs,
        };
        std::fs::write(
            &output,
            serde_json::to_vec_pretty(&record).expect("encode matrix"),
        )
        .expect("write matrix");
        println!("wrote {}", output.display());
        return;
    }
    match mode.as_str() {
        "exact" => set_bench_exact_cardinality(true),
        "sampled" => set_bench_exact_cardinality(false),
        _ => panic!("mode must be exact or sampled"),
    }
    let corpus = args
        .next()
        .map(PathBuf::from)
        .expect("usage: cardinality_query_bench <exact|sampled> <events.jsonl> [iterations]");
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
            mode,
            nmp_commit: git_output(&["rev-parse", "HEAD"]),
            git_dirty: !git_output(&["status", "--porcelain"]).is_empty(),
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
