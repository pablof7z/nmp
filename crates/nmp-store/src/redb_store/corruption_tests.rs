//! #790 falsifiers: a persisted row that does not decode surfaces as a
//! typed [`PersistenceError`] through its owning `EventStore` door, and the
//! process stays alive.
//!
//! Each test corrupts exactly ONE class of persisted row through a raw
//! `redb` handle — the store's own doors cannot write these bytes, which is
//! the point — reopens through the ordinary public constructor, and proves
//! four things about the owning door:
//!
//! 1. it returns `Err`, classified [`PersistenceFault::Invariant`] with
//!    [`DurabilityOutcome::Absent`] (see that variant's doc for why a
//!    decode failure is `Invariant` and not `Corrupted`);
//! 2. it does not panic — asserted directly with `catch_unwind`, because
//!    "returns `Err`" and "does not abort the embedding host" are different
//!    claims and this issue exists because of the second one;
//! 3. `None`/empty/no-op stay distinguishable from corruption — a corrupt
//!    row is never reported as an absent row;
//! 4. a failed mutation commits nothing: the corrupted table's bytes are
//!    byte-identical afterwards.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use nostr::nips::nip01::Coordinate;
use nostr::{EventBuilder, Keys, Tag};
use redb::ReadableDatabase;
use tempfile::TempDir;

use super::postings::Family;
use super::*;
use crate::{sentinel_signature, AcceptWrite, DurabilityOutcome, IntentSigState, PersistenceFault};

const RELAY: &str = "wss://corruption-proof.example";

fn keys() -> Keys {
    Keys::generate()
}

fn relay() -> RelayUrl {
    RelayUrl::parse(RELAY).expect("relay url")
}

fn observed() -> RelayObserved {
    RelayObserved::new(relay(), Timestamp::from(1_000))
}

fn note(keys: &Keys, content: &str, created_at: u64) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign note")
}

fn frozen_from(signed: &Event) -> Event {
    Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    )
}

fn accept_of(frozen: Event) -> AcceptWrite {
    let expected_pubkey = frozen.pubkey;
    AcceptWrite {
        frozen,
        replaceable_base: None,
        monotonic_stamp: false,
        expected_pubkey,
        signing_identity_ref: "local".to_owned(),
        routing: "auto".to_owned(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(1_000),
        correlation: None,
    }
}

/// A temp directory plus the one store path inside it. The store handle is
/// opened and dropped around every raw mutation: `redb` is single-writer per
/// file and the store registry refuses a second live open of the same path.
struct Fixture {
    _dir: TempDir,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corruption.redb");
        Self { _dir: dir, path }
    }

    fn open(&self) -> RedbStore {
        RedbStore::open(&self.path).expect("open store")
    }

    fn raw(&self) -> Database {
        Database::create(&self.path).expect("raw redb handle")
    }
}

/// Replace one `&str`-valued row in place, leaving every other row alone.
fn rewrite_str_row(fixture: &Fixture, table: TableDefinition<&str, &str>, key: &str, value: &str) {
    let db = fixture.raw();
    let write_txn = db.begin_write().expect("raw begin_write");
    {
        let mut open = write_txn.open_table(table).expect("raw open_table");
        open.insert(key, value).expect("raw insert");
    }
    write_txn.commit().expect("raw commit");
}

/// The first key of a `&str`-keyed table — every table this module corrupts
/// is seeded with exactly one row by its test.
fn first_str_key(fixture: &Fixture, table: TableDefinition<&str, &str>) -> String {
    let db = fixture.raw();
    let read_txn = db.begin_read().expect("raw begin_read");
    let open = read_txn.open_table(table).expect("raw open_table");
    let (key, _value) = open
        .first()
        .expect("raw first")
        .expect("table has at least one row");
    key.value().to_owned()
}

/// A stable digest of one `&str`-keyed table, used to prove a refused
/// mutation left the durable bytes untouched.
fn str_table_digest(
    fixture: &Fixture,
    table: TableDefinition<&str, &str>,
) -> Vec<(String, String)> {
    let db = fixture.raw();
    let read_txn = db.begin_read().expect("raw begin_read");
    let open = read_txn.open_table(table).expect("raw open_table");
    open.iter()
        .expect("raw iter")
        .map(|entry| {
            let (key, value) = entry.expect("raw entry");
            (key.value().to_owned(), value.value().to_owned())
        })
        .collect()
}

fn rewrite_fixed_row<const N: usize>(
    fixture: &Fixture,
    table: TableDefinition<&'static [u8; N], &'static [u8]>,
    key: &[u8; N],
    value: &[u8],
) {
    let db = fixture.raw();
    let write_txn = db.begin_write().expect("raw begin_write");
    {
        let mut open = write_txn.open_table(table).expect("raw open_table");
        open.insert(key, value).expect("raw insert");
    }
    write_txn.commit().expect("raw commit");
}

fn first_fixed_key<const N: usize>(
    fixture: &Fixture,
    table: TableDefinition<&'static [u8; N], &'static [u8]>,
) -> [u8; N] {
    let db = fixture.raw();
    let read_txn = db.begin_read().expect("raw begin_read");
    let open = read_txn.open_table(table).expect("raw open_table");
    let (key, _value) = open
        .first()
        .expect("raw first")
        .expect("table has at least one row");
    *key.value()
}

fn fixed_table_digest<const N: usize>(
    fixture: &Fixture,
    table: TableDefinition<&'static [u8; N], &'static [u8]>,
) -> Vec<([u8; N], Vec<u8>)> {
    let db = fixture.raw();
    let read_txn = db.begin_read().expect("raw begin_read");
    let open = read_txn.open_table(table).expect("raw open_table");
    open.iter()
        .expect("raw iter")
        .map(|entry| {
            let (key, value) = entry.expect("raw entry");
            (*key.value(), value.value().to_vec())
        })
        .collect()
}

/// Assert the shape every #790 door must answer with: an `Err` (not a
/// panic, not an empty success) classified as an invariant violation whose
/// durability claim — nothing committed — is actually true, because the
/// decode always precedes the commit.
#[track_caller]
fn assert_typed_refusal<T: std::fmt::Debug>(
    what: &str,
    call: impl FnOnce() -> Result<T, PersistenceError>,
) -> PersistenceError {
    let outcome = catch_unwind(AssertUnwindSafe(call))
        .unwrap_or_else(|_| panic!("{what} panicked the host instead of reporting corruption"));
    let error = match outcome {
        Ok(value) => panic!("{what} returned {value:?} for a corrupt persisted row"),
        Err(error) => error,
    };
    assert_eq!(
        error.fault(),
        PersistenceFault::Invariant,
        "{what}: a row this crate cannot decode is an invariant violation, not a backend fault"
    );
    assert_eq!(
        error.durability(),
        DurabilityOutcome::Absent,
        "{what}: the decode precedes the commit, so nothing landed"
    );
    error
}

// -------------------------------------------------------------- delivery

/// #909: the complete bootstrap return value must be decoded before commit.
///
/// A malformed existing lane must be decoded before bootstrap stages or
/// commits anything. The fixed door refuses against the same write
/// transaction, leaving the raw corrupt fixture byte-identical across two
/// fresh opens.
#[test]
fn bootstrap_lane_prefix_invariant_is_absent_across_two_reopens() {
    let fixture = Fixture::new();
    let keys = keys();
    let signed = note(&keys, "bootstrap-prefix-invariant", 990);
    let intent = {
        let mut store = fixture.open();
        let accepted = store
            .accept_write(accept_of(frozen_from(&signed)))
            .expect("accept bootstrap fixture");
        let intent = accepted.journaled_intent_id().expect("durable intent");
        store
            .promote_signed(intent, signed.sig)
            .expect("promote bootstrap fixture");
        store
            .record_route_revision(
                intent,
                BTreeSet::from([
                    RelayUrl::parse("wss://a.bootstrap-prefix.example").unwrap(),
                    RelayUrl::parse("wss://b.bootstrap-prefix.example").unwrap(),
                ]),
            )
            .expect("record two routed relays");
        store
            .bootstrap_publish_queue_lanes(intent)
            .expect("bootstrap initial lanes");
        intent
    };

    let storage_key = first_fixed_key(&fixture, PUBLISH_QUEUE_LANES);
    rewrite_fixed_row(
        &fixture,
        PUBLISH_QUEUE_LANES,
        &storage_key,
        b"NMPL-truncated",
    );
    let before = fixed_table_digest(&fixture, PUBLISH_QUEUE_LANES);

    for reopen in 1..=2 {
        let mut store = fixture.open();
        let error = assert_typed_refusal("bootstrap_publish_queue_lanes", || {
            store.bootstrap_publish_queue_lanes(intent)
        });
        assert!(
            error.message().contains("lane"),
            "reopen {reopen}: exact seeded corruption must remain visible: {error}"
        );
        drop(store);
        assert_eq!(
            fixed_table_digest(&fixture, PUBLISH_QUEUE_LANES),
            before,
            "reopen {reopen}: Absent must mean no valid lane was committed"
        );
    }
}

/// The highest-value single conversion in #790: boot-time journal replay.
#[test]
fn recover_publish_queue_reports_a_corrupt_intent_row() {
    let fixture = Fixture::new();
    let keys = keys();
    let signed = note(&keys, "corrupt-intent", 1_000);
    {
        let mut store = fixture.open();
        store
            .accept_write(accept_of(frozen_from(&signed)))
            .expect("accept_write");
    }
    let key = first_fixed_key(&fixture, PUBLISH_QUEUE_INTENTS);
    rewrite_fixed_row(&fixture, PUBLISH_QUEUE_INTENTS, &key, b"NMPI-truncated");

    let store = fixture.open();
    let error = assert_typed_refusal("recover_publish_queue", || store.recover_publish_queue());
    assert!(
        error.message().contains("intent"),
        "the error must name the row it failed on: {error}"
    );
}

/// The journal row itself decodes; the frozen event it carries does not.
/// Distinct from the row-level failure above and separately reachable.
#[test]
fn recover_publish_queue_reports_a_corrupt_frozen_event() {
    let fixture = Fixture::new();
    let keys = keys();
    let signed = note(&keys, "corrupt-frozen", 1_000);
    {
        let mut store = fixture.open();
        store
            .accept_write(accept_of(frozen_from(&signed)))
            .expect("accept_write");
    }
    let key = first_fixed_key(&fixture, PUBLISH_QUEUE_INTENTS);
    let intact = fixed_table_digest(&fixture, PUBLISH_QUEUE_INTENTS);
    let mut record = intact[0].1.clone();
    // Intent envelope (8) + receipt id (8) + event byte length (4).
    record[20] ^= 0xff;
    rewrite_fixed_row(&fixture, PUBLISH_QUEUE_INTENTS, &key, &record);

    let store = fixture.open();
    let error = assert_typed_refusal("recover_publish_queue", || store.recover_publish_queue());
    assert!(
        error.message().contains("intent"),
        "the error must name the frozen event: {error}"
    );
}

/// An empty journal and an unreadable journal are different facts, and the
/// engine's boot path branches on exactly this distinction.
#[test]
fn an_empty_publish_queue_store_stays_distinguishable_from_an_unreadable_one() {
    let fixture = Fixture::new();
    let store = fixture.open();
    assert_eq!(
        store
            .recover_publish_queue()
            .expect("healthy recover_publish_queue"),
        Vec::new()
    );
}

/// The displaced predecessor snapshot is a separate binary value with its
/// own decoder; corrupting it must not be reported as "nothing displaced".
#[test]
fn recover_publish_queue_reports_a_corrupt_displaced_snapshot() {
    let fixture = Fixture::new();
    let keys = keys();
    let first = EventBuilder::new(Kind::Metadata, "first")
        .custom_created_at(Timestamp::from(1_000))
        .sign_with_keys(&keys)
        .expect("sign first");
    let second = EventBuilder::new(Kind::Metadata, "second")
        .custom_created_at(Timestamp::from(2_000))
        .sign_with_keys(&keys)
        .expect("sign second");
    {
        let mut store = fixture.open();
        store
            .insert(first, observed())
            .expect("insert relay-observed predecessor");
        store
            .accept_write(accept_of(frozen_from(&second)))
            .expect("accept second");
    }

    let displaced_key = {
        let db = fixture.raw();
        let read_txn = db.begin_read().expect("raw begin_read");
        let open = read_txn
            .open_table(PUBLISH_QUEUE_DISPLACED)
            .expect("raw open displaced");
        let (key, _value) = open
            .first()
            .expect("raw first")
            .expect("supersession stashes a predecessor");
        key.value().to_owned()
    };
    {
        let db = fixture.raw();
        let write_txn = db.begin_write().expect("raw begin_write");
        {
            let mut open = write_txn
                .open_table(PUBLISH_QUEUE_DISPLACED)
                .expect("raw open displaced");
            open.insert(&displaced_key, b"NMPC-truncated".as_slice())
                .expect("raw insert");
        }
        write_txn.commit().expect("raw commit");
    }

    let store = fixture.open();
    let error = assert_typed_refusal("recover_publish_queue", || store.recover_publish_queue());
    assert!(
        error.message().contains("displaced event"),
        "the error must name the displaced snapshot: {error}"
    );
}

/// A retained receipt row that will not decode is already fallible on
/// master; this pins that `reattach_receipt` keeps the same classification
/// the newly-converted doors use, so an embedder branches on one rule.
#[test]
fn reattach_receipt_reports_a_corrupt_receipt_row() {
    let fixture = Fixture::new();
    let keys = keys();
    let signed = note(&keys, "corrupt-receipt", 1_000);
    let receipt_id = {
        let mut store = fixture.open();
        store
            .accept_write(accept_of(frozen_from(&signed)))
            .expect("accept_write")
            .journaled_receipt_id()
            .expect("accepted write journals a receipt")
    };
    let key = first_fixed_key(&fixture, PUBLISH_QUEUE_RECEIPTS);
    rewrite_fixed_row(&fixture, PUBLISH_QUEUE_RECEIPTS, &key, b"NMPR-truncated");

    let store = fixture.open();
    assert_typed_refusal("reattach_receipt", || store.reattach_receipt(receipt_id));
    assert_eq!(
        store
            .reattach_receipt(receipt_id + 1_000)
            .expect("an unknown receipt id is a healthy None"),
        None,
        "absence and corruption must stay different answers"
    );
}

/// Promotion reads this intent's own kind:5 suppression claims back before
/// it can close them. A corrupt claim list must refuse the promotion whole.
#[test]
fn promote_reports_a_corrupt_kind5_claim_record() {
    let fixture = Fixture::new();
    let keys = keys();
    let target = note(&keys, "deletion-target", 1_000);
    let deletion = EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::event(target.id))
        .custom_created_at(Timestamp::from(2_000))
        .sign_with_keys(&keys)
        .expect("sign deletion");
    let intent_id = {
        let mut store = fixture.open();
        store.insert(target.clone(), observed()).expect("insert");
        store
            .accept_write(accept_of(frozen_from(&deletion)))
            .expect("accept deletion")
            .journaled_intent_id()
            .expect("accepted deletion journals an intent")
    };
    let key = first_fixed_key(&fixture, PUBLISH_QUEUE_KIND5_CLAIMS);
    rewrite_fixed_row(
        &fixture,
        PUBLISH_QUEUE_KIND5_CLAIMS,
        &key,
        b"NMPK-truncated",
    );
    let before = fixed_table_digest(&fixture, PUBLISH_QUEUE_INTENTS);

    {
        let mut store = fixture.open();
        assert_typed_refusal("promote_signed", || {
            store.promote_signed(intent_id, deletion.sig)
        });
    }
    assert_eq!(
        fixed_table_digest(&fixture, PUBLISH_QUEUE_INTENTS),
        before,
        "a refused promotion commits none of its own journal transition"
    );
}

/// The claimant set is read at ingest time to decide visibility. A corrupt
/// set must not read as "no claimant", which would reveal a suppressed row.
#[test]
fn accept_reports_a_corrupt_suppression_claimant_set() {
    let fixture = Fixture::new();
    let keys = keys();
    let target = note(&keys, "claimant-target", 1_000);
    let deletion = EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::event(target.id))
        .custom_created_at(Timestamp::from(2_000))
        .sign_with_keys(&keys)
        .expect("sign deletion");
    {
        let mut store = fixture.open();
        store.insert(target.clone(), observed()).expect("insert");
        store
            .accept_write(accept_of(frozen_from(&deletion)))
            .expect("accept deletion");
    }
    let key = first_fixed_key(&fixture, PUBLISH_QUEUE_SUPPRESS_BY_ID);
    rewrite_fixed_row(
        &fixture,
        PUBLISH_QUEUE_SUPPRESS_BY_ID,
        &key,
        b"NMPS-truncated",
    );

    let mut store = fixture.open();
    let second = EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::event(target.id))
        .custom_created_at(Timestamp::from(3_000))
        .sign_with_keys(&keys)
        .expect("sign second deletion");
    assert_typed_refusal("accept_write", || {
        store.accept_write(accept_of(frozen_from(&second)))
    });
}

/// Address tombstones gate every future arrival at that address, so a
/// corrupt ceiling must refuse the ingest rather than silently admit a row
/// a permanent deletion already covers.
#[test]
fn insert_reports_a_corrupt_address_tombstone() {
    let fixture = Fixture::new();
    let keys = keys();
    let addressable = EventBuilder::new(Kind::Metadata, "addressable")
        .custom_created_at(Timestamp::from(1_000))
        .sign_with_keys(&keys)
        .expect("sign addressable");
    let deletion = EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::coordinate(
            Coordinate::new(Kind::Metadata, keys.public_key()),
            None,
        ))
        .custom_created_at(Timestamp::from(2_000))
        .sign_with_keys(&keys)
        .expect("sign address deletion");
    {
        let mut store = fixture.open();
        store
            .insert(addressable.clone(), observed())
            .expect("insert addressable");
        store.insert(deletion, observed()).expect("insert deletion");
    }
    let key = first_str_key(&fixture, ADDR_TOMBSTONES);
    rewrite_str_row(&fixture, ADDR_TOMBSTONES, &key, "{ not a tombstone");
    let before = str_table_digest(&fixture, ADDR_TOMBSTONES);

    let later = EventBuilder::new(Kind::Metadata, "later")
        .custom_created_at(Timestamp::from(3_000))
        .sign_with_keys(&keys)
        .expect("sign later");
    {
        let mut store = fixture.open();
        assert_typed_refusal("insert", || store.insert(later, observed()));
    }
    assert_eq!(
        str_table_digest(&fixture, ADDR_TOMBSTONES),
        before,
        "a refused ingest commits nothing"
    );
}

// -------------------------------------------------------------- coverage

/// `record_coverage` merges against the persisted interval inside its own
/// write transaction: a corrupt row must refuse the merge, never silently
/// re-base the watermark on a defaulted window.
#[test]
fn record_coverage_and_gc_report_a_corrupt_coverage_row() {
    let fixture = Fixture::new();
    let keys = keys();
    let atom = healthy_atom(&keys);
    {
        let mut store = fixture.open();
        store
            .record_coverage(&[(
                atom.clone(),
                relay(),
                CoverageInterval::new(Timestamp::from(10), Timestamp::from(20)),
            )])
            .expect("record_coverage");
    }
    let key = first_str_key(&fixture, COVERAGE);
    rewrite_str_row(&fixture, COVERAGE, &key, "{ not a coverage row");
    let before = str_table_digest(&fixture, COVERAGE);

    {
        let mut store = fixture.open();
        assert_typed_refusal("record_coverage", || {
            store.record_coverage(&[(
                atom.clone(),
                relay(),
                CoverageInterval::new(Timestamp::from(30), Timestamp::from(40)),
            )])
        });
    }
    assert_eq!(
        str_table_digest(&fixture, COVERAGE),
        before,
        "a refused record_coverage merges nothing"
    );

    {
        let mut store = fixture.open();
        assert_typed_refusal("gc", || store.gc(&GcRetentionSet::default()));
    }
    assert_eq!(
        str_table_digest(&fixture, COVERAGE),
        before,
        "a refused gc shrinks nothing"
    );
}

// ------------------------------------------------------------- canonical

/// The id fast path used to `.expect` this decode. Answering "no such
/// event" instead would be a false miss on a store that still holds a row
/// for that id — the exact semantic the issue forbids.
#[test]
fn query_reports_a_corrupt_canonical_event() {
    let fixture = Fixture::new();
    let keys = keys();
    let event = note(&keys, "corrupt-canonical", 1_000);
    {
        let mut store = fixture.open();
        store.insert(event.clone(), observed()).expect("insert");
    }
    // The first event stored in a fresh store owns surrogate key 1.
    let event_key: EventKey = 1;
    {
        let db = fixture.raw();
        let write_txn = db.begin_write().expect("raw begin_write");
        {
            let mut events = write_txn.open_table(EVENTS).expect("raw open events");
            events
                .insert(event_key, b"NMPE-truncated".as_slice())
                .expect("raw insert");
        }
        write_txn.commit().expect("raw commit");
    }

    let store = fixture.open();
    let by_id = Filter::new().id(event.id);
    assert_typed_refusal("query (id fast path)", || store.query(&by_id));
    assert_typed_refusal("query (ordered path)", || store.query(&Filter::new()));

    // A genuinely unknown id is still an ordinary empty answer.
    let unknown = note(&keys, "never-stored", 9_000);
    assert_eq!(
        store
            .query(&Filter::new().id(unknown.id))
            .expect("unknown id is a healthy empty result"),
        Vec::new()
    );
}

// -------------------------------------------------------- packed postings

fn packed_run_ids(fixture: &Fixture) -> Vec<u64> {
    let db = fixture.raw();
    let read_txn = db.begin_read().expect("raw begin_read");
    let open = read_txn
        .open_table(POSTINGS_RUN_META)
        .expect("raw open run meta");
    open.iter()
        .expect("raw iter")
        .map(|entry| entry.expect("raw entry").0.value())
        .collect()
}

/// Seed a store with one packed run holding several postings in the global
/// segment. `insert_batch` is deliberate: one governed transaction
/// publishes one run, which is what makes a within-run ordering corruption
/// expressible at all.
fn seeded_packed_store(fixture: &Fixture) -> Vec<Event> {
    let keys = keys();
    let events: Vec<_> = (0..4)
        .map(|index| note(&keys, &format!("packed-{index}"), 1_000 + index))
        .collect();
    let mut store = fixture.open();
    store
        .insert_batch(
            events
                .iter()
                .map(|event| (event.clone(), observed()))
                .collect(),
        )
        .expect("insert_batch");
    events
}

fn global_segment_key(run_id: u64) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = Family::Global as u8;
    key[1] = 0;
    key[2..].copy_from_slice(&run_id.to_be_bytes());
    key
}

fn read_packed_bytes(fixture: &Fixture, run_id: u64, dictionary: bool) -> Vec<u8> {
    let db = fixture.raw();
    let read_txn = db.begin_read().expect("raw begin_read");
    if dictionary {
        let open = read_txn
            .open_table(POSTINGS_DICTIONARIES)
            .expect("raw open dictionaries");
        open.get(run_id)
            .expect("raw get")
            .expect("run has a dictionary")
            .value()
            .to_vec()
    } else {
        let open = read_txn
            .open_table(POSTINGS_SEGMENTS)
            .expect("raw open segments");
        open.get(global_segment_key(run_id).as_slice())
            .expect("raw get")
            .expect("run has a global segment")
            .value()
            .to_vec()
    }
}

fn write_packed_bytes(fixture: &Fixture, run_id: u64, dictionary: bool, bytes: &[u8]) {
    let db = fixture.raw();
    let write_txn = db.begin_write().expect("raw begin_write");
    {
        if dictionary {
            let mut open = write_txn
                .open_table(POSTINGS_DICTIONARIES)
                .expect("raw open dictionaries");
            open.insert(run_id, bytes).expect("raw insert");
        } else {
            let mut open = write_txn
                .open_table(POSTINGS_SEGMENTS)
                .expect("raw open segments");
            open.insert(global_segment_key(run_id).as_slice(), bytes)
                .expect("raw insert");
        }
    }
    write_txn.commit().expect("raw commit");
}

/// Structurally valid, semantically wrong: the dictionary is exactly the
/// right width and parses cleanly, but two entries are transposed so the
/// keys are no longer strictly ordered. `DictionaryView::parse` accepts
/// this; only `validate_order` catches it, and before #790 the production
/// scan never called it.
#[test]
fn packed_scan_reports_an_unsorted_dictionary() {
    let fixture = Fixture::new();
    let events = seeded_packed_store(&fixture);
    assert!(!events.is_empty());
    let run_id = *packed_run_ids(&fixture).first().expect("one packed run");

    let mut bytes = read_packed_bytes(&fixture, run_id, true);
    const HEADER: usize = 12;
    const ENTRY: usize = 40;
    assert!(
        bytes.len() >= HEADER + 2 * ENTRY,
        "need two entries to swap"
    );
    for offset in 0..ENTRY {
        bytes.swap(HEADER + offset, HEADER + ENTRY + offset);
    }
    write_packed_bytes(&fixture, run_id, true, &bytes);

    let store = fixture.open();
    let error = assert_typed_refusal("query over an unsorted packed dictionary", || {
        store.query(&Filter::new())
    });
    assert!(
        error
            .message()
            .contains("dictionary keys are not strictly ordered"),
        "the error must name the semantic violation: {error}"
    );
}

/// The same class one level down: the posting list is the right width and
/// parses, but two postings are transposed. `PostingListView::cursor`
/// binary-searches this list, so without validation the corruption lands
/// the cursor in the wrong place and the query answers with a FALSE MISS
/// rather than an error.
#[test]
fn packed_scan_reports_an_out_of_order_posting_list() {
    let fixture = Fixture::new();
    let events = seeded_packed_store(&fixture);
    let run_id = *packed_run_ids(&fixture).first().expect("one packed run");

    let mut bytes = read_packed_bytes(&fixture, run_id, false);
    // Global segment: one prefix record, empty prefix. Header is 14 bytes,
    // then a single 4-byte offset, then `[prefix_len:u32][posting_count:u32]`
    // followed by fixed 12-byte postings.
    let record_offset = u32::from_be_bytes(bytes[14..18].try_into().expect("offset word")) as usize;
    let prefix_len = u32::from_be_bytes(
        bytes[record_offset..record_offset + 4]
            .try_into()
            .expect("prefix len"),
    ) as usize;
    assert_eq!(prefix_len, 0, "the global prefix is empty");
    let postings = record_offset + 8;
    const POSTING: usize = 12;
    assert!(bytes.len() >= postings + 2 * POSTING, "need two postings");
    for offset in 0..POSTING {
        bytes.swap(postings + offset, postings + POSTING + offset);
    }
    write_packed_bytes(&fixture, run_id, false, &bytes);

    let store = fixture.open();
    let error = assert_typed_refusal("query over an out-of-order posting list", || {
        store.query_newest(&Filter::new(), events.len())
    });
    assert!(
        error
            .message()
            .contains("posting list violates canonical order"),
        "the error must name the semantic violation: {error}"
    );
}

/// The run catalog and its by-min-key mirror are one relationship. A
/// wrong-id entry is not a run this scan may quietly skip.
#[test]
fn packed_scan_reports_a_disagreeing_run_range_index() {
    let fixture = Fixture::new();
    seeded_packed_store(&fixture);
    let run_id = *packed_run_ids(&fixture).first().expect("one packed run");

    {
        let db = fixture.raw();
        let write_txn = db.begin_write().expect("raw begin_write");
        {
            let mut open = write_txn
                .open_table(POSTINGS_RUN_BY_MIN)
                .expect("raw open run by min");
            let existing: Vec<u64> = open
                .iter()
                .expect("raw iter")
                .map(|entry| entry.expect("raw entry").0.value())
                .collect();
            for min_event_key in existing {
                open.insert(min_event_key, run_id + 9_999)
                    .expect("raw insert");
            }
        }
        write_txn.commit().expect("raw commit");
    }

    let store = fixture.open();
    assert_typed_refusal("query over a cross-wired run catalog", || {
        store.query(&Filter::new())
    });
}

fn set_next_run_id(fixture: &Fixture, value: Option<u64>) {
    let db = fixture.raw();
    let write_txn = db.begin_write().expect("raw begin_write");
    {
        let mut open = write_txn
            .open_table(STORE_META)
            .expect("raw open postings meta");
        match value {
            Some(value) => {
                open.insert(POSTINGS_NEXT_RUN_ID, value)
                    .expect("raw insert");
            }
            None => {
                open.remove(POSTINGS_NEXT_RUN_ID).expect("raw remove");
            }
        }
    }
    write_txn.commit().expect("raw commit");
}

/// How the packed run allocator can lie without any byte being malformed.
enum Rewind {
    /// The row is gone while a run is still live. Legal only against an
    /// empty catalog, where the canonical initial next id is `1`.
    Missing,
    /// Zero is never a valid run id.
    Zero,
    /// Exactly the id a live run already owns: the next publication would
    /// overwrite that run's dictionary, segments, and catalog row.
    OntoLiveRun,
}

/// Every byte of the allocator is a well-typed `u64`, so no decoder can see
/// this class. A rewound allocator hands back an id a live run already owns
/// and the publication that follows overwrites it inside an otherwise valid
/// transaction. Proven for each way the allocator can lie.
#[test]
fn packed_publication_rejects_an_allocator_that_would_reuse_a_live_run() {
    for rewind in [Rewind::Missing, Rewind::Zero, Rewind::OntoLiveRun] {
        let fixture = Fixture::new();
        let seeded = seeded_packed_store(&fixture);
        let live = *packed_run_ids(&fixture).first().expect("one packed run");
        set_next_run_id(
            &fixture,
            match rewind {
                Rewind::Missing => None,
                Rewind::Zero => Some(0),
                Rewind::OntoLiveRun => Some(live),
            },
        );

        let healthy_before = {
            let store = fixture.open();
            store.query(&Filter::new()).expect("healthy query").len()
        };
        assert_eq!(healthy_before, seeded.len());

        let keys = keys();
        let fresh = note(&keys, "after-rewind", 5_000);
        {
            let mut store = fixture.open();
            assert_typed_refusal("insert onto a rewound packed allocator", || {
                store.insert(fresh, observed())
            });
        }

        // The refused publication left the packed artifacts alone, and the
        // prior healthy query still answers identically after a reopen.
        assert_eq!(packed_run_ids(&fixture), vec![live]);
        let store = fixture.open();
        assert_eq!(
            store.query(&Filter::new()).expect("healthy query").len(),
            healthy_before,
            "a refused publication leaves the prior packed state intact"
        );
    }
}

/// `u64::MAX` stays typed exhaustion rather than wrapping back onto a live
/// run id.
#[test]
fn packed_publication_reports_run_id_exhaustion() {
    let fixture = Fixture::new();
    seeded_packed_store(&fixture);
    set_next_run_id(&fixture, Some(u64::MAX));

    let keys = keys();
    let fresh = note(&keys, "exhausted", 6_000);
    let mut store = fixture.open();
    let error = assert_typed_refusal("insert at run id exhaustion", || {
        store.insert(fresh, observed())
    });
    assert!(
        error.message().contains("run id space exhausted"),
        "exhaustion must be named, never wrapped: {error}"
    );
}

/// Sanity: the corruption harness itself is not what makes these doors
/// fail. An untouched store answers every one of them.
#[test]
fn a_healthy_store_answers_every_hardened_door() {
    let fixture = Fixture::new();
    let events = seeded_packed_store(&fixture);
    let store = fixture.open();
    assert_eq!(
        store.query(&Filter::new()).expect("query").len(),
        events.len()
    );
    assert_eq!(
        store
            .query_newest(&Filter::new(), events.len())
            .expect("query_newest")
            .len(),
        events.len()
    );
    assert!(store
        .recover_publish_queue()
        .expect("recover_publish_queue")
        .is_empty());
    assert!(
        store
            .get_coverage(
                crate::coverage::coverage_key(&healthy_atom(&keys())),
                &relay()
            )
            .is_none(),
        "an unrecorded coverage row stays a healthy None"
    );
}

fn healthy_atom(keys: &Keys) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
            ids: None,
            tags: BTreeMap::new(),
            since: None,
            until: None,
            limit: None,
        },
        source: nmp_grammar::SourceAuthority::AuthorOutboxes,
        access: nmp_grammar::AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}
