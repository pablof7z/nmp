//! Real-store query matrix for cardinality-aware one-best-index planning.
//!
//! Usage:
//! `cargo run -p nmp-store --release --example query_bench -- <store.redb> [iterations]`

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nmp_store::{EventStore, RedbStore, RelayObserved};
use nostr::{
    Alphabet, Event, Filter, JsonUtil, Kind, PublicKey, RelayUrl, SingleLetterTag, Timestamp,
};

struct CountingAllocator;

static ALLOCATION_OPS: AtomicU64 = AtomicU64::new(0);

// SAFETY: every operation delegates unchanged to `System`; the counter is
// observational and does not affect pointer/layout semantics.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

fn mean_ms(iterations: u32, mut query: impl FnMut() -> usize, expected: usize) -> f64 {
    assert_eq!(query(), expected, "warm query result count");
    let mut elapsed = Duration::ZERO;
    for _ in 0..iterations {
        let started = Instant::now();
        assert_eq!(query(), expected, "timed query result count");
        elapsed += started.elapsed();
    }
    elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations)
}

fn mean_allocation_ops(iterations: u32, mut query: impl FnMut() -> usize, expected: usize) -> f64 {
    assert_eq!(query(), expected, "warm allocation query result count");
    let mut total = 0u64;
    for _ in 0..iterations {
        let before = ALLOCATION_OPS.load(Ordering::Relaxed);
        assert_eq!(query(), expected, "allocation query result count");
        total = total.saturating_add(
            ALLOCATION_OPS
                .load(Ordering::Relaxed)
                .saturating_sub(before),
        );
    }
    total as f64 / f64::from(iterations)
}

fn main() {
    let mut args = env::args_os().skip(1);
    let input_path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: query_bench <store.redb|events.jsonl> [iterations]");
    let iterations: u32 = args
        .next()
        .map(|raw| raw.to_string_lossy().parse().expect("iterations is a u32"))
        .unwrap_or(30);

    let scratch = (input_path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .then(|| tempfile::tempdir().expect("query benchmark tempdir"));
    let path = scratch
        .as_ref()
        .map_or_else(|| input_path.clone(), |dir| dir.path().join("store.redb"));
    if scratch.is_some() {
        let source = std::fs::read_to_string(&input_path).expect("read event JSONL");
        let events: Vec<Event> = source
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| Event::from_json(line).expect("parse event JSONL row"))
            .collect();
        let relay = RelayUrl::parse("wss://query-benchmark.invalid").unwrap();
        let mut imported = RedbStore::open(&path).expect("open benchmark store");
        imported
            .insert_batch(
                events
                    .into_iter()
                    .map(|event| {
                        let observed = RelayObserved::new(relay.clone(), Timestamp::from(1u64));
                        (event, observed)
                    })
                    .collect(),
            )
            .expect("import benchmark JSONL");
    }

    let store = RedbStore::open(&path).expect("open redb store");
    let kind = Kind::from(9u16);
    let h = SingleLetterTag::lowercase(Alphabet::H);
    let p = SingleLetterTag::lowercase(Alphabet::P);
    let all_rows = store.query(&Filter::new()).expect("scan all rows");
    let all_kind9 = store
        .query(&Filter::new().kind(kind))
        .expect("scan kind:9 rows");

    let mut room_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut author_counts: BTreeMap<PublicKey, usize> = BTreeMap::new();
    for stored in &all_kind9 {
        *author_counts.entry(stored.event.pubkey).or_default() += 1;
        for tag in stored.event.tags.iter() {
            if tag.single_letter_tag() == Some(h) {
                if let Some(value) = tag.content() {
                    *room_counts.entry(value.to_owned()).or_default() += 1;
                }
            }
        }
    }
    let (room, room_rows) = room_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .expect("database has no kind:9 #h room rows");
    let (author, author_kind_rows) = author_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .expect("database has no kind:9 author");

    let mut member_counts: BTreeMap<String, usize> = BTreeMap::new();
    for stored in &all_kind9 {
        let in_room =
            stored.event.tags.iter().any(|tag| {
                tag.single_letter_tag() == Some(h) && tag.content() == Some(room.as_str())
            });
        if !in_room {
            continue;
        }
        for tag in stored.event.tags.iter() {
            if tag.single_letter_tag() == Some(p) {
                if let Some(value) = tag.content() {
                    *member_counts.entry(value.to_owned()).or_default() += 1;
                }
            }
        }
    }
    let (member, room_member_rows) = member_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .expect("busiest room has no #p member rows");

    let room_filter = Filter::new().kind(kind).custom_tag(h, room.clone());
    let room_member_filter = room_filter.clone().custom_tag(p, member.clone());
    let kind_filter = Filter::new().kind(kind);
    let author_filter = Filter::new().author(author);
    let author_kind_filter = author_filter.clone().kind(kind);
    let corpus_authors: BTreeSet<_> = all_rows.iter().map(|row| row.event.pubkey).collect();
    assert!(
        corpus_authors.len() >= 43,
        "real-corpus 43-author OR requires 43 populated authors"
    );
    let authors_43: BTreeSet<_> = corpus_authors.into_iter().take(43).collect();
    let authors_43_filter = Filter::new().authors(authors_43.clone());
    let authors_43_rows = all_rows
        .iter()
        .filter(|row| authors_43.contains(&row.event.pubkey))
        .count();
    let search = all_kind9
        .iter()
        .find_map(|row| (!row.event.content.is_empty()).then_some(row.event.content.clone()))
        .expect("kind:9 corpus has nonempty content");
    let search_filter = kind_filter.clone().search(search);
    let search_rows = store
        .query(&search_filter)
        .expect("count search rows")
        .len();

    let bounded_room_ms = mean_ms(
        iterations,
        || store.query_newest(&room_filter, 200).unwrap().len(),
        room_rows.min(200),
    );
    let bounded_room_member_ms = mean_ms(
        iterations,
        || store.query_newest(&room_member_filter, 200).unwrap().len(),
        room_member_rows.min(200),
    );
    let complete_room_ms = mean_ms(
        iterations,
        || store.query(&room_filter).unwrap().len(),
        room_rows,
    );
    let complete_room_member_ms = mean_ms(
        iterations,
        || store.query(&room_member_filter).unwrap().len(),
        room_member_rows,
    );
    let complete_global_ms = mean_ms(
        iterations,
        || store.query(&Filter::new()).unwrap().len(),
        all_rows.len(),
    );
    let complete_kind_ms = mean_ms(
        iterations,
        || store.query(&kind_filter).unwrap().len(),
        all_kind9.len(),
    );
    let complete_author_ms = mean_ms(
        iterations,
        || store.query(&author_filter).unwrap().len(),
        all_rows
            .iter()
            .filter(|row| row.event.pubkey == author)
            .count(),
    );
    let complete_author_kind_ms = mean_ms(
        iterations,
        || store.query(&author_kind_filter).unwrap().len(),
        author_kind_rows,
    );
    let complete_authors_43_ms = mean_ms(
        iterations,
        || store.query(&authors_43_filter).unwrap().len(),
        authors_43_rows,
    );
    let rejected_search_ms = mean_ms(
        iterations,
        || store.query_newest(&search_filter, 1).unwrap().len(),
        search_rows.min(1),
    );
    let allocation_matrix = [
        (
            "bounded_room",
            mean_allocation_ops(
                iterations,
                || store.query_newest(&room_filter, 200).unwrap().len(),
                room_rows.min(200),
            ),
        ),
        (
            "bounded_room_member",
            mean_allocation_ops(
                iterations,
                || store.query_newest(&room_member_filter, 200).unwrap().len(),
                room_member_rows.min(200),
            ),
        ),
        (
            "complete_room",
            mean_allocation_ops(
                iterations,
                || store.query(&room_filter).unwrap().len(),
                room_rows,
            ),
        ),
        (
            "complete_room_member",
            mean_allocation_ops(
                iterations,
                || store.query(&room_member_filter).unwrap().len(),
                room_member_rows,
            ),
        ),
        (
            "complete_global",
            mean_allocation_ops(
                iterations,
                || store.query(&Filter::new()).unwrap().len(),
                all_rows.len(),
            ),
        ),
        (
            "complete_kind",
            mean_allocation_ops(
                iterations,
                || store.query(&kind_filter).unwrap().len(),
                all_kind9.len(),
            ),
        ),
        (
            "complete_author",
            mean_allocation_ops(
                iterations,
                || store.query(&author_filter).unwrap().len(),
                all_rows
                    .iter()
                    .filter(|row| row.event.pubkey == author)
                    .count(),
            ),
        ),
        (
            "complete_author_kind",
            mean_allocation_ops(
                iterations,
                || store.query(&author_kind_filter).unwrap().len(),
                author_kind_rows,
            ),
        ),
        (
            "complete_authors_43",
            mean_allocation_ops(
                iterations,
                || store.query(&authors_43_filter).unwrap().len(),
                authors_43_rows,
            ),
        ),
        (
            "rejected_search",
            mean_allocation_ops(
                iterations,
                || store.query_newest(&search_filter, 1).unwrap().len(),
                search_rows.min(1),
            ),
        ),
    ];
    drop(store);

    let started = Instant::now();
    let reopened = RedbStore::open(&path).expect("reopen redb store");
    let reopened_rows = reopened
        .query_newest(&room_member_filter, 200)
        .expect("reopened first room/member query");
    let reopened_first_ms = started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(reopened_rows.len(), room_member_rows.min(200));

    println!("store={}", path.display());
    println!("all_rows={}", all_rows.len());
    println!("kind9_rows={}", all_kind9.len());
    println!("room={room}");
    println!("room_rows={room_rows}");
    println!("member={member}");
    println!("room_member_rows={room_member_rows}");
    println!("authors_43={}", authors_43.len());
    println!("authors_43_populated_rows={authors_43_rows}");
    println!("iterations={iterations}");
    println!("bounded_room_mean_ms={bounded_room_ms:.3}");
    println!("bounded_room_member_mean_ms={bounded_room_member_ms:.3}");
    println!("complete_room_mean_ms={complete_room_ms:.3}");
    println!("complete_room_member_mean_ms={complete_room_member_ms:.3}");
    println!("complete_global_mean_ms={complete_global_ms:.3}");
    println!("complete_kind_mean_ms={complete_kind_ms:.3}");
    println!("complete_author_mean_ms={complete_author_ms:.3}");
    println!("complete_author_kind_mean_ms={complete_author_kind_ms:.3}");
    println!("complete_authors_43_mean_ms={complete_authors_43_ms:.3}");
    println!("rejected_search_mean_ms={rejected_search_ms:.3}");
    for (name, allocations) in allocation_matrix {
        println!("{name}_mean_allocation_ops={allocations:.1}");
    }
    println!("reopened_first_room_member_ms={reopened_first_ms:.3}");
}
