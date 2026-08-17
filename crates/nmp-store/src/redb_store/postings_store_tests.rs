//! Falsifiers for #1817: `RunMeta.live_events` must be derived from a run's
//! dictionary and its true merged dead set, never trusted from a previously
//! stored value, and `rewrite_run_without_dead` must refuse to delete a run
//! it cannot actually replace rather than sharing `compact_cohort`'s
//! tolerance for "no output."
//!
//! These pokes at raw `redb::Database` transactions live in their own
//! `_tests.rs` file -- rather than inline in `postings_store.rs` -- because
//! `commit_structure_tests.rs` census-checks every production module that
//! begins or commits a transaction, and `postings_store.rs` deliberately owns
//! no transaction boundary of its own in production; it only ever receives
//! one from its caller.

use std::collections::BTreeMap;

use nostr::{EventBuilder, Keys, Kind, Timestamp};
use redb::{Database, ReadableDatabase};

use super::postings::{DeadKeys, RunMeta};
use super::postings_store::{
    apply_run_deaths, catalog_key, catalog_run_metas, publish_pending, rewrite_run_without_dead,
    PendingEvent, RedbPostingsTxn, CATALOG_RUN_META,
};
use super::schema::{EventKey, POSTINGS_CATALOG};

fn open_db() -> (Database, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir for postings unit test");
    let db = Database::create(dir.path().join("postings-unit.redb"))
        .expect("create postings unit test database");
    (db, dir)
}

fn pending_batch(count: u64) -> BTreeMap<EventKey, PendingEvent> {
    let author = Keys::generate();
    (1..=count)
        .map(|event_key| {
            let event = EventBuilder::new(Kind::TextNote, format!("live-events-{event_key}"))
                .custom_created_at(Timestamp::from(1_000 + event_key))
                .sign_with_keys(&author)
                .expect("sign fixture event");
            (event_key, PendingEvent::prepare(&event, event_key))
        })
        .collect()
}

fn only_run_meta(db: &Database) -> RunMeta {
    let read_txn = db.begin_read().expect("read postings catalog");
    let catalog = read_txn
        .open_table(POSTINGS_CATALOG)
        .expect("open postings catalog");
    let metas = catalog_run_metas(&catalog).expect("decode run catalog");
    assert_eq!(metas.len(), 1, "fixture must publish exactly one run");
    metas[0]
}

/// The falsifier for #1817's first finding: `live_events` used to be
/// maintained by `meta.live_events -= fresh_count`, trusting whatever the
/// stored counter already said. This test deliberately corrupts that stored
/// counter to a wrong-but-still-`encode()`-valid value between two death
/// applications, then proves the *next* death application ignores the
/// corruption entirely and lands on the value derived from the run's
/// dictionary length and its true merged dead set. A drifted counter can no
/// longer compound itself: the very next write recomputes ground truth and
/// overwrites it.
#[test]
fn a_corrupted_live_events_counter_is_overwritten_by_the_next_death_not_compounded() {
    let (db, _dir) = open_db();

    let write_txn = db.begin_write().expect("begin publish txn");
    let events = pending_batch(10);
    publish_pending(&mut RedbPostingsTxn::new(&write_txn), &events).expect("publish fixture run");
    write_txn.commit().expect("commit publish txn");

    // Two ordinary deaths first, so there is a real death block on disk for
    // the corrupted step to have to agree with.
    let write_txn = db.begin_write().expect("begin first death txn");
    apply_run_deaths(
        &mut RedbPostingsTxn::new(&write_txn),
        only_run_meta(&db).run_id,
        vec![1, 2],
    )
    .expect("apply first deaths");
    write_txn.commit().expect("commit first death txn");
    let run_id = only_run_meta(&db).run_id;
    assert_eq!(
        only_run_meta(&db).live_events,
        8,
        "sanity: two deaths out of ten must leave eight live"
    );

    // Corrupt the stored counter directly, bypassing every production write
    // path. 5 is wrong (true live count is 8) but still passes
    // `RunMeta::encode`'s own range check, so this models silent drift
    // rather than a value the format would already refuse.
    let write_txn = db.begin_write().expect("begin corruption txn");
    {
        let mut catalog = write_txn
            .open_table(POSTINGS_CATALOG)
            .expect("open catalog for corruption");
        let mut corrupted = only_run_meta(&db);
        corrupted.live_events = 5;
        catalog
            .insert(
                catalog_key(CATALOG_RUN_META, run_id).as_slice(),
                corrupted
                    .encode()
                    .expect("encode corrupted meta")
                    .as_slice(),
            )
            .expect("write corrupted meta");
    }
    write_txn.commit().expect("commit corruption txn");
    assert_eq!(
        only_run_meta(&db).live_events,
        5,
        "sanity: the corruption must actually be on disk before the real test"
    );

    // One more death. If `live_events` were still decremented from the
    // stored value, this would land on 5 - 1 = 4 (wrong). Derived from the
    // dictionary (10 events) and the true dead set ({1, 2, 3}), the correct
    // answer is 7.
    let write_txn = db.begin_write().expect("begin second death txn");
    apply_run_deaths(&mut RedbPostingsTxn::new(&write_txn), run_id, vec![3])
        .expect("apply second death");
    write_txn.commit().expect("commit second death txn");

    assert_eq!(
        only_run_meta(&db).live_events,
        7,
        "live_events must be derived from the dictionary and the true dead set, \
         not decremented from a stored value that may have drifted"
    );
}

/// The falsifier for #1817's second finding: `rewrite_run_without_dead` used
/// to call `delete_run(old_meta)` unconditionally and only then check
/// whether `stream_compaction_cohort` had produced a replacement, silently
/// destroying the run when it had not. This constructs exactly that "cannot
/// happen" case directly -- a death set that empties a run whose
/// `live_events` says it should not be empty -- and proves the function
/// refuses instead of deleting, and that the refusal happens before
/// `delete_run` runs at all (checked from inside the same, uncommitted
/// transaction).
#[test]
fn rewrite_run_without_dead_refuses_a_death_set_that_empties_a_run_it_should_not() {
    let (db, _dir) = open_db();

    let write_txn = db.begin_write().expect("begin publish txn");
    let events = pending_batch(4);
    publish_pending(&mut RedbPostingsTxn::new(&write_txn), &events).expect("publish fixture run");
    write_txn.commit().expect("commit publish txn");
    let meta = only_run_meta(&db);
    assert_eq!(meta.live_events, 4);

    // Every key the run holds, presented as dead -- inconsistent with
    // `meta.live_events == 4`, which is exactly the precondition violation
    // #1817 flagged as reachable only through drift.
    let dead = DeadKeys::new(vec![1, 2, 3, 4]).expect("build full-run dead set");

    let write_txn = db.begin_write().expect("begin rewrite txn");
    let result = rewrite_run_without_dead(&mut RedbPostingsTxn::new(&write_txn), meta, &dead);
    assert!(
        result.is_err(),
        "rewrite_run_without_dead must refuse a death set that empties a run \
         its own live_events says should survive, not silently delete it"
    );

    // Prove the refusal happened before any destructive write: read the run
    // back from inside this same, still-uncommitted transaction.
    let catalog = write_txn
        .open_table(POSTINGS_CATALOG)
        .expect("reopen catalog inside the rewrite txn");
    let metas = catalog_run_metas(&catalog).expect("decode run catalog");
    assert_eq!(
        metas.len(),
        1,
        "delete_run must not run before stream_compaction_cohort's output is proven valid"
    );
    assert_eq!(metas[0], meta);
}
