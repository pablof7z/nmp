//! Real-corpus logical and physical proof for the packed immutable note codec.
//!
//! Usage:
//! `cargo run -p nmp-store --release --example codec_bench -- events.jsonl`

use std::env;
use std::path::PathBuf;
use std::time::Instant;

use nmp_store::{EventStore, RedbStore, RelayObserved};
use nostr::{Event, JsonUtil, RelayUrl, Timestamp};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

const EVENTS_V6: TableDefinition<u64, &[u8]> = TableDefinition::new("events_v6");
const CODEC_ROWS: TableDefinition<u64, &[u8]> = TableDefinition::new("codec_rows");

fn encode_v3_event(event: &Event) -> Vec<u8> {
    const V3_HEADER_LEN: usize = 158;
    let tags_len = event
        .tags
        .as_slice()
        .iter()
        .map(|tag| {
            4 + tag
                .as_slice()
                .iter()
                .map(|element| 4 + element.len())
                .sum::<usize>()
        })
        .sum::<usize>();
    let mut out = Vec::with_capacity(V3_HEADER_LEN + tags_len + event.content.len());
    out.extend_from_slice(b"NMPE");
    out.push(3);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(event.id.as_bytes());
    out.extend_from_slice(event.pubkey.as_bytes());
    out.extend_from_slice(event.sig.as_ref());
    out.extend_from_slice(&event.created_at.as_secs().to_be_bytes());
    out.extend_from_slice(&event.kind.as_u16().to_be_bytes());
    out.extend_from_slice(&(event.tags.len() as u32).to_be_bytes());
    out.extend_from_slice(&(tags_len as u32).to_be_bytes());
    out.extend_from_slice(&(event.content.len() as u32).to_be_bytes());
    for tag in event.tags.iter() {
        out.extend_from_slice(&(tag.as_slice().len() as u32).to_be_bytes());
        for element in tag.as_slice() {
            out.extend_from_slice(&(element.len() as u32).to_be_bytes());
            out.extend_from_slice(element.as_bytes());
        }
    }
    out.extend_from_slice(event.content.as_bytes());
    out
}

fn percent_smaller(before: u64, after: u64) -> f64 {
    (before.saturating_sub(after)) as f64 * 100.0 / before as f64
}

fn measure_codec_files(
    root: &std::path::Path,
    label: &str,
    rows: &[(u64, Vec<u8>)],
    copies: u64,
) -> Vec<u64> {
    (0..5)
        .map(|repeat| {
            let path = root.join(format!("{label}-{repeat}.redb"));
            let mut db = Database::create(&path).expect("create codec-only redb");
            let write_txn = db.begin_write().expect("begin codec-only write");
            let mut table = write_txn
                .open_table(CODEC_ROWS)
                .expect("open codec-only rows");
            for copy in 0..copies {
                for (key, value) in rows {
                    let key = copy * rows.len() as u64 + key;
                    table
                        .insert(key, value.as_slice())
                        .expect("insert codec-only row");
                }
            }
            drop(table);
            write_txn.commit().expect("commit codec-only redb");
            while db.compact().expect("compact codec-only redb") {}
            drop(db);
            std::fs::metadata(path).unwrap().len()
        })
        .collect()
}

fn main() {
    let input = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: codec_bench events.jsonl");
    let source = std::fs::read_to_string(&input).expect("read event JSONL");
    let events: Vec<Event> = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Event::from_json(line).expect("parse event JSONL row"))
        .collect();
    let input_rows = events.len() as u64;
    let scratch = tempfile::tempdir().expect("codec benchmark tempdir");
    let v3_rows: Vec<_> = events
        .iter()
        .enumerate()
        .map(|(index, event)| (index as u64 + 1, encode_v3_event(event)))
        .collect();
    let v3_value_bytes = v3_rows.iter().map(|(_, value)| value.len() as u64).sum();
    let path = scratch.path().join("packed.redb");
    let relay = RelayUrl::parse("wss://codec-benchmark.invalid").unwrap();
    let mut store = RedbStore::open(&path).expect("open packed store");
    let insert_started = Instant::now();
    store
        .insert_batch(
            events
                .into_iter()
                .map(|event| {
                    (
                        event,
                        RelayObserved::new(relay.clone(), Timestamp::from(1u64)),
                    )
                })
                .collect(),
        )
        .expect("import benchmark corpus");
    let insert_batch_ms = insert_started.elapsed().as_secs_f64() * 1_000.0;
    drop(store);

    let db = Database::create(&path).expect("open packed redb");
    let (rows, v4_value_bytes, event_stats, v4_rows) = {
        let read_txn = db.begin_read().expect("begin packed read");
        let table = read_txn.open_table(EVENTS_V6).expect("open events_v6");
        let mut rows = 0u64;
        let mut value_bytes = 0u64;
        let mut codec_rows = Vec::new();
        for entry in table.iter().expect("iterate packed events") {
            let (key, value) = entry.expect("read packed event");
            rows += 1;
            value_bytes += value.value().len() as u64;
            codec_rows.push((key.value(), value.value().to_vec()));
        }
        (
            rows,
            value_bytes,
            table.stats().expect("events_v6 stats"),
            codec_rows,
        )
    };
    drop(db);
    let v3_codec_files = measure_codec_files(scratch.path(), "v3-events-only", &v3_rows, 1);
    let v4_codec_files = measure_codec_files(scratch.path(), "v4-events-only", &v4_rows, 1);
    assert!(v3_codec_files.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(v4_codec_files.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(
        rows, input_rows,
        "codec comparison corpus must not contain governed supersession/deletion"
    );

    println!("corpus={}", input.display());
    println!("rows={rows}");
    println!("insert_batch_ms={insert_batch_ms:.3}");
    println!("v3_event_value_bytes={v3_value_bytes}");
    println!("v4_event_value_bytes={v4_value_bytes}");
    println!(
        "event_value_percent_smaller={:.3}",
        percent_smaller(v3_value_bytes, v4_value_bytes)
    );
    println!("events_table_stored_bytes={}", event_stats.stored_bytes());
    println!(
        "events_table_metadata_bytes={}",
        event_stats.metadata_bytes()
    );
    println!(
        "events_table_fragmented_bytes={}",
        event_stats.fragmented_bytes()
    );
    println!("v3_codec_files_after_compaction={v3_codec_files:?}");
    println!("v4_codec_files_after_compaction={v4_codec_files:?}");
    println!(
        "codec_file_percent_smaller={:.3}",
        percent_smaller(v3_codec_files[0], v4_codec_files[0])
    );
}
