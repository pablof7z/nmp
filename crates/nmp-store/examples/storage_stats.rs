//! Print physical and logical redb table sizes without opening an NMP schema.
//!
//! This intentionally uses redb's untyped table view, so it can compare a
//! pre-v3 database with the schema-breaking v3 replacement:
//! `cargo run -p nmp-store --release --example storage_stats -- <old.redb> <new.redb>`

use std::env;
use std::path::PathBuf;

use redb::{Database, ReadableDatabase, ReadableTableMetadata, TableHandle};

fn main() {
    let paths: Vec<_> = env::args_os().skip(1).map(PathBuf::from).collect();
    assert!(!paths.is_empty(), "usage: storage_stats <store.redb> [...]");

    for path in paths {
        let db = Database::open(&path).expect("open redb store");
        let write_txn = db.begin_write().expect("begin stats transaction");
        let stats = write_txn.stats().expect("read database stats");
        println!(
            "store={} file_bytes={} stored_bytes={} metadata_bytes={} fragmented_bytes={} page_size={} allocated_pages={}",
            path.display(),
            std::fs::metadata(&path).expect("stat redb file").len(),
            stats.stored_bytes(),
            stats.metadata_bytes(),
            stats.fragmented_bytes(),
            stats.page_size(),
            stats.allocated_pages(),
        );
        drop(write_txn);

        let read_txn = db.begin_read().expect("begin table stats transaction");
        let mut tables = Vec::new();
        for handle in read_txn.list_tables().expect("list redb tables") {
            let name = handle.name().to_owned();
            let table = read_txn
                .open_untyped_table(handle)
                .expect("open untyped redb table");
            let stats = table.stats().expect("read table stats");
            tables.push((
                name,
                table.len().expect("count table rows"),
                stats.stored_bytes(),
                stats.metadata_bytes(),
                stats.fragmented_bytes(),
            ));
        }
        tables.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
        for (name, entries, stored, metadata, fragmented) in tables {
            println!(
                "table={name} entries={entries} stored_bytes={stored} metadata_bytes={metadata} fragmented_bytes={fragmented}"
            );
        }
    }
}
