//! Deterministic-shape million-row scale proof for the persistent store.
//!
//! The generated events are genuinely signed. Event ids, authors, timestamps,
//! kinds, tags, and content are deterministic; Schnorr auxiliary randomness is
//! intentionally allowed to vary because it has no effect on store shape or
//! query semantics.
//!
//! Usage:
//! `cargo run -p nmp-store --release --features bench-instrumentation --example scale_bench -- <store.redb> [canonical_rows] [iterations]`

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nmp_store::{EventStore, RedbStore, RelayObserved};
use nostr::{
    Alphabet, Event, EventBuilder, Filter, Keys, Kind, PublicKey, RelayUrl, SingleLetterTag, Tag,
    Timestamp,
};
use serde::{Deserialize, Serialize};

struct CountingAllocator;

static ALLOCATION_OPS: AtomicU64 = AtomicU64::new(0);

// SAFETY: every allocation is delegated unchanged to the system allocator.
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

const GENERATOR_VERSION: &str = "nmp-scale-v2";
const EVENT_SHAPE_VERSION: &str = "nmp-scale-v1";
const DEFAULT_CANONICAL_ROWS: u64 = 1_000_000;
const AUTHOR_COUNT: usize = 1_024;
const TAIL_ROOM_COUNT: u64 = 4_096;
const BATCH_SIZE: usize = 4_096;
const BASE_CREATED_AT: u64 = 1_700_000_000;
const HOT_ROOM: &str = "nmp-scale-hot-room";
const SECOND_ROOM: &str = "nmp-scale-second-room";
const DISCOVERY_ORDINAL: u64 = 1_024;
const DISCOVERY_RELAY: &str = "wss://nmp-device-proof.invalid";
const EXPECTED_MILLION_HOT_ROOM_ROWS: usize = 59_915;
const EXPECTED_MILLION_HOT_ROOM_TOP_200_NEWEST_ID: &str =
    "4718e3ccdff3511ade3b5b96b3a2f8561afc888f7b78ed2ff2c698e910743cc8";

#[derive(Default)]
struct BuildStats {
    insert_attempts: u64,
    batches: u64,
    generation: Duration,
    insertion: Duration,
}

#[derive(Debug, Serialize, Deserialize)]
struct FixtureManifest {
    generator: String,
    canonical_rows: u64,
    authors: usize,
    tail_rooms: u64,
    batch_size: usize,
    event_shape_seed: u64,
    hot_room: String,
    hot_room_rows: usize,
    hot_room_member_rows: usize,
    hot_room_top_200_newest_id: String,
    discovery_room: String,
    discovery_relay: String,
    discovery_rows: usize,
    discovery_newest_id: String,
}

impl FixtureManifest {
    fn assert_config(&self, canonical_rows: u64) {
        assert_eq!(self.generator, GENERATOR_VERSION);
        assert_eq!(self.canonical_rows, canonical_rows);
        assert_eq!(self.authors, AUTHOR_COUNT);
        assert_eq!(self.tail_rooms, TAIL_ROOM_COUNT);
        assert_eq!(self.batch_size, BATCH_SIZE);
        assert_eq!(self.event_shape_seed, BASE_CREATED_AT);
        assert_eq!(self.hot_room, HOT_ROOM);
        assert_eq!(self.discovery_room, HOT_ROOM);
        assert_eq!(self.discovery_relay, DISCOVERY_RELAY);
        assert_eq!(self.discovery_rows, 1);
        assert_million_contract(
            canonical_rows,
            self.hot_room_rows,
            &self.hot_room_top_200_newest_id,
        );
    }
}

fn manifest_path(store_path: &Path) -> PathBuf {
    let mut path = store_path.as_os_str().to_os_string();
    path.push(".manifest.json");
    PathBuf::from(path)
}

struct GeneratedEvent {
    event: Event,
    redeliver: bool,
}

fn deterministic_keys(index: usize) -> Keys {
    Keys::parse(&format!("{:064x}", index + 1)).expect("small nonzero scalar is a secret key")
}

fn tag(name: &str, value: impl Into<String>) -> Tag {
    Tag::parse([name.to_owned(), value.into()]).expect("two-field benchmark tag")
}

fn sign(keys: &Keys, kind: Kind, created_at: u64, tags: Vec<Tag>, content: String) -> Event {
    EventBuilder::new(kind, content)
        .tags(tags)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign benchmark event")
}

fn author_index(ordinal: u64) -> usize {
    // Half the traffic belongs to 32 highly active authors; the other half
    // spreads over the long tail. This intentionally makes both the author
    // and kind indexes contain large, uneven ranges.
    if ordinal % 100 < 50 {
        (ordinal as usize) % 32
    } else {
        32 + (ordinal.wrapping_mul(6_364_136_223_846_793_005) as usize % (AUTHOR_COUNT - 32))
    }
}

fn ordinary_event(ordinal: u64, canonical_rows: u64, authors: &[Keys], hot_member: &str) -> Event {
    let author = author_index(ordinal);
    let keys = &authors[author];
    let created_at = BASE_CREATED_AT + ordinal * 4 + 2;
    let distribution_slot = ordinal % 10;
    let kind = match distribution_slot {
        0..=5 => Kind::from(9u16),
        6..=7 => Kind::TextNote,
        8 => Kind::from(7u16),
        _ => Kind::from(11u16),
    };
    let mut tags = Vec::with_capacity(4);
    if kind == Kind::from(9u16) {
        let room_slot = ordinal / 10;
        let room = if room_slot.is_multiple_of(10) {
            HOT_ROOM.to_owned()
        } else if room_slot.is_multiple_of(5) {
            SECOND_ROOM.to_owned()
        } else {
            format!(
                "nmp-scale-room-{}",
                ordinal.wrapping_mul(1_140_071_481_932_319_849) % TAIL_ROOM_COUNT
            )
        };
        let is_hot = room == HOT_ROOM;
        tags.push(tag("h", room));
        if room_slot.is_multiple_of(4) {
            let member = if is_hot {
                hot_member.to_owned()
            } else {
                authors[(author + 17) % AUTHOR_COUNT].public_key().to_hex()
            };
            tags.push(tag("p", member));
        }
        if room_slot.is_multiple_of(3) {
            tags.push(tag(
                "e",
                format!("{:064x}", ordinal.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            ));
        }
    }
    if ordinal % 10_000 == 7_777 {
        tags.push(Tag::expiration(Timestamp::from(
            BASE_CREATED_AT + canonical_rows * 4 + 86_400,
        )));
    }
    sign(
        keys,
        kind,
        created_at,
        tags,
        format!(
            "{EVENT_SHAPE_VERSION} ordinal={ordinal} author={author} payload={:016x}",
            ordinal.wrapping_mul(0xd6e8_feb8_6659_fd93)
        ),
    )
}

fn discovery_event(authors: &[Keys]) -> Event {
    sign(
        &authors[0],
        Kind::from(39_000u16),
        BASE_CREATED_AT + DISCOVERY_ORDINAL * 4 + 2,
        vec![
            tag("d", HOT_ROOM),
            tag("name", "NMP Scale Hot Room"),
            tag(
                "about",
                "Deterministic million-row proof room for bounded NMP store queries",
            ),
        ],
        format!("{GENERATOR_VERSION} discovery={HOT_ROOM}"),
    )
}

fn events_for_ordinal(
    ordinal: u64,
    canonical_rows: u64,
    authors: &[Keys],
    hot_member: &str,
) -> Vec<GeneratedEvent> {
    let mut out = Vec::with_capacity(4);
    let author = author_index(ordinal);
    let keys = &authors[author];
    let created_at = BASE_CREATED_AT + ordinal * 4;

    // Extra already-expired attempts prove the NIP-40 refusal path without
    // changing the requested final cardinality.
    if ordinal % 100_000 == 99_999 {
        out.push(GeneratedEvent {
            event: sign(
                keys,
                Kind::TextNote,
                created_at,
                vec![Tag::expiration(Timestamp::from(created_at + 1))],
                format!("{EVENT_SHAPE_VERSION} expired={ordinal}"),
            ),
            redeliver: false,
        });
    }

    if ordinal < 512 {
        // One durable winner per profile address, preceded by an older value.
        let profile_keys = &authors[ordinal as usize];
        for (revision, timestamp) in [("old", created_at), ("winner", created_at + 1)] {
            out.push(GeneratedEvent {
                event: sign(
                    profile_keys,
                    Kind::Metadata,
                    timestamp,
                    Vec::new(),
                    format!(r#"{{"name":"scale-{ordinal}-{revision}"}}"#),
                ),
                redeliver: revision == "winner",
            });
        }
    } else if ordinal < 1_024 {
        // Parameterized replaceable rows exercise the address index and
        // lockstep supersession of every query index.
        let address_keys = &authors[(ordinal - 512) as usize];
        let d = format!("scale-list-{}", ordinal - 512);
        for (revision, timestamp) in [("old", created_at), ("winner", created_at + 1)] {
            out.push(GeneratedEvent {
                event: sign(
                    address_keys,
                    Kind::from(30_000u16),
                    timestamp,
                    vec![tag("d", d.clone())],
                    format!("{EVENT_SHAPE_VERSION} address={d} revision={revision}"),
                ),
                redeliver: revision == "winner",
            });
        }
    } else if ordinal.is_multiple_of(50_000) {
        // The final kind:5 row replaces a same-author target in the canonical
        // count, so this ordinal still contributes exactly one surviving row.
        let target = sign(
            keys,
            Kind::TextNote,
            created_at,
            Vec::new(),
            format!("{EVENT_SHAPE_VERSION} deletion-target={ordinal}"),
        );
        let deletion = sign(
            keys,
            Kind::EventDeletion,
            created_at + 1,
            vec![Tag::event(target.id)],
            String::new(),
        );
        out.push(GeneratedEvent {
            event: target,
            redeliver: false,
        });
        out.push(GeneratedEvent {
            event: deletion,
            redeliver: true,
        });
    } else if ordinal == DISCOVERY_ORDINAL {
        out.push(GeneratedEvent {
            event: discovery_event(authors),
            redeliver: false,
        });
    } else {
        out.push(GeneratedEvent {
            event: ordinary_event(ordinal, canonical_rows, authors, hot_member),
            redeliver: ordinal.is_multiple_of(10),
        });
    }
    out
}

fn flush_batch(
    store: &mut RedbStore,
    batch: &mut Vec<(Event, RelayObserved)>,
    stats: &mut BuildStats,
) {
    if batch.is_empty() {
        return;
    }
    let batch_len = batch.len();
    stats.insert_attempts += batch_len as u64;
    let started = Instant::now();
    let outcomes = store
        .insert_batch(std::mem::take(batch))
        .expect("insert scale batch");
    stats.insertion += started.elapsed();
    stats.batches += 1;
    assert_eq!(outcomes.len(), batch_len);
}

fn build_fixture(path: &Path, canonical_rows: u64) -> BuildStats {
    assert!(
        canonical_rows >= 10_000,
        "scale fixture needs at least 10,000 rows"
    );
    let authors: Vec<_> = (0..AUTHOR_COUNT).map(deterministic_keys).collect();
    let hot_member = authors[AUTHOR_COUNT - 1].public_key().to_hex();
    let relays: Vec<_> = (0..8)
        .map(|index| {
            RelayUrl::parse(&format!("wss://scale-{index}.benchmark.invalid"))
                .expect("benchmark relay URL")
        })
        .collect();
    let discovery_relay = RelayUrl::parse(DISCOVERY_RELAY).expect("proof relay URL");
    let mut store = RedbStore::open(path).expect("create scale store");
    let mut stats = BuildStats::default();
    let mut batch = Vec::with_capacity(BATCH_SIZE + BATCH_SIZE / 4);

    for ordinal in 0..canonical_rows {
        let started = Instant::now();
        let generated = events_for_ordinal(ordinal, canonical_rows, &authors, &hot_member);
        stats.generation += started.elapsed();
        for generated in generated {
            let observed_at = Timestamp::from(generated.event.created_at.as_secs() + 10);
            let primary = (ordinal as usize) % relays.len();
            let relay = if generated.event.kind == Kind::from(39_000u16) {
                discovery_relay.clone()
            } else {
                relays[primary].clone()
            };
            batch.push((
                generated.event.clone(),
                RelayObserved::new(relay, observed_at),
            ));
            if generated.redeliver {
                batch.push((
                    generated.event,
                    RelayObserved::new(
                        relays[(primary + 1) % relays.len()].clone(),
                        Timestamp::from(observed_at.as_secs() + 1),
                    ),
                ));
            }
        }
        if batch.len() >= BATCH_SIZE {
            flush_batch(&mut store, &mut batch, &mut stats);
        }
        if (ordinal + 1) % 100_000 == 0 {
            eprintln!(
                "generated_canonical_target={} generation_s={:.3} insertion_s={:.3}",
                ordinal + 1,
                stats.generation.as_secs_f64(),
                stats.insertion.as_secs_f64()
            );
        }
    }
    flush_batch(&mut store, &mut batch, &mut stats);
    stats
}

fn assert_newest_order(rows: &[nmp_store::StoredEvent]) {
    assert!(rows.windows(2).all(|pair| {
        pair[0].event.created_at > pair[1].event.created_at
            || (pair[0].event.created_at == pair[1].event.created_at
                && pair[0].event.id < pair[1].event.id)
    }));
}

fn assert_million_contract(
    canonical_rows: u64,
    hot_room_rows: usize,
    hot_room_top_200_newest_id: &str,
) {
    if canonical_rows == DEFAULT_CANONICAL_ROWS {
        assert_eq!(hot_room_rows, EXPECTED_MILLION_HOT_ROOM_ROWS);
        assert_eq!(
            hot_room_top_200_newest_id,
            EXPECTED_MILLION_HOT_ROOM_TOP_200_NEWEST_ID
        );
    }
}

struct BoundedSummary {
    rows: usize,
    newest_id: String,
}

fn percentile(samples: &mut [Duration], numerator: usize, denominator: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * numerator / denominator]
}

fn bounded_case(
    store: &RedbStore,
    label: &str,
    filter: &Filter,
    limit: usize,
    iterations: u32,
) -> BoundedSummary {
    let warm = store
        .query_newest(filter, limit)
        .expect("warm bounded query");
    assert_newest_order(&warm);
    let expected = warm.len();
    assert!(expected <= limit);

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let started = Instant::now();
        let rows = store
            .query_newest(filter, limit)
            .expect("timed bounded query");
        samples.push(started.elapsed());
        assert_eq!(rows.len(), expected);
        assert_newest_order(&rows);
    }
    let p50 = percentile(&mut samples.clone(), 50, 100);
    let p95 = percentile(&mut samples, 95, 100);

    store.reset_query_work();
    let allocations_before = ALLOCATION_OPS.load(Ordering::Relaxed);
    let rows = store
        .query_newest(filter, limit)
        .expect("instrumented bounded query");
    let allocations = ALLOCATION_OPS
        .load(Ordering::Relaxed)
        .saturating_sub(allocations_before);
    let (index_rows, event_values, materialized_rows) = store.query_work();
    let newest = rows
        .first()
        .map(|row| row.event.id.to_hex())
        .unwrap_or_else(|| "none".to_owned());
    println!("query={label}");
    println!("query_rows={}", rows.len());
    println!("query_newest_id={newest}");
    println!("query_p50_ms={:.3}", p50.as_secs_f64() * 1_000.0);
    println!("query_p95_ms={:.3}", p95.as_secs_f64() * 1_000.0);
    println!("query_index_rows={index_rows}");
    println!("query_event_values={event_values}");
    println!("query_materialized_rows={materialized_rows}");
    println!("query_allocation_ops={allocations}");
    BoundedSummary {
        rows: rows.len(),
        newest_id: newest,
    }
}

fn complete_case(store: &RedbStore, label: &str, filter: &Filter) -> usize {
    store.reset_query_work();
    let allocations_before = ALLOCATION_OPS.load(Ordering::Relaxed);
    let started = Instant::now();
    let rows = store.query(filter).expect("complete query");
    let elapsed = started.elapsed();
    let allocations = ALLOCATION_OPS
        .load(Ordering::Relaxed)
        .saturating_sub(allocations_before);
    let (index_rows, event_values, materialized_rows) = store.query_work();
    println!("complete={label}");
    println!("complete_rows={}", rows.len());
    println!("complete_ms={:.3}", elapsed.as_secs_f64() * 1_000.0);
    println!("complete_index_rows={index_rows}");
    println!("complete_event_values={event_values}");
    println!("complete_materialized_rows={materialized_rows}");
    println!("complete_allocation_ops={allocations}");
    rows.len()
}

fn mean_reopen_ms(path: &Path, iterations: u32) -> f64 {
    let mut elapsed = Duration::ZERO;
    for _ in 0..iterations {
        let started = Instant::now();
        let store = RedbStore::open(path).expect("healthy scale-store reopen");
        elapsed += started.elapsed();
        drop(store);
    }
    elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations)
}

fn main() {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .expect("usage: scale_bench <store.redb> [canonical_rows] [iterations]");
    let canonical_rows: u64 = args
        .next()
        .map(|value| {
            value
                .to_string_lossy()
                .parse()
                .expect("canonical_rows is u64")
        })
        .unwrap_or(DEFAULT_CANONICAL_ROWS);
    let iterations: u32 = args
        .next()
        .map(|value| value.to_string_lossy().parse().expect("iterations is u32"))
        .unwrap_or(30);

    let existed = path.exists();
    let manifest_path = manifest_path(&path);
    let existing_manifest = existed.then(|| {
        let bytes = fs::read(&manifest_path).expect("existing fixture must carry its manifest");
        let manifest: FixtureManifest =
            serde_json::from_slice(&bytes).expect("decode fixture manifest");
        manifest.assert_config(canonical_rows);
        manifest
    });
    let build = if existed {
        None
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        Some(build_fixture(&path, canonical_rows))
    };

    println!("generator={GENERATOR_VERSION}");
    println!("store={}", path.display());
    println!("manifest={}", manifest_path.display());
    println!("reused_existing_store={existed}");
    println!("target_canonical_rows={canonical_rows}");
    println!("authors={AUTHOR_COUNT}");
    println!("tail_rooms={TAIL_ROOM_COUNT}");
    println!("batch_size={BATCH_SIZE}");
    if let Some(stats) = build {
        println!("insert_attempts={}", stats.insert_attempts);
        println!("insert_batches={}", stats.batches);
        println!(
            "generation_ms={:.3}",
            stats.generation.as_secs_f64() * 1_000.0
        );
        println!(
            "insertion_ms={:.3}",
            stats.insertion.as_secs_f64() * 1_000.0
        );
    }
    println!(
        "file_bytes={}",
        fs::metadata(&path).expect("scale store metadata").len()
    );

    let store = RedbStore::open(&path).expect("open scale store for queries");
    let h = SingleLetterTag::lowercase(Alphabet::H);
    let p = SingleLetterTag::lowercase(Alphabet::P);
    let d = SingleLetterTag::lowercase(Alphabet::D);
    let authors: Vec<_> = (0..AUTHOR_COUNT).map(deterministic_keys).collect();
    let hot_member = authors[AUTHOR_COUNT - 1].public_key().to_hex();
    let hot_author: PublicKey = authors[0].public_key();
    let kind9 = Kind::from(9u16);
    let kind39000 = Kind::from(39_000u16);
    let hot_room_filter = Filter::new().kind(kind9).custom_tag(h, HOT_ROOM);
    let discovery_filter = Filter::new().kind(kind39000).custom_tag(d, HOT_ROOM);
    let hot_member_filter = hot_room_filter.clone().custom_tag(p, hot_member.clone());
    let hot_author_filter = Filter::new().author(hot_author);
    let hot_author_kind_filter = hot_author_filter.clone().kind(kind9);
    let authors_43: BTreeSet<_> = authors.iter().take(43).map(Keys::public_key).collect();
    let authors_43_filter = Filter::new().authors(authors_43);

    bounded_case(&store, "global_top_200", &Filter::new(), 200, iterations);
    let discovery_top = bounded_case(
        &store,
        "hot_room_discovery_top_200",
        &discovery_filter,
        200,
        iterations,
    );
    assert_eq!(discovery_top.rows, 1);
    bounded_case(
        &store,
        "kind9_top_200",
        &Filter::new().kind(kind9),
        200,
        iterations,
    );
    let hot_room_top = bounded_case(
        &store,
        "hot_room_top_200",
        &hot_room_filter,
        200,
        iterations,
    );
    bounded_case(
        &store,
        "hot_room_member_top_200",
        &hot_member_filter,
        200,
        iterations,
    );
    bounded_case(
        &store,
        "hot_author_top_200",
        &hot_author_filter,
        200,
        iterations,
    );
    bounded_case(
        &store,
        "hot_author_kind9_top_200",
        &hot_author_kind_filter,
        200,
        iterations,
    );
    bounded_case(
        &store,
        "authors_43_top_200",
        &authors_43_filter,
        200,
        iterations,
    );

    let hot_room_rows = complete_case(&store, "hot_room", &hot_room_filter);
    let discovery_rows = complete_case(&store, "hot_room_discovery", &discovery_filter);
    assert_eq!(discovery_rows, 1);
    let hot_member_rows = complete_case(&store, "hot_room_member", &hot_member_filter);
    let all_rows = complete_case(&store, "global", &Filter::new());
    assert_eq!(
        all_rows as u64, canonical_rows,
        "governed final cardinality"
    );
    assert_million_contract(canonical_rows, hot_room_rows, &hot_room_top.newest_id);
    if let Some(manifest) = existing_manifest {
        assert_eq!(hot_room_rows, manifest.hot_room_rows);
        assert_eq!(hot_member_rows, manifest.hot_room_member_rows);
        assert_eq!(hot_room_top.newest_id, manifest.hot_room_top_200_newest_id);
        assert_eq!(discovery_rows, manifest.discovery_rows);
        assert_eq!(discovery_top.newest_id, manifest.discovery_newest_id);
    } else {
        let manifest = FixtureManifest {
            generator: GENERATOR_VERSION.to_owned(),
            canonical_rows,
            authors: AUTHOR_COUNT,
            tail_rooms: TAIL_ROOM_COUNT,
            batch_size: BATCH_SIZE,
            event_shape_seed: BASE_CREATED_AT,
            hot_room: HOT_ROOM.to_owned(),
            hot_room_rows,
            hot_room_member_rows: hot_member_rows,
            hot_room_top_200_newest_id: hot_room_top.newest_id.clone(),
            discovery_room: HOT_ROOM.to_owned(),
            discovery_relay: DISCOVERY_RELAY.to_owned(),
            discovery_rows,
            discovery_newest_id: discovery_top.newest_id.clone(),
        };
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("encode fixture manifest"),
        )
        .expect("write fixture manifest");
    }
    println!("hot_room_rows={hot_room_rows}");
    println!("hot_room_top_200_newest_id={}", hot_room_top.newest_id);
    println!("hot_room_discovery_rows={discovery_rows}");
    println!("hot_room_discovery_newest_id={}", discovery_top.newest_id);
    println!("hot_room_member_rows={hot_member_rows}");
    drop(store);

    println!(
        "healthy_reopen_mean_ms={:.3}",
        mean_reopen_ms(&path, iterations.min(10))
    );
}
