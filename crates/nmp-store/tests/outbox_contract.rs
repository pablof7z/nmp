//! The durable write-outbox door contract (issues #2/#3, Unit U1 —
//! `docs/design/crashsafe-accepted-2-3-plan.md` + its Fable checkpoint
//! verdict R1-R8, plus post-build architecture-review corrections:
//! `IntentId`/receipt-id store allocation, `Ephemeral` receipt-only
//! persistence, intent-KEYED `promote_signed`/`compensate_write` (an
//! intent's own `OUTBOX_INTENTS` row is the source of truth for its frozen
//! body, independent of whether a live `EVENTS` row currently exists for
//! it — covers `Duplicate`/`Stale` intents, chained local supersession,
//! relay supersession, kind:5 deletion, and NIP-40 expiry uniformly),
//! kind:5 immediate local delete on `accept_write`, and fallible
//! persistence doors. Mirrors `store_contract.rs`'s convention of running
//! shared-contract tests against BOTH `MemoryStore` and a fresh
//! `RedbStore`; recovery/atomicity tests that specifically need a durable
//! reopen are `RedbStore`-only.

use std::collections::BTreeSet;

use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, AttemptOutcome, ClaimSet, CompensateOutcome,
    EventStore, FinishAttemptOutcome, InsertOutcome, IntentSigState, LocalOrigin, MemoryStore,
    PromoteOutcome, ReceiptState, RedbStore, RefuseReason, RelayObserved, RetractReason, SigState,
    WriteDurability,
};
use nostr::nips::nip01::Coordinate;
use nostr::{Event, EventBuilder, Filter, JsonUtil, Keys, Kind, RelayUrl, Tag, Timestamp};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

fn keys() -> Keys {
    Keys::generate()
}

/// Build BOTH the `frozen` (sentinel-sig) event `accept_write` takes and the
/// REAL signed event (same id — NIP-01's id never depends on `sig`) a
/// signer would later hand back to `promote_signed`.
fn compose_with_tags(
    keys: &Keys,
    kind: Kind,
    content: &str,
    created_at: u64,
    tags: Vec<Tag>,
) -> (Event, Event) {
    let mut builder =
        EventBuilder::new(kind, content).custom_created_at(Timestamp::from(created_at));
    for tag in tags {
        builder = builder.tag(tag);
    }
    let signed = builder.sign_with_keys(keys).expect("sign event");
    let frozen = Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    );
    (frozen, signed)
}

fn compose(keys: &Keys, kind: Kind, content: &str, created_at: u64) -> (Event, Event) {
    compose_with_tags(keys, kind, content, created_at, Vec::new())
}

fn deletion_event(keys: &Keys, targets: Vec<Tag>, created_at: u64) -> Event {
    EventBuilder::new(Kind::EventDeletion, "")
        .tags(targets)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign deletion event")
}

fn frozen_from_signed(signed: &Event) -> Event {
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

/// An `AcceptWrite` for `frozen`. Neither `IntentId` nor a receipt id is a
/// parameter here — the store allocates BOTH (architecture review
/// correction; see `nmp_store::IntentId`'s doc) and hands them back on
/// every journaled `AcceptOutcome` variant via `.journaled_intent_id()`/
/// `.journaled_receipt_id()`.
fn accept(frozen: Event, expected_pubkey: nostr::PublicKey, accepted_at: u64) -> AcceptWrite {
    AcceptWrite {
        frozen,
        expected_pubkey,
        signing_identity_ref: "local".to_string(),
        durability: WriteDurability::Durable,
        routing: "author-outbox".to_string(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(accepted_at),
    }
}

/// `accept_write`, unwrapping the persistence `Result` — every test here
/// exercises a healthy in-process store, so a persistence failure would
/// itself be the bug under test.
fn do_accept(store: &mut dyn EventStore, accept: AcceptWrite) -> AcceptOutcome {
    store
        .accept_write(accept)
        .expect("accept_write persistence")
}

/// Run `body` against both backends, exactly like `store_contract.rs`'s
/// helper of the same name — every shared door-contract test goes through
/// this so the two backends can never silently diverge.
fn for_each_backend(mut body: impl FnMut(&mut dyn EventStore)) {
    let mut mem = MemoryStore::new();
    body(&mut mem);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let mut redb = RedbStore::open(&path).expect("open redb store");
    body(&mut redb);
}

// ---------------------------------------------------------------------

#[test]
fn accept_write_inserts_pending_row_and_journal_in_one_txn() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, _signed) = compose(&k, Kind::TextNote, "hello", 100);
        let frozen_id = frozen.id;

        let outcome = do_accept(store, accept(frozen, k.public_key(), 100));
        match outcome {
            AcceptOutcome::Inserted { intent_id, row, .. } => {
                assert_eq!(row.event.id, frozen_id);
                assert_eq!(row.event.sig, sentinel_signature());
                let local = row
                    .provenance
                    .local
                    .expect("locally-accepted row carries local provenance");
                assert!(local.owners.contains(&intent_id));
                assert_eq!(local.sig_state, SigState::Pending);
            }
            other => panic!("expected Inserted, got {other:?}"),
        }

        // Same call: the row is already queryable, no separate visibility
        // mechanism (issue #2's "store mutation and the normal resolver/
        // invalidation path are the only visibility mechanism").
        let rows = store.query(&Filter::new().id(frozen_id));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.id, frozen_id);
    });
}

#[test]
fn pending_row_projects_sig_state_and_is_queryable_like_any_row() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, _signed) = compose(&k, Kind::TextNote, "hi", 200);
        let frozen_id = frozen.id;
        let outcome = do_accept(store, accept(frozen, k.public_key(), 200));
        let intent_id = outcome.journaled_intent_id().expect("journaled");

        // Ordinary kind/author filtering, not just an id lookup — proves
        // participation in the SAME query path every other row uses.
        let rows = store.query(&Filter::new().kind(Kind::TextNote).author(k.public_key()));
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.event.id, frozen_id);
        let local = row
            .provenance
            .local
            .as_ref()
            .expect("app surface must be able to tell this row is pending");
        assert_eq!(local.sig_state, SigState::Pending);
        assert!(local.owners.contains(&intent_id));
    });
}

#[test]
fn promote_signed_swaps_sig_in_place_zero_id_churn_and_clears_displaced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");

    let k = keys();
    let (frozen_a, _signed_a) = compose(&k, Kind::ContactList, "v1", 100);
    let frozen_a_id = frozen_a.id;
    do_accept(&mut store, accept(frozen_a, k.public_key(), 100));

    let (frozen_b, signed_b) = compose(&k, Kind::ContactList, "v2", 200);
    let frozen_b_id = frozen_b.id;
    let outcome = do_accept(&mut store, accept(frozen_b, k.public_key(), 200));
    let intent_b = outcome.journaled_intent_id().expect("journaled");
    match outcome {
        AcceptOutcome::Superseded { row, replaced, .. } => {
            assert_eq!(replaced.event.id, frozen_a_id);
            assert_eq!(row.event.id, frozen_b_id);
        }
        other => panic!("expected Superseded, got {other:?}"),
    }

    // Before promotion, the intent's displaced stash is still open.
    let before = store.recover_outbox();
    let intent_before = before
        .iter()
        .find(|r| r.intent_id == intent_b)
        .expect("intent still open");
    assert!(intent_before.displaced.is_some());

    let real_sig = signed_b.sig;
    let promoted = store
        .promote_signed(intent_b, real_sig)
        .expect("promote_signed persistence");
    match promoted {
        PromoteOutcome::Promoted { row, .. } => {
            assert_eq!(
                row.event.id, frozen_b_id,
                "zero id churn: same id before/after promotion"
            );
            assert_eq!(row.event.sig, real_sig);
            let local = row
                .provenance
                .local
                .expect("promoted row keeps local provenance");
            assert_eq!(local.sig_state, SigState::Signed);
        }
        other => panic!("expected Promoted, got {other:?}"),
    }

    // Same id, still the one live row at it — no remove/re-add churn.
    let rows = store.query(&Filter::new().id(frozen_b_id));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.sig, real_sig);

    // R6: the displaced stash is durably cleared in the SAME promote
    // transaction — a boot after this point must never see it.
    let after = store.recover_outbox();
    let intent_after = after.iter().find(|r| r.intent_id == intent_b).expect(
        "intent still open (not yet delivered — only compensate_write/full-delivery closes it)",
    );
    assert!(intent_after.displaced.is_none());
    assert_eq!(intent_after.sig_state, IntentSigState::Signed);
}

#[test]
fn compensate_removes_pending_and_restores_displaced() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::ContactList, "v1", 100);
        let frozen_a_id = frozen_a.id;
        do_accept(store, accept(frozen_a, k.public_key(), 100));

        let (frozen_b, _signed_b) = compose(&k, Kind::ContactList, "v2", 200);
        let frozen_b_id = frozen_b.id;
        let outcome = do_accept(store, accept(frozen_b.clone(), k.public_key(), 200));
        let intent_b = outcome.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome, AcceptOutcome::Superseded { .. }));

        let compensated = store
            .compensate_write(intent_b)
            .expect("compensate_write persistence");
        match compensated {
            CompensateOutcome::Compensated { restored, .. } => {
                let restored = restored.expect("the displaced predecessor is restored");
                assert_eq!(restored.event.id, frozen_a_id);
            }
            CompensateOutcome::NotFound => panic!("expected Compensated"),
        }

        // The rejected pending row is gone; the predecessor is back.
        assert!(store.query(&Filter::new().id(frozen_b_id)).is_empty());
        assert_eq!(store.query(&Filter::new().id(frozen_a_id)).len(), 1);

        // NO TOMBSTONE (retraction doc §4.2: the row was never validly
        // signed, so `remove` writes none — this is the falsifier, not
        // just an assumption): re-observing `frozen_b`'s exact id/address
        // from a relay must NOT be refused as tombstoned. It legitimately
        // re-wins the address (its `created_at` is still the newest), which
        // simultaneously proves ordinary ADDR_INDEX bookkeeping was left
        // consistent by the compensation.
        let _ = RetractReason::Rejected; // documents which reason `compensate_write` used
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        let reinsert = store.insert(frozen_b, RelayObserved::new(relay, Timestamp::from(300)));
        match reinsert {
            InsertOutcome::Superseded { replaced } => {
                assert_eq!(replaced.event.id, frozen_a_id);
            }
            other => panic!("expected the compensated id to be freely re-insertable (no tombstone), got {other:?}"),
        }
    });
}

#[test]
fn refused_accept_leaves_no_journal_residue() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, _signed) = compose_with_tags(
            &k,
            Kind::TextNote,
            "already expired",
            50,
            vec![Tag::expiration(Timestamp::from(10u64))],
        );

        let outcome = do_accept(store, accept(frozen, k.public_key(), 50));
        assert!(matches!(
            outcome,
            AcceptOutcome::Refused(RefuseReason::AlreadyExpired)
        ));
        assert!(
            outcome.journaled_intent_id().is_none(),
            "a refused call must never allocate an IntentId either"
        );
        assert!(outcome.journaled_receipt_id().is_none());
    });

    // RedbStore only: the durable journal itself (not just the row) is
    // empty — `recover_outbox` finds nothing for this intent.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let k = keys();
    let (frozen, _signed) = compose_with_tags(
        &k,
        Kind::TextNote,
        "already expired",
        50,
        vec![Tag::expiration(Timestamp::from(10u64))],
    );
    do_accept(&mut store, accept(frozen, k.public_key(), 50));
    assert!(store.recover_outbox().is_empty());
}

#[test]
fn pending_row_is_not_gc_evicted_while_intent_open() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, signed) = compose(&k, Kind::TextNote, "unsigned draft", 100);
        let frozen_id = frozen.id;
        let outcome = do_accept(store, accept(frozen, k.public_key(), 100));
        let intent_id = outcome.journaled_intent_id().expect("journaled");

        // An EMPTY claim set: nothing claims this row by demand at all —
        // and yet it must survive GC while still `Pending` (Fable
        // checkpoint R5). A regular (non-addressable) kind, so it would be
        // an ordinary GC candidate the moment it stops being an open
        // intent.
        let claims = ClaimSet::new(Vec::new());
        let report = store.gc(&claims);
        assert_eq!(
            report.events_evicted, 0,
            "an open, unsigned intent must never be GC-evicted"
        );
        assert_eq!(store.query(&Filter::new().id(frozen_id)).len(), 1);

        // Once promoted, it is an ordinary event again — GC-able under the
        // SAME empty claim set.
        store
            .promote_signed(intent_id, signed.sig)
            .expect("promote_signed persistence");
        let report2 = store.gc(&claims);
        assert_eq!(
            report2.events_evicted, 1,
            "a signed row is GC-able like any other event"
        );
        assert!(store.query(&Filter::new().id(frozen_id)).is_empty());
    });
}

/// The single-transaction atomicity boundary (Fable checkpoint R7): a
/// successful `accept_write` and a `Refused` one are the only two durable
/// outcomes, and each is atomic in itself — the row and its journal entry
/// always travel together (never one without the other) across a reopen, a
/// `Refused` call leaves neither. A literal kill-mid-transaction fault
/// injection (interrupting `redb`'s own `Database` between the event-table
/// write and the outbox-table write) is U5's dedicated crash-injection
/// suite's job (crashsafe-accepted-2-3-plan.md §6 U5); this test proves the
/// structural property — everything inside ONE `write_txn`/`commit()` call
/// — that is what makes such a kill safe by construction.
#[test]
fn accept_crash_is_all_or_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");

    let k = keys();
    let (frozen_ok, _signed_ok) = compose(&k, Kind::TextNote, "ok", 100);
    let frozen_ok_id = frozen_ok.id;
    let (frozen_exp, _signed_exp) = compose_with_tags(
        &k,
        Kind::TextNote,
        "already expired",
        50,
        vec![Tag::expiration(Timestamp::from(10u64))],
    );
    let frozen_exp_id = frozen_exp.id;

    let ok_intent_id = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let ok = do_accept(&mut store, accept(frozen_ok, k.public_key(), 100));
        let ok_intent_id = ok.journaled_intent_id().expect("journaled");
        assert!(matches!(ok, AcceptOutcome::Inserted { .. }));

        let refused = do_accept(&mut store, accept(frozen_exp, k.public_key(), 50));
        assert!(matches!(
            refused,
            AcceptOutcome::Refused(RefuseReason::AlreadyExpired)
        ));
        assert!(refused.journaled_intent_id().is_none());
        // Dropped here — reopening below is the only way to tell what
        // actually landed durably.
        ok_intent_id
    };

    let store = RedbStore::open(&path).expect("reopen redb store");

    let ok_rows = store.query(&Filter::new().id(frozen_ok_id));
    assert_eq!(ok_rows.len(), 1, "the accepted row must survive reopen");
    let recovered = store.recover_outbox();
    assert!(
        recovered.iter().any(|r| r.intent_id == ok_intent_id),
        "its journal entry must survive TOGETHER with the row (same transaction)"
    );
    assert_eq!(
        recovered.len(),
        1,
        "a refused intent must leave no journal residue, even across a reopen"
    );

    let exp_rows = store.query(&Filter::new().id(frozen_exp_id));
    assert!(
        exp_rows.is_empty(),
        "a refused intent must leave no row either, even across a reopen"
    );
}

#[test]
fn recover_outbox_reconstructs_inflight_after_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");

    let k = keys();
    let (frozen, _signed) = compose(&k, Kind::TextNote, "offline draft", 100);
    let frozen_id = frozen.id;

    let (accepted_intent_id, accepted_receipt_id) = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let outcome = do_accept(&mut store, accept(frozen, k.public_key(), 100));
        let intent_id = outcome.journaled_intent_id().expect("journaled");
        let receipt_id = outcome.journaled_receipt_id().expect("journaled");
        assert!(matches!(outcome, AcceptOutcome::Inserted { .. }));
        // Dropped here WITHOUT ever calling `promote_signed` — simulates a
        // crash between acceptance and the signer's response.
        (intent_id, receipt_id)
    };

    let store = RedbStore::open(&path).expect("reopen redb store");
    let recovered = store.recover_outbox();
    assert_eq!(recovered.len(), 1);
    let intent = &recovered[0];
    assert_eq!(intent.intent_id, accepted_intent_id);
    assert_eq!(intent.receipt_id, accepted_receipt_id);
    assert_eq!(intent.frozen.id, frozen_id);
    assert_eq!(intent.sig_state, IntentSigState::Pending);
    assert!(intent.displaced.is_none());

    // The pending row itself is ALREADY live in the store post-reopen —
    // recovery does not re-insert it (plan §2.3: query-visible from the
    // first post-boot subscription, no separate replay-into-store step).
    let rows = store.query(&Filter::new().id(frozen_id));
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].provenance.local,
        Some(LocalOrigin {
            sig_state: SigState::Pending,
            ..
        })
    ));
}

#[test]
fn attempt_started_bytes_and_ordinals_survive_real_reopen_append_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();
    let (frozen, signed) = compose(&k, Kind::TextNote, "attempt bytes", 101);
    let relay = RelayUrl::parse("wss://attempt.example").unwrap();

    let intent_id = {
        let mut store = RedbStore::open(&path).expect("open");
        let outcome = do_accept(&mut store, accept(frozen, k.public_key(), 101));
        let intent_id = outcome.journaled_intent_id().unwrap();
        store.promote_signed(intent_id, signed.sig).unwrap();
        // Drop a database produced by the pre-attempt acceptance shape.
        // Reopening it and writing the first v1 attempt is the migration
        // behavior: existing OUTBOX_* rows remain readable and no rewrite
        // of the intent/receipt is required.
        intent_id
    };

    {
        let mut store = RedbStore::open(&path).expect("reopen before first attempt");
        let first = store
            .start_attempt(intent_id, relay.clone(), signed.clone())
            .unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(first.ordinal, 1);
        assert_eq!(first.event.as_json(), signed.as_json());
    }

    {
        let mut store = RedbStore::open(&path).expect("reopen");
        let recovered = store.recover_attempts(intent_id).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].outcome, AttemptOutcome::Started);
        assert_eq!(recovered[0].event.as_json(), signed.as_json());
        store
            .finish_attempt(
                intent_id,
                &relay,
                1,
                AttemptOutcome::Rejected("nope".into()),
            )
            .unwrap();
        // A late contradictory terminal is typed non-success and cannot
        // rewrite append-only truth.
        assert!(store
            .finish_attempt(intent_id, &relay, 1, AttemptOutcome::Acked)
            .is_err());
        let second = store
            .start_attempt(intent_id, relay.clone(), signed.clone())
            .unwrap();
        assert_eq!(second.ordinal, 2);
    }

    let store = RedbStore::open(&path).expect("second reopen");
    let recovered = store.recover_attempts(intent_id).unwrap();
    assert_eq!(recovered.len(), 2);
    assert_eq!(
        recovered[0].outcome,
        AttemptOutcome::Rejected("nope".into())
    );
    assert_eq!(recovered[1].outcome, AttemptOutcome::Started);
    assert!(recovered
        .iter()
        .all(|a| a.event.as_json() == signed.as_json()));
}

#[test]
fn resolved_route_revisions_are_append_only_canonical_and_backend_identical() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, _) = compose(&k, Kind::TextNote, "route revisions", 102);
        let outcome = do_accept(store, accept(frozen, k.public_key(), 102));
        let intent = outcome.journaled_intent_id().unwrap();
        let a = RelayUrl::parse("wss://a-route.example").unwrap();
        let z = RelayUrl::parse("wss://z-route.example").unwrap();
        let first = store
            .record_route_revision(intent, BTreeSet::from([z.clone(), a.clone()]))
            .unwrap();
        let second = store
            .record_route_revision(intent, BTreeSet::from([z.clone()]))
            .unwrap();
        assert_eq!((first.ordinal, second.ordinal), (1, 2));
        assert_eq!(first.relays, BTreeSet::from([a, z.clone()]));
        assert_eq!(second.relays, BTreeSet::from([z]));
        assert_eq!(
            store
                .recover_route_revisions(intent)
                .unwrap()
                .into_iter()
                .map(|revision| revision.ordinal)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    });
}

#[test]
fn resolved_route_revision_survives_real_redb_reopen_without_an_attempt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("route-revision.redb");
    let k = keys();
    let relay = RelayUrl::parse("wss://durable-route.example").unwrap();
    let intent = {
        let mut store = RedbStore::open(&path).unwrap();
        let (frozen, _) = compose(&k, Kind::TextNote, "durable route", 103);
        let outcome = do_accept(&mut store, accept(frozen, k.public_key(), 103));
        let intent = outcome.journaled_intent_id().unwrap();
        store
            .record_route_revision(intent, BTreeSet::from([relay.clone()]))
            .unwrap();
        intent
    };
    let store = RedbStore::open(&path).unwrap();
    assert!(store.recover_attempts(intent).unwrap().is_empty());
    assert_eq!(
        store.recover_route_revisions(intent).unwrap()[0].relays,
        BTreeSet::from([relay])
    );
}

#[test]
fn finish_attempt_missing_same_and_conflict_are_not_false_success() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, signed) = compose(&k, Kind::TextNote, "finish truth", 102);
        let relay = RelayUrl::parse("wss://finish.example").unwrap();
        let outcome = do_accept(store, accept(frozen, k.public_key(), 102));
        let intent_id = outcome.journaled_intent_id().unwrap();
        store.promote_signed(intent_id, signed.sig).unwrap();
        assert!(store
            .finish_attempt(intent_id, &relay, 1, AttemptOutcome::Acked)
            .is_err());
        store
            .start_attempt(intent_id, relay.clone(), signed)
            .unwrap();
        assert_eq!(
            store
                .finish_attempt(intent_id, &relay, 1, AttemptOutcome::Acked)
                .unwrap(),
            FinishAttemptOutcome::Committed
        );
        assert_eq!(
            store
                .finish_attempt(intent_id, &relay, 1, AttemptOutcome::Acked)
                .unwrap(),
            FinishAttemptOutcome::AlreadySame
        );
        assert!(store
            .finish_attempt(
                intent_id,
                &relay,
                1,
                AttemptOutcome::Rejected("conflict".into())
            )
            .is_err());
    });
}

#[test]
fn relay_prefixes_have_disjoint_attempt_ordinals() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, signed) = compose(&k, Kind::TextNote, "prefix", 103);
        let short = RelayUrl::parse("wss://prefix.example/x").unwrap();
        let extended = RelayUrl::parse("wss://prefix.example/x:443").unwrap();
        let outcome = do_accept(store, accept(frozen, k.public_key(), 103));
        let intent_id = outcome.journaled_intent_id().unwrap();
        store.promote_signed(intent_id, signed.sig).unwrap();
        assert_eq!(
            store
                .start_attempt(intent_id, short.clone(), signed.clone())
                .unwrap()
                .ordinal,
            1
        );
        assert_eq!(
            store
                .start_attempt(intent_id, extended.clone(), signed)
                .unwrap()
                .ordinal,
            1
        );
        let attempts = store.recover_attempts(intent_id).unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(attempts.iter().any(|a| a.relay == short));
        assert!(attempts.iter().any(|a| a.relay == extended));
    });
}

#[test]
fn recover_attempt_order_is_canonical_and_identical_across_backends() {
    // Lexically `aa` sorts before `z`, while the length-prefixed Redb key
    // sorts the shorter `z` URL first. Recovery must ignore that storage
    // order and return `(relay, ordinal)` canonically on both backends.
    let mut backend_orders = Vec::new();
    for_each_backend(|store| {
        let k = keys();
        let (frozen, signed) = compose(&k, Kind::TextNote, "ordering", 109);
        let z_short = RelayUrl::parse("wss://z.example/x").unwrap();
        let aa_long = RelayUrl::parse("wss://aa.example/x").unwrap();
        let outcome = do_accept(store, accept(frozen, k.public_key(), 109));
        let intent_id = outcome.journaled_intent_id().unwrap();
        store.promote_signed(intent_id, signed.sig).unwrap();

        store
            .start_attempt(intent_id, z_short.clone(), signed.clone())
            .unwrap();
        store
            .finish_attempt(intent_id, &z_short, 1, AttemptOutcome::GaveUp)
            .unwrap();
        store
            .start_attempt(intent_id, z_short.clone(), signed.clone())
            .unwrap();
        store
            .start_attempt(intent_id, aa_long.clone(), signed)
            .unwrap();

        let order: Vec<_> = store
            .recover_attempts(intent_id)
            .unwrap()
            .into_iter()
            .map(|attempt| (attempt.relay, attempt.ordinal))
            .collect();
        assert_eq!(
            order,
            vec![(aa_long, 1), (z_short.clone(), 1), (z_short, 2)]
        );
        backend_orders.push(order);
    });
    assert_eq!(backend_orders.len(), 2);
    assert_eq!(backend_orders[0], backend_orders[1]);
}

fn raw_attempt_key(intent_id: nmp_store::IntentId, relay: &RelayUrl, ordinal: &str) -> String {
    format!(
        "{:020}:{:020}:{}:{}",
        intent_id.0,
        relay.as_str().len(),
        relay.as_str(),
        ordinal
    )
}

fn raw_route_revision_key(intent_id: nmp_store::IntentId, ordinal: u64) -> String {
    format!("{:020}:{:020}", intent_id.0, ordinal)
}

#[test]
fn route_revision_range_excludes_prefix_intents_but_rejects_target_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("route-range-corruption.redb");
    let k = keys();
    let short = RelayUrl::parse("wss://prefix.example/x").unwrap();
    let extended = RelayUrl::parse("wss://prefix.example/x:443").unwrap();
    let (target, prefix_adversary) = {
        let mut store = RedbStore::open(&path).unwrap();
        let mut ids = Vec::new();
        for index in 0..10u64 {
            let (frozen, _) = compose(&k, Kind::TextNote, &format!("intent-{index}"), 500 + index);
            let outcome = do_accept(&mut store, accept(frozen, k.public_key(), 500 + index));
            ids.push(outcome.journaled_intent_id().unwrap());
        }
        assert_eq!(ids[0], nmp_store::IntentId(1));
        assert_eq!(ids[9], nmp_store::IntentId(10));
        store
            .record_route_revision(ids[0], BTreeSet::from([short.clone(), extended.clone()]))
            .unwrap();
        store
            .record_route_revision(ids[9], BTreeSet::from([short.clone()]))
            .unwrap();
        (ids[0], ids[9])
    };

    const ROUTES: TableDefinition<&str, &str> = TableDefinition::new("outbox_route_revisions");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(ROUTES).unwrap();
        table
            .insert(raw_route_revision_key(prefix_adversary, 1).as_str(), "{}")
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);

    let store = RedbStore::open(&path).unwrap();
    let target_rows = store.recover_route_revisions(target).unwrap();
    assert_eq!(target_rows.len(), 1);
    assert_eq!(target_rows[0].relays, BTreeSet::from([short, extended]));
    drop(store);

    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(ROUTES).unwrap();
        let key = raw_route_revision_key(target, 1);
        let json = table
            .get(key.as_str())
            .unwrap()
            .unwrap()
            .value()
            .to_string();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["ordinal"] = serde_json::json!(2);
        let encoded = serde_json::to_string(&value).unwrap();
        table.insert(key.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_route_revisions(target)
        .unwrap_err()
        .to_string()
        .contains("key does not match"));
}

#[test]
fn corrupt_or_unknown_attempt_rows_are_fallible_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-attempt.redb");
    let k = keys();
    let (frozen, signed) = compose(&k, Kind::TextNote, "corrupt", 104);
    let relay = RelayUrl::parse("wss://corrupt.example").unwrap();
    let intent_id = {
        let mut store = RedbStore::open(&path).unwrap();
        let outcome = do_accept(&mut store, accept(frozen, k.public_key(), 104));
        let intent_id = outcome.journaled_intent_id().unwrap();
        store.promote_signed(intent_id, signed.sig).unwrap();
        store
            .start_attempt(intent_id, relay.clone(), signed.clone())
            .unwrap();
        intent_id
    };

    const ATTEMPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempts");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(ATTEMPTS).unwrap();
        let key = raw_attempt_key(intent_id, &relay, &format!("{:020}", 1));
        let json = table
            .get(key.as_str())
            .unwrap()
            .unwrap()
            .value()
            .to_string();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["version"] = serde_json::json!(99);
        let encoded = serde_json::to_string(&value).unwrap();
        table.insert(key.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);

    let store = RedbStore::open(&path).unwrap();
    assert!(store.recover_attempts(intent_id).is_err());
}

#[test]
fn attempt_key_tuple_mismatch_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tuple-mismatch.redb");
    let k = keys();
    let (frozen, signed) = compose(&k, Kind::TextNote, "tuple", 107);
    let relay = RelayUrl::parse("wss://tuple.example").unwrap();
    let intent_id = {
        let mut store = RedbStore::open(&path).unwrap();
        let outcome = do_accept(&mut store, accept(frozen, k.public_key(), 107));
        let intent_id = outcome.journaled_intent_id().unwrap();
        store.promote_signed(intent_id, signed.sig).unwrap();
        store
            .start_attempt(intent_id, relay.clone(), signed)
            .unwrap();
        intent_id
    };
    const ATTEMPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempts");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(ATTEMPTS).unwrap();
        let key = raw_attempt_key(intent_id, &relay, &format!("{:020}", 1));
        let json = table
            .get(key.as_str())
            .unwrap()
            .unwrap()
            .value()
            .to_string();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["ordinal"] = serde_json::json!(2);
        let encoded = serde_json::to_string(&value).unwrap();
        table.insert(key.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_attempts(intent_id)
        .unwrap_err()
        .to_string()
        .contains("key does not match"));
}

#[test]
fn malformed_matching_attempt_key_and_ordinal_exhaustion_are_typed_errors() {
    for (suffix, expected) in [
        ("not-an-ordinal".to_string(), "parse"),
        (u64::MAX.to_string(), "exhausted"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-key.redb");
        let k = keys();
        let (frozen, signed) = compose(&k, Kind::TextNote, "bad key", 105);
        let relay = RelayUrl::parse("wss://bad-key.example").unwrap();
        let intent_id = {
            let mut store = RedbStore::open(&path).unwrap();
            let outcome = do_accept(&mut store, accept(frozen, k.public_key(), 105));
            let intent_id = outcome.journaled_intent_id().unwrap();
            store.promote_signed(intent_id, signed.sig).unwrap();
            intent_id
        };
        const ATTEMPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempts");
        let db = Database::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(ATTEMPTS).unwrap();
            let key = raw_attempt_key(intent_id, &relay, &suffix);
            table.insert(key.as_str(), "{}").unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let mut store = RedbStore::open(&path).unwrap();
        let err = store
            .start_attempt(intent_id, relay, signed)
            .expect_err("corrupt key cannot allocate an ordinal");
        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[test]
fn durable_id_counter_overflow_and_receipt_namespace_boundary_are_errors() {
    for (meta_key, value, expected) in [
        ("next_intent_id", u64::MAX.to_string(), "exhausted"),
        ("next_receipt_id", (1u64 << 63).to_string(), "namespace"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counter.redb");
        drop(RedbStore::open(&path).unwrap());
        const META: TableDefinition<&str, &str> = TableDefinition::new("outbox_meta");
        let db = Database::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(META).unwrap();
            table.insert(meta_key, value.as_str()).unwrap();
        }
        tx.commit().unwrap();
        drop(db);
        let k = keys();
        let (frozen, _) = compose(&k, Kind::TextNote, "counter", 106);
        let mut store = RedbStore::open(&path).unwrap();
        let err = store
            .accept_write(accept(frozen, k.public_key(), 106))
            .expect_err("counter boundary must reject acceptance atomically");
        assert!(err.to_string().contains(expected), "{err}");
        assert!(store.recover_outbox().is_empty());
    }
}

/// Architecture-review correction #1 falsifier: `IntentId` must be
/// allocated from a durable, store-owned high-water mark, NEVER inferred
/// from the currently-open recovered set. This test constructs the exact
/// trap a naive "seed past max open id" allocator falls into: terminate
/// EVERY intent via `compensate_write` (the one open-work-row deletion path
/// this unit actually implements — promotion deliberately does NOT delete
/// `OUTBOX_INTENTS`, since a promoted-but-undelivered intent is still
/// legitimately open work; full-delivery terminal cleanup is a later
/// unit's job), restart so `recover_outbox` sees nothing open at all, then
/// accept a fresh intent and assert its id was never used before — because
/// the store's allocator is a durable counter, not an inference over
/// "what's currently open".
#[test]
fn intent_id_never_reused_after_all_intents_terminate_and_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();

    let (id1, id2) = {
        let mut store = RedbStore::open(&path).expect("open redb store");

        // Both intents terminate via compensation — THIS is the exact
        // path that deletes their `OUTBOX_INTENTS` open-work row, the
        // reuse hazard the correction closes.
        let (frozen1, _signed1) = compose(&k, Kind::TextNote, "one", 100);
        let outcome1 = do_accept(&mut store, accept(frozen1, k.public_key(), 100));
        let id1 = outcome1.journaled_intent_id().expect("journaled");
        store.compensate_write(id1).expect("compensate persistence");

        let (frozen2, _signed2) = compose(&k, Kind::TextNote, "two", 200);
        let outcome2 = do_accept(&mut store, accept(frozen2, k.public_key(), 200));
        let id2 = outcome2.journaled_intent_id().expect("journaled");
        store.compensate_write(id2).expect("compensate persistence");

        // At this exact moment, `recover_outbox` sees NOTHING open — the
        // scenario a naive "seed past max open id" allocator would read as
        // "no id has ever been used".
        assert!(store.recover_outbox().is_empty());

        (id1, id2)
    };

    // Restart — the open set is (still) empty.
    let mut store = RedbStore::open(&path).expect("reopen redb store");
    assert!(store.recover_outbox().is_empty());

    let (frozen3, _signed3) = compose(&k, Kind::TextNote, "three", 300);
    let outcome3 = do_accept(&mut store, accept(frozen3, k.public_key(), 300));
    let id3 = outcome3.journaled_intent_id().expect("journaled");

    assert_ne!(
        id3, id1,
        "a post-restart id must never collide with a terminated (compensated) intent"
    );
    assert_ne!(
        id3, id2,
        "a post-restart id must never collide with a terminated (compensated) intent"
    );
    assert!(
        id3.0 > id1.0 && id3.0 > id2.0,
        "the durable high-water mark must strictly advance across restart: got {id3:?}, prior {id1:?}/{id2:?}"
    );
}

/// The identical reuse-hazard falsifier as `intent_id_never_reused_after_
/// all_intents_terminate_and_restart`, for `receipt_id` (team-lead
/// correction: once receipts are durably RETAINED across restart, a
/// caller-side receipt-id counter that resets on restart has the exact
/// same collision hazard `IntentId` had — `receipt_id` is therefore
/// ALSO store-allocated from `OUTBOX_META`'s durable high-water mark,
/// bumped in the same `accept_write`/`accept_ephemeral` transaction,
/// never inferred from "what's currently open" or "what's currently
/// retained"). Unlike an intent's `OUTBOX_INTENTS` row, a receipt's
/// `OUTBOX_RECEIPTS` row is NEVER deleted by this unit — so this test
/// terminates every intent (closing its open-work row) while leaving the
/// receipts themselves retained, then restarts and asserts the next
/// receipt id was never used before, even against the surviving retained
/// set, not just the open one.
#[test]
fn receipt_id_never_reused_after_terminal_and_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();

    let (receipt1, receipt2, receipt3_ephemeral) = {
        let mut store = RedbStore::open(&path).expect("open redb store");

        let (frozen1, _signed1) = compose(&k, Kind::TextNote, "one", 100);
        let outcome1 = do_accept(&mut store, accept(frozen1, k.public_key(), 100));
        let intent1 = outcome1.journaled_intent_id().expect("journaled");
        let receipt1 = outcome1.journaled_receipt_id().expect("journaled");
        store
            .compensate_write(intent1)
            .expect("compensate persistence");

        let (frozen2, _signed2) = compose(&k, Kind::TextNote, "two", 200);
        let outcome2 = do_accept(&mut store, accept(frozen2, k.public_key(), 200));
        let intent2 = outcome2.journaled_intent_id().expect("journaled");
        let receipt2 = outcome2.journaled_receipt_id().expect("journaled");
        store
            .compensate_write(intent2)
            .expect("compensate persistence");

        // An Ephemeral receipt — receipt-only, never backed by an intent
        // at all — draws from the SAME durable counter.
        let (frozen_eph, _signed_eph) = compose(&k, Kind::TextNote, "ephemeral", 250);
        let receipt3 = store
            .accept_ephemeral(frozen_eph.id, k.public_key())
            .expect("accept_ephemeral persistence");

        // Every intent's open-work row is gone, but all THREE receipts
        // remain durably RETAINED — the exact surviving-retained-set trap
        // a naive allocator could otherwise be seeded from.
        assert!(store.recover_outbox().is_empty());
        assert!(store.reattach_receipt(receipt1).unwrap().is_some());
        assert!(store.reattach_receipt(receipt2).unwrap().is_some());
        assert!(store.reattach_receipt(receipt3).unwrap().is_some());

        (receipt1, receipt2, receipt3)
    };

    // Restart — the retained receipts still answer, the open set is empty.
    let mut store = RedbStore::open(&path).expect("reopen redb store");
    assert!(store.recover_outbox().is_empty());
    assert!(store.reattach_receipt(receipt1).unwrap().is_some());
    assert!(store.reattach_receipt(receipt2).unwrap().is_some());
    assert!(store
        .reattach_receipt(receipt3_ephemeral)
        .unwrap()
        .is_some());

    let (frozen4, _signed4) = compose(&k, Kind::TextNote, "four", 300);
    let outcome4 = do_accept(&mut store, accept(frozen4, k.public_key(), 300));
    let receipt4 = outcome4.journaled_receipt_id().expect("journaled");

    assert_ne!(
        receipt4, receipt1,
        "a post-restart receipt id must never collide with a retained, terminated receipt"
    );
    assert_ne!(receipt4, receipt2);
    assert_ne!(receipt4, receipt3_ephemeral);
    assert!(
        receipt4 > receipt1 && receipt4 > receipt2 && receipt4 > receipt3_ephemeral,
        "the durable receipt high-water mark must strictly advance across restart: got {receipt4}, prior {receipt1}/{receipt2}/{receipt3_ephemeral}"
    );
}

/// Architecture-review correction #2 falsifier: a receipt must stay
/// reattachable via `reattach_receipt` after its intent's `OUTBOX_INTENTS`
/// open-work row is gone — the case this unit can actually produce that
/// for is `compensate_write` (promotion deliberately does NOT delete the
/// open-work row; a promoted-but-undelivered intent legitimately stays in
/// `recover_outbox` until a later unit's full-delivery tracking closes it
/// — see the sibling reuse-hazard test's doc). This test proves both
/// halves: (a) the compensated intent's receipt survives independently of
/// its now-gone open-work row, in the correct terminal `Compensated`
/// state; (b) a still-open (signed-but-undelivered) intent's receipt is
/// ALSO independently reattachable — the `OUTBOX_RECEIPTS` mechanism is
/// general, not conditional on the open-work row being gone.
#[test]
fn terminal_receipt_still_reattachable_after_recover() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();

    let (frozen_signed, signed) = compose(&k, Kind::TextNote, "goes terminal", 100);
    let frozen_signed_id = frozen_signed.id;
    let (frozen_comp, _signed_comp) = compose(&k, Kind::TextNote, "gets compensated", 200);
    let frozen_comp_id = frozen_comp.id;

    let (intent_signed, receipt_signed_id, intent_comp, receipt_comp_id) = {
        let mut store = RedbStore::open(&path).expect("open redb store");

        let outcome_a = do_accept(&mut store, accept(frozen_signed, k.public_key(), 100));
        let intent_signed = outcome_a.journaled_intent_id().expect("journaled");
        let receipt_signed_id = outcome_a.journaled_receipt_id().expect("journaled");
        store
            .promote_signed(intent_signed, signed.sig)
            .expect("promote persistence");

        let outcome_b = do_accept(&mut store, accept(frozen_comp, k.public_key(), 200));
        let intent_comp = outcome_b.journaled_intent_id().expect("journaled");
        let receipt_comp_id = outcome_b.journaled_receipt_id().expect("journaled");
        store
            .compensate_write(intent_comp)
            .expect("compensate persistence");

        (
            intent_signed,
            receipt_signed_id,
            intent_comp,
            receipt_comp_id,
        )
    };

    // Reopen. The COMPENSATED intent's open-work row is gone (the
    // falsifier this test is really for); the SIGNED-but-undelivered
    // intent's open-work row legitimately still exists (out of this
    // unit's scope to close) — both receipts must reattach regardless.
    let store = RedbStore::open(&path).expect("reopen redb store");
    let recovered = store.recover_outbox();
    assert!(
        !recovered.iter().any(|r| r.intent_id == intent_comp),
        "the compensated intent must not appear in open-work recovery"
    );
    assert!(
        recovered.iter().any(|r| r.intent_id == intent_signed),
        "a signed-but-undelivered intent legitimately remains open work in this unit's scope"
    );

    let receipt_signed = store
        .reattach_receipt(receipt_signed_id)
        .expect("receipt lookup must be readable")
        .expect("signed receipt must still be reattachable");
    assert_eq!(receipt_signed.intent_id, Some(intent_signed));
    assert_eq!(receipt_signed.frozen_id, frozen_signed_id);
    assert_eq!(receipt_signed.state, ReceiptState::Signed);

    let receipt_comp = store
        .reattach_receipt(receipt_comp_id)
        .expect("receipt lookup must be readable")
        .expect("compensated receipt must still be reattachable");
    assert_eq!(receipt_comp.intent_id, Some(intent_comp));
    assert_eq!(receipt_comp.frozen_id, frozen_comp_id);
    assert_eq!(receipt_comp.state, ReceiptState::Compensated);

    assert!(
        store.reattach_receipt(99_999).unwrap().is_none(),
        "an unknown receipt id must reattach to nothing"
    );

    // Retention (not crash-survival) is the contract — `MemoryStore`
    // answers a freshly-accepted, still-open receipt just as faithfully,
    // within the life of the process.
    let mut mem = MemoryStore::new();
    let (frozen_fresh, _signed_fresh) = compose(&k, Kind::TextNote, "still open", 300);
    let outcome_fresh = do_accept(&mut mem, accept(frozen_fresh, k.public_key(), 300));
    let intent_fresh = outcome_fresh.journaled_intent_id().expect("journaled");
    let receipt_fresh_id = outcome_fresh.journaled_receipt_id().expect("journaled");
    let receipt_fresh = mem
        .reattach_receipt(receipt_fresh_id)
        .expect("receipt lookup must be readable")
        .expect("fresh receipt reattachable on MemoryStore too");
    assert_eq!(receipt_fresh.intent_id, Some(intent_fresh));
    assert_eq!(receipt_fresh.state, ReceiptState::Accepted);
}

/// VISION-ratified receipt contract clarification (team-lead correction,
/// issue #3): `Ephemeral` must NOT mean "no receipt / no restart
/// reattachment" — a durable OR explicitly non-durable write is still
/// observed through a reattachable receipt. `accept_ephemeral` persists a
/// receipt-ONLY record (`intent_id: None` — no `OUTBOX_INTENTS`/journal row
/// backs it, and `accept_ephemeral` never touches `EVENTS` at all, so
/// there is no query-visible pending row either). After a reopen with no
/// further transition ever recorded, the receipt must report the
/// `Abandoned` terminal (R4 stays correct: `Ephemeral` is never retried
/// after process loss, so `Accepted`-at-reopen can only mean the process
/// died before any further transition).
#[test]
fn ephemeral_persists_receipt_only_no_journal_no_pending_row_and_reattaches_after_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();

    let (frozen, _signed) = compose(&k, Kind::TextNote, "fire and forget", 100);
    let frozen_id = frozen.id;

    let receipt_id = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let receipt_id = store
            .accept_ephemeral(frozen_id, k.public_key())
            .expect("accept_ephemeral persistence");

        // No pending row: `accept_ephemeral` never touches `EVENTS`.
        assert!(store.query(&Filter::new().id(frozen_id)).is_empty());
        // No open-work/journal row: `recover_outbox` (OUTBOX_INTENTS-only)
        // sees nothing.
        assert!(store.recover_outbox().is_empty());

        // The receipt itself IS there, `Accepted`, receipt-only.
        let receipt = store
            .reattach_receipt(receipt_id)
            .expect("receipt lookup must be readable")
            .expect("ephemeral receipt persists immediately");
        assert_eq!(receipt.intent_id, None, "receipt-only: nothing backs it");
        assert_eq!(receipt.frozen_id, frozen_id);
        assert_eq!(receipt.state, ReceiptState::Accepted);
        // Dropped here with no further transition — simulates the process
        // dying before any dispatch/ack tracking (out of this unit's
        // scope) ever advanced this receipt past `Accepted`.
        receipt_id
    };

    let store = RedbStore::open(&path).expect("reopen redb store");

    // Still no pending row, still no open-work row, after reopen.
    assert!(store.query(&Filter::new().id(frozen_id)).is_empty());
    assert!(store.recover_outbox().is_empty());

    // But the receipt is reattachable, now correctly `Abandoned` — the
    // boot-time reconciliation `RedbStore::open()` runs.
    let receipt = store
        .reattach_receipt(receipt_id)
        .expect("receipt lookup must be readable")
        .expect("ephemeral receipt still reattachable after reopen");
    assert_eq!(receipt.intent_id, None);
    assert_eq!(receipt.frozen_id, frozen_id);
    assert_eq!(receipt.state, ReceiptState::Abandoned);

    // MemoryStore: same receipt-only shape, but no crash concept — it
    // stays `Accepted` for the life of the process (Q4: retention, not
    // restart-survival, is the contract for the volatile backend, and
    // there is no "reopen" event to reconcile against).
    let mut mem = MemoryStore::new();
    let (frozen2, _signed2) = compose(&k, Kind::TextNote, "fire and forget 2", 200);
    let frozen2_id = frozen2.id;
    let receipt2_id = mem
        .accept_ephemeral(frozen2_id, k.public_key())
        .expect("accept_ephemeral persistence");
    assert!(mem.query(&Filter::new().id(frozen2_id)).is_empty());
    assert!(mem.recover_outbox().is_empty());
    let mem_receipt = mem
        .reattach_receipt(receipt2_id)
        .expect("receipt lookup must be readable")
        .expect("ephemeral receipt reattachable on MemoryStore too");
    assert_eq!(mem_receipt.intent_id, None);
    assert_eq!(mem_receipt.state, ReceiptState::Accepted);
}

/// Architecture-review blocker: `promote_signed`/`compensate_write` used to
/// locate an intent by reading the CURRENT row at its frozen event id —
/// which a `Duplicate` (the row belongs to a DIFFERENT provenance) or
/// `Stale` (no row was ever stored) intent never has. Both must still be
/// promotable/compensable via their own `IntentId`, keyed off the intent's
/// own `OUTBOX_INTENTS.frozen_json`, not off `EVENTS`.
#[test]
fn duplicate_and_stale_intents_are_promotable_and_compensable_via_intent_id() {
    for_each_backend(|store| {
        let k = keys();

        // Duplicate: the exact same frozen body accepted twice.
        let (frozen_dup, signed_dup) = compose(&k, Kind::TextNote, "same content", 100);
        let outcome1 = do_accept(store, accept(frozen_dup.clone(), k.public_key(), 100));
        assert!(matches!(outcome1, AcceptOutcome::Inserted { .. }));
        let outcome2 = do_accept(store, accept(frozen_dup, k.public_key(), 100));
        let intent_dup = outcome2.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome2, AcceptOutcome::Duplicate { .. }));

        let promoted_dup = store
            .promote_signed(intent_dup, signed_dup.sig)
            .expect("promote persistence");
        assert!(
            matches!(promoted_dup, PromoteOutcome::Promoted { .. }),
            "a Duplicate intent's row belongs to someone else, but it must still promote via its own IntentId"
        );

        // Stale: an older candidate accepted after a newer one already won.
        let (frozen_new, _signed_new) = compose(&k, Kind::ContactList, "newer", 200);
        do_accept(store, accept(frozen_new, k.public_key(), 200));

        let (frozen_old, signed_old) = compose(&k, Kind::ContactList, "older", 100);
        let outcome_stale = do_accept(store, accept(frozen_old, k.public_key(), 100));
        let intent_stale = outcome_stale.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_stale, AcceptOutcome::Stale { .. }));

        let promoted_stale = store
            .promote_signed(intent_stale, signed_old.sig)
            .expect("promote persistence");
        match promoted_stale {
            PromoteOutcome::Promoted { row, .. } => {
                assert_eq!(row.event.sig, signed_old.sig);
            }
            other => panic!("expected Promoted for a Stale intent, got {other:?}"),
        }

        // A second Stale intent, compensated instead of promoted.
        let (frozen_old2, _signed_old2) = compose(&k, Kind::ContactList, "older2", 100);
        let outcome_stale2 = do_accept(store, accept(frozen_old2, k.public_key(), 100));
        let intent_stale2 = outcome_stale2.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_stale2, AcceptOutcome::Stale { .. }));
        let compensated_stale2 = store
            .compensate_write(intent_stale2)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_stale2,
            CompensateOutcome::Compensated { restored: None, .. }
        ));
    });
}

/// Architecture-review blocker: a stashed pending predecessor "can later
/// sign or cancel" — its copy in the displacing intent's `OUTBOX_DISPLACED`
/// must never resurrect STALE state. Signing a displaced intent must sync
/// the real signature into its stash copy, so that cancelling the intent
/// that displaced it restores the SIGNED bytes, not the original sentinel.
#[test]
fn chained_local_supersession_promote_displaced_then_cancel_newer_restores_signed_predecessor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let k = keys();

    let (frozen_a, signed_a) = compose(&k, Kind::ContactList, "a", 100);
    let frozen_a_id = frozen_a.id;
    let outcome_a = do_accept(&mut store, accept(frozen_a, k.public_key(), 100));
    let intent_a = outcome_a.journaled_intent_id().expect("journaled");

    let (frozen_b, _signed_b) = compose(&k, Kind::ContactList, "b", 200);
    let outcome_b = do_accept(&mut store, accept(frozen_b, k.public_key(), 200));
    let intent_b = outcome_b.journaled_intent_id().expect("journaled");
    assert!(matches!(outcome_b, AcceptOutcome::Superseded { .. }));

    // A is now displaced (stashed under B, since B superseded it). Sign it
    // while displaced.
    let promoted_a = store
        .promote_signed(intent_a, signed_a.sig)
        .expect("promote persistence");
    match promoted_a {
        PromoteOutcome::Promoted { row, .. } => {
            assert_eq!(row.event.id, frozen_a_id);
            assert_eq!(row.event.sig, signed_a.sig);
        }
        other => panic!("expected Promoted, got {other:?}"),
    }

    // Cancel B — restores its displaced predecessor (A), which must carry
    // the REAL signature synced in above, not the original sentinel.
    let compensated_b = store
        .compensate_write(intent_b)
        .expect("compensate persistence");
    match compensated_b {
        CompensateOutcome::Compensated { restored, .. } => {
            let restored = restored.expect("A restored");
            assert_eq!(restored.event.id, frozen_a_id);
            assert_eq!(
                restored.event.sig, signed_a.sig,
                "the restored predecessor must carry the REAL signature synced in while it was displaced, not a stale sentinel"
            );
            assert_eq!(
                restored
                    .provenance
                    .local
                    .expect("still carries local provenance")
                    .sig_state,
                SigState::Signed
            );
        }
        other => panic!("expected Compensated, got {other:?}"),
    }
}

/// Architecture-review blocker (the other half): cancelling a stashed
/// pending predecessor must invalidate its copy in the displacing intent's
/// stash for good — a LATER, unrelated cancellation of the displacing
/// intent must never resurrect an intent that was already permanently
/// rejected.
#[test]
fn chained_local_supersession_cancel_displaced_then_cancel_newer_never_resurrects() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::ContactList, "a", 100);
        let outcome_a = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let (frozen_b, _signed_b) = compose(&k, Kind::ContactList, "b", 200);
        let frozen_b_id = frozen_b.id;
        let outcome_b = do_accept(store, accept(frozen_b, k.public_key(), 200));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Superseded { .. }));

        // Cancel A WHILE it is displaced (stashed under B) — must
        // invalidate B's stash copy so a later cancel of B can never
        // resurrect A.
        let compensated_a = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_a,
            CompensateOutcome::Compensated { restored: None, .. }
        ));

        // Cancel B — must find NOTHING to restore (A was permanently
        // rejected, not merely superseded).
        let compensated_b = store
            .compensate_write(intent_b)
            .expect("compensate persistence");
        match compensated_b {
            CompensateOutcome::Compensated { restored, .. } => {
                assert!(
                    restored.is_none(),
                    "a cancelled intent must never be resurrected by a later, unrelated compensation"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert!(store.query(&Filter::new().id(frozen_b_id)).is_empty());
    });
}

/// Architecture-review blocker (generalization): an accepted intent's row
/// can disappear for reasons OTHER than local chained supersession — a
/// RELAY-observed event superseding it via the ordinary `insert` door is
/// one. The intent must remain compensable via its own `IntentId`
/// regardless.
#[test]
fn relay_supersession_orphans_pending_intent_still_compensable() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::ContactList, "local", 100);
        let frozen_a_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let relay_event = EventBuilder::new(Kind::ContactList, "from relay")
            .custom_created_at(Timestamp::from(200))
            .sign_with_keys(&k)
            .expect("sign relay event");
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        let insert_outcome =
            store.insert(relay_event, RelayObserved::new(relay, Timestamp::from(200)));
        assert!(matches!(insert_outcome, InsertOutcome::Superseded { .. }));
        assert!(
            store.query(&Filter::new().id(frozen_a_id)).is_empty(),
            "A's row is gone, superseded by the relay-observed event"
        );

        // Before the fix, `compensate_write(event_id)` would return
        // `NotFound` here since no live row carries A's id anymore.
        let compensated = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(
            matches!(compensated, CompensateOutcome::Compensated { .. }),
            "an orphaned intent must still be compensable via IntentId, got {compensated:?}"
        );
    });
}

/// Architecture-review blocker (generalization, continued): a kind:5
/// (NIP-09) deletion from the SAME author can also remove an accepted
/// intent's pending row via the ordinary `insert` door. The intent must
/// remain compensable via its own `IntentId`.
#[test]
fn kind5_deletion_orphans_pending_intent_still_compensable() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::TextNote, "will be deleted", 100);
        let frozen_a_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let deletion = deletion_event(&k, vec![Tag::event(frozen_a_id)], 200);
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        let insert_outcome =
            store.insert(deletion, RelayObserved::new(relay, Timestamp::from(200)));
        assert!(matches!(
            insert_outcome,
            InsertOutcome::Kind5Processed { .. }
        ));
        assert!(store.query(&Filter::new().id(frozen_a_id)).is_empty());

        let compensated = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(matches!(compensated, CompensateOutcome::Compensated { .. }));
    });
}

/// Architecture-review blocker (generalization, continued): a NIP-40
/// expiration sweep can also remove an accepted intent's pending row. The
/// intent must remain compensable via its own `IntentId`.
#[test]
fn expiry_orphans_pending_intent_still_compensable() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose_with_tags(
            &k,
            Kind::TextNote,
            "expires",
            100,
            vec![Tag::expiration(Timestamp::from(150u64))],
        );
        let frozen_a_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let due = store.expire_due(Timestamp::from(200u64));
        assert_eq!(due.len(), 1);
        assert!(store.query(&Filter::new().id(frozen_a_id)).is_empty());

        let compensated = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(matches!(compensated, CompensateOutcome::Compensated { .. }));
    });
}

/// Architecture-review blocker: a locally-composed kind:5 draft did not run
/// any tombstone-write processing at accept time, so its targets stayed
/// visible until the relay echoed the deletion back — conflicting with
/// issue #2's "no app optimistic mirror" promise. `accept_write` must
/// stage a suppression claim, in the SAME transaction, so the target is
/// hidden immediately and locally (architecture review requirement —
/// codex-nova's suppression-claim model, replacing a withdrawn design
/// that physically moved the target row into a per-intent stash; see
/// `AcceptOutcome::Kind5Processed`'s doc for why that was unsound).
#[test]
fn kind5_immediate_delete_hides_target_before_relay_echo() {
    for_each_backend(|store| {
        let k = keys();
        // The target is already held (e.g. relay-observed earlier).
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(target, RelayObserved::new(relay, Timestamp::from(50)));
        assert_eq!(store.query(&Filter::new().id(target_id)).len(), 1);

        // Locally compose + accept a kind:5 deleting it — BEFORE any relay
        // echo of the deletion itself.
        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome = do_accept(store, accept(deletion, k.public_key(), 100));
        let intent = outcome.journaled_intent_id().expect("journaled");
        match &outcome {
            AcceptOutcome::Kind5Processed { hidden, .. } => {
                assert_eq!(hidden.len(), 1);
                assert_eq!(hidden[0].event.id, target_id);
            }
            other => panic!("expected Kind5Processed, got {other:?}"),
        }

        // Immediate, local, optimistic HIDE — the target disappears from
        // `query` right now, no relay round-trip needed.
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // It was never actually removed: cancelling brings it straight
        // back, reported via `revealed`.
        let compensated = store
            .compensate_write(intent)
            .expect("compensate persistence");
        match compensated {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert_eq!(revealed.len(), 1);
                assert_eq!(revealed[0].event.id, target_id);
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert_eq!(store.query(&Filter::new().id(target_id)).len(), 1);
    });
}

/// Architecture-review requirement (the other half of the same fork): once
/// a pending kind:5 draft actually SIGNS, its suppression claims become
/// AUTHORITATIVE — the target is permanently, really removed, per
/// retraction-and-negative-deltas.md §7 — and can no longer be reversed by
/// a (now-invalid) later `compensate_write`.
#[test]
fn pending_kind5_delete_commits_to_permanent_on_promote() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(target, RelayObserved::new(relay, Timestamp::from(50)));

        let signed_deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let frozen_deletion = Event::new(
            signed_deletion.id,
            signed_deletion.pubkey,
            signed_deletion.created_at,
            signed_deletion.kind,
            signed_deletion.tags.clone(),
            signed_deletion.content.clone(),
            sentinel_signature(),
        );

        let outcome = do_accept(store, accept(frozen_deletion, k.public_key(), 100));
        let intent = outcome.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome, AcceptOutcome::Kind5Processed { .. }));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        let promoted = store
            .promote_signed(intent, signed_deletion.sig)
            .expect("promote persistence");
        assert!(matches!(promoted, PromoteOutcome::Promoted { .. }));

        // Compensation is pre-signature only (retraction doc §4.2's
        // "Promotion correction") — a no-op now, NOT a reversal.
        let compensated = store
            .compensate_write(intent)
            .expect("compensate persistence");
        assert!(matches!(compensated, CompensateOutcome::NotFound));
        assert!(
            store.query(&Filter::new().id(target_id)).is_empty(),
            "promotion must not be reversible — the delete is now permanent"
        );

        // The same PERMANENT tombstone retraction doc §7 governs now
        // refuses a redelivery of the byte-identical target.
        let redelivered = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign redelivered target");
        assert_eq!(redelivered.id, target_id);
        let relay2 = RelayUrl::parse("wss://relay2.example").expect("relay url");
        let reinsert = store.insert(
            redelivered,
            RelayObserved::new(relay2, Timestamp::from(300)),
        );
        assert!(matches!(
            reinsert,
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));
    });
}

/// Architecture-review requirement (codex-nova's suppression-claim model):
/// unlike a PERMANENT tombstone, a provisional suppression claim never
/// refuses an `insert` — a redelivered target is accepted and stored
/// normally (dedup-by-id merges provenance, same as any other redelivery),
/// it just stays hidden from `query` until the claim clears. This is the
/// key behavioral difference from the withdrawn stash-based design (which
/// had no live row left to dedup against in the first place).
#[test]
fn late_arrival_while_hidden_is_stored_not_refused_and_reveals_on_cancel() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(
            target.clone(),
            RelayObserved::new(relay, Timestamp::from(50)),
        );

        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome = do_accept(store, accept(deletion, k.public_key(), 100));
        let intent = outcome.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome, AcceptOutcome::Kind5Processed { .. }));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // A relay redelivery of the byte-identical target WHILE hidden —
        // must be accepted (dedup-by-id), NEVER `Refused`.
        let relay2 = RelayUrl::parse("wss://relay2.example").expect("relay url");
        let reinsert = store.insert(target, RelayObserved::new(relay2, Timestamp::from(300)));
        assert!(
            matches!(reinsert, InsertOutcome::Duplicate { .. }),
            "a redelivered target hidden by a PENDING claim must be accepted, not refused, got {reinsert:?}"
        );

        // Still hidden — the claim is still open.
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // Cancel — reveals it, with the redelivered observation's
        // provenance intact (proving the row was never actually removed).
        let compensated = store
            .compensate_write(intent)
            .expect("compensate persistence");
        match compensated {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert_eq!(revealed.len(), 1);
                assert_eq!(
                    revealed[0].provenance.seen.len(),
                    2,
                    "both the original and redelivered observations must have merged into the same retained row"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert_eq!(store.query(&Filter::new().id(target_id)).len(), 1);
    });
}

/// Issue #61's literal first-arrival falsifier: the target is absent when
/// the provisional claim is staged, then arrives for the first time while
/// suppressed. It must enter the canonical store, merge a second relay's
/// provenance in place, remain query-hidden, and reveal as one row with
/// both observations when the claim is cancelled.
#[test]
fn first_arrival_while_suppressed_is_retained_deduped_and_revealed_on_cancel() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "late target")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;

        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome = do_accept(
            store,
            accept(frozen_from_signed(&deletion), k.public_key(), 100),
        );
        let intent = outcome.journaled_intent_id().expect("journaled");
        assert!(matches!(
            outcome,
            AcceptOutcome::Kind5Processed { ref hidden, .. } if hidden.is_empty()
        ));

        let first = store.insert(
            target.clone(),
            RelayObserved::new(
                RelayUrl::parse("wss://relay1.example").expect("relay url"),
                Timestamp::from(150),
            ),
        );
        assert!(matches!(first, InsertOutcome::Inserted));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        let second = store.insert(
            target,
            RelayObserved::new(
                RelayUrl::parse("wss://relay2.example").expect("relay url"),
                Timestamp::from(200),
            ),
        );
        assert!(matches!(
            second,
            InsertOutcome::Duplicate {
                provenance_grew: true,
                ..
            }
        ));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        let cancelled = store
            .compensate_write(intent)
            .expect("compensate persistence");
        match cancelled {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert_eq!(revealed.len(), 1);
                assert_eq!(revealed[0].event.id, target_id);
                assert_eq!(revealed[0].provenance.seen.len(), 2);
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        let rows = store.query(&Filter::new().id(target_id));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provenance.seen.len(), 2);
    });
}

/// codex-nova P0 falsifier: a target's OWN `promote_signed` must remain
/// fully functional while it is hidden by an UNRELATED pending kind:5
/// intent's suppression claim. The withdrawn Kind5Stash design moved the
/// target row out of `EVENTS` entirely, making the target's own intent
/// blind to it (neither `promote_signed` nor `compensate_write` searches a
/// stash); the suppression-claim model never moves anything, so this must
/// just work.
#[test]
fn target_signs_while_hidden() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_t, signed_t) = compose(&k, Kind::TextNote, "target", 50);
        let target_id = frozen_t.id;
        let outcome_t = do_accept(store, accept(frozen_t, k.public_key(), 50));
        let intent_t = outcome_t.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_t, AcceptOutcome::Inserted { .. }));

        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome_d = do_accept(store, accept(deletion, k.public_key(), 100));
        let intent_d = outcome_d.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_d, AcceptOutcome::Kind5Processed { .. }));
        assert!(
            store.query(&Filter::new().id(target_id)).is_empty(),
            "hidden while D is pending"
        );

        // Sign the TARGET while it is hidden by D's still-open claim.
        let promoted_t = store
            .promote_signed(intent_t, signed_t.sig)
            .expect("promote persistence");
        match promoted_t {
            PromoteOutcome::Promoted { row, .. } => {
                assert_eq!(row.event.id, target_id);
                assert_eq!(row.event.sig, signed_t.sig);
            }
            other => panic!("expected Promoted even while hidden, got {other:?}"),
        }

        // Cancel D — must reveal the SIGNED bytes, not a stale sentinel.
        let compensated_d = store
            .compensate_write(intent_d)
            .expect("compensate persistence");
        match compensated_d {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert_eq!(revealed.len(), 1);
                assert_eq!(
                    revealed[0].event.sig, signed_t.sig,
                    "must reveal the REAL signature, not a stale sentinel"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        let rows = store.query(&Filter::new().id(target_id));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.sig, signed_t.sig);
    });
}

/// codex-nova P0 falsifier (the other half): cancelling the TARGET's own
/// intent must remain fully functional while it is hidden by an UNRELATED
/// pending kind:5 intent's claim, and a LATER cancel of the delete must
/// never resurrect the properly-cancelled target.
#[test]
fn target_cancels_while_hidden() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_t, _signed_t) = compose(&k, Kind::TextNote, "target", 50);
        let target_id = frozen_t.id;
        let outcome_t = do_accept(store, accept(frozen_t, k.public_key(), 50));
        let intent_t = outcome_t.journaled_intent_id().expect("journaled");

        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome_d = do_accept(store, accept(deletion, k.public_key(), 100));
        let intent_d = outcome_d.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_d, AcceptOutcome::Kind5Processed { .. }));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // Cancel the TARGET's own intent while hidden — must actually
        // remove it (the ordinary is-live compensate path, unaffected by
        // suppression).
        let compensated_t = store
            .compensate_write(intent_t)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_t,
            CompensateOutcome::Compensated { restored: None, .. }
        ));

        // Cancel D — must find NOTHING to reveal: the target is truly
        // gone (properly cancelled by its own owner), not merely hidden.
        let compensated_d = store
            .compensate_write(intent_d)
            .expect("compensate persistence");
        match compensated_d {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert!(
                    revealed.is_empty(),
                    "a properly-cancelled target must never be resurrected by an unrelated delete's cancel"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert!(store.query(&Filter::new().id(target_id)).is_empty());
    });
}

/// codex-nova P0 falsifier: two INDEPENDENT pending kind:5 intents naming
/// the SAME target must both need to clear before it reappears — hidden
/// while EITHER claim applies, visible again only once BOTH are dropped.
#[test]
fn overlapping_kind5_claims_hide_while_any_applies_reveal_only_when_all_drop() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(target, RelayObserved::new(relay, Timestamp::from(50)));

        // D1 and D2: two DIFFERENT (non-byte-identical) kind:5 drafts,
        // both naming the same target.
        let deletion_1 = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome_1 = do_accept(store, accept(deletion_1, k.public_key(), 100));
        let intent_1 = outcome_1.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_1, AcceptOutcome::Kind5Processed { .. }));

        let deletion_2 = deletion_event(&k, vec![Tag::event(target_id)], 101);
        let outcome_2 = do_accept(store, accept(deletion_2, k.public_key(), 101));
        let intent_2 = outcome_2.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_2, AcceptOutcome::Kind5Processed { .. }));

        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // Cancel D1 — D2's claim still applies, target stays hidden.
        let compensated_1 = store
            .compensate_write(intent_1)
            .expect("compensate persistence");
        match compensated_1 {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert!(
                    revealed.is_empty(),
                    "D2's overlapping claim must keep the target hidden"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // Cancel D2 — the last claim; the target must reappear now.
        let compensated_2 = store
            .compensate_write(intent_2)
            .expect("compensate persistence");
        match compensated_2 {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert_eq!(revealed.len(), 1);
                assert_eq!(revealed[0].event.id, target_id);
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert_eq!(store.query(&Filter::new().id(target_id)).len(), 1);
    });
}

/// Issue #61 literal order falsifier: cancelling one independent delete
/// must leave the other's suppression claim intact, and promoting that
/// remaining intent must convert its own claim into the permanent
/// tombstone without depending on the cancelled intent's metadata.
#[test]
fn independent_kind5_claims_cancel_then_promote_commit_the_remaining_delete() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "target")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(
            target.clone(),
            RelayObserved::new(relay, Timestamp::from(50)),
        );

        let signed_1 = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome_1 = do_accept(
            store,
            accept(frozen_from_signed(&signed_1), k.public_key(), 100),
        );
        let intent_1 = outcome_1.journaled_intent_id().expect("journaled");

        let signed_2 = deletion_event(&k, vec![Tag::event(target_id)], 101);
        let outcome_2 = do_accept(
            store,
            accept(frozen_from_signed(&signed_2), k.public_key(), 101),
        );
        let intent_2 = outcome_2.journaled_intent_id().expect("journaled");

        let cancelled = store
            .compensate_write(intent_1)
            .expect("compensate persistence");
        assert!(matches!(
            cancelled,
            CompensateOutcome::Compensated { ref revealed, .. } if revealed.is_empty()
        ));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        let promoted = store
            .promote_signed(intent_2, signed_2.sig)
            .expect("promote persistence");
        assert!(matches!(promoted, PromoteOutcome::Promoted { .. }));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        let replay = store.insert(
            target,
            RelayObserved::new(
                RelayUrl::parse("wss://relay2.example").expect("relay url"),
                Timestamp::from(300),
            ),
        );
        assert!(matches!(
            replay,
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));
    });
}

/// Issue #61's reverse-order falsifier: once either independent delete
/// promotes, the target is permanently deleted. Cancelling the other
/// still-pending intent may remove only its own provisional claim and
/// must neither reveal nor resurrect the target.
#[test]
fn independent_kind5_claims_promote_then_cancel_preserve_permanent_delete() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "target")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(
            target.clone(),
            RelayObserved::new(relay, Timestamp::from(50)),
        );

        let signed_1 = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome_1 = do_accept(
            store,
            accept(frozen_from_signed(&signed_1), k.public_key(), 100),
        );
        let intent_1 = outcome_1.journaled_intent_id().expect("journaled");

        let signed_2 = deletion_event(&k, vec![Tag::event(target_id)], 101);
        let outcome_2 = do_accept(
            store,
            accept(frozen_from_signed(&signed_2), k.public_key(), 101),
        );
        let intent_2 = outcome_2.journaled_intent_id().expect("journaled");

        let promoted = store
            .promote_signed(intent_1, signed_1.sig)
            .expect("promote persistence");
        assert!(matches!(promoted, PromoteOutcome::Promoted { .. }));

        let cancelled = store
            .compensate_write(intent_2)
            .expect("compensate persistence");
        assert!(matches!(
            cancelled,
            CompensateOutcome::Compensated { ref revealed, .. } if revealed.is_empty()
        ));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        let replay = store.insert(
            target,
            RelayObserved::new(
                RelayUrl::parse("wss://relay2.example").expect("relay url"),
                Timestamp::from(300),
            ),
        );
        assert!(matches!(
            replay,
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));
    });
}

/// A provisional claim overlapping an already-permanent tombstone is
/// allowed to own only its reversible metadata. Cancelling it cannot
/// erase the older permanent deletion or make a redelivery admissible.
#[test]
fn pending_kind5_claim_over_permanent_tombstone_cannot_reveal_target() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "target")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(
            target.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(50)),
        );

        let permanent = deletion_event(&k, vec![Tag::event(target_id)], 100);
        assert!(matches!(
            store.insert(permanent, RelayObserved::new(relay, Timestamp::from(100))),
            InsertOutcome::Kind5Processed { .. }
        ));

        let pending = deletion_event(&k, vec![Tag::event(target_id)], 101);
        let outcome = do_accept(
            store,
            accept(frozen_from_signed(&pending), k.public_key(), 101),
        );
        let intent = outcome.journaled_intent_id().expect("journaled");
        assert!(matches!(
            outcome,
            AcceptOutcome::Kind5Processed { ref hidden, .. } if hidden.is_empty()
        ));

        let cancelled = store
            .compensate_write(intent)
            .expect("compensate persistence");
        assert!(matches!(
            cancelled,
            CompensateOutcome::Compensated { ref revealed, .. } if revealed.is_empty()
        ));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());
        assert!(matches!(
            store.insert(
                target,
                RelayObserved::new(
                    RelayUrl::parse("wss://relay2.example").expect("relay url"),
                    Timestamp::from(300),
                ),
            ),
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));
    });
}

/// One kind:5 intent may name ordinary `e` targets and addressable `a`
/// targets together. Both claims are staged atomically and cancelling the
/// one intent reveals both rows through the same lifecycle.
#[test]
fn mixed_e_and_a_tag_kind5_intent_hides_and_reveals_both_targets() {
    for_each_backend(|store| {
        let k = keys();
        let regular = EventBuilder::new(Kind::TextNote, "regular")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign regular target");
        let regular_id = regular.id;
        let addressable = EventBuilder::new(Kind::from(30_003u16), "addressable")
            .tag(Tag::identifier("g1"))
            .custom_created_at(Timestamp::from(60))
            .sign_with_keys(&k)
            .expect("sign addressable target");
        let addressable_id = addressable.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(
            regular,
            RelayObserved::new(relay.clone(), Timestamp::from(50)),
        );
        store.insert(addressable, RelayObserved::new(relay, Timestamp::from(60)));

        let coord = Coordinate::new(Kind::from(30_003u16), k.public_key()).identifier("g1");
        let signed_delete = deletion_event(
            &k,
            vec![Tag::event(regular_id), Tag::coordinate(coord, None)],
            100,
        );
        let outcome = do_accept(
            store,
            accept(frozen_from_signed(&signed_delete), k.public_key(), 100),
        );
        let intent = outcome.journaled_intent_id().expect("journaled");
        match outcome {
            AcceptOutcome::Kind5Processed { hidden, .. } => {
                let mut hidden_ids: Vec<_> = hidden.into_iter().map(|row| row.event.id).collect();
                hidden_ids.sort();
                let mut expected = vec![regular_id, addressable_id];
                expected.sort();
                assert_eq!(hidden_ids, expected);
            }
            other => panic!("expected Kind5Processed, got {other:?}"),
        }
        assert!(store.query(&Filter::new().id(regular_id)).is_empty());
        assert!(store.query(&Filter::new().id(addressable_id)).is_empty());

        let cancelled = store
            .compensate_write(intent)
            .expect("compensate persistence");
        match cancelled {
            CompensateOutcome::Compensated { revealed, .. } => {
                let mut revealed_ids: Vec<_> =
                    revealed.into_iter().map(|row| row.event.id).collect();
                revealed_ids.sort();
                let mut expected = vec![regular_id, addressable_id];
                expected.sort();
                assert_eq!(revealed_ids, expected);
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
    });
}

/// Promotion of one mixed deletion intent must convert both provisional
/// claim classes into their permanent NIP-09 forms in the same lifecycle:
/// the exact `e` target is id-tombstoned and an at-or-before-ceiling
/// candidate for the `a` target is address-tombstoned.
#[test]
fn mixed_e_and_a_tag_kind5_promotion_permanently_deletes_both_targets() {
    for_each_backend(|store| {
        let k = keys();
        let regular = EventBuilder::new(Kind::TextNote, "regular")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign regular target");
        let regular_id = regular.id;
        let addressable = EventBuilder::new(Kind::from(30_003u16), "addressable")
            .tag(Tag::identifier("g1"))
            .custom_created_at(Timestamp::from(60))
            .sign_with_keys(&k)
            .expect("sign addressable target");
        let addressable_id = addressable.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(
            regular.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(50)),
        );
        store.insert(
            addressable,
            RelayObserved::new(relay.clone(), Timestamp::from(60)),
        );

        let coord = Coordinate::new(Kind::from(30_003u16), k.public_key()).identifier("g1");
        let signed_delete = deletion_event(
            &k,
            vec![Tag::event(regular_id), Tag::coordinate(coord, None)],
            100,
        );
        let outcome = do_accept(
            store,
            accept(frozen_from_signed(&signed_delete), k.public_key(), 100),
        );
        let intent = outcome.journaled_intent_id().expect("journaled");
        match outcome {
            AcceptOutcome::Kind5Processed { hidden, .. } => {
                assert_eq!(hidden.len(), 2);
            }
            other => panic!("expected Kind5Processed, got {other:?}"),
        }

        let promoted = store
            .promote_signed(intent, signed_delete.sig)
            .expect("promote persistence");
        assert!(matches!(promoted, PromoteOutcome::Promoted { .. }));
        assert!(store.query(&Filter::new().id(regular_id)).is_empty());
        assert!(store.query(&Filter::new().id(addressable_id)).is_empty());

        assert!(matches!(
            store.insert(
                regular,
                RelayObserved::new(relay.clone(), Timestamp::from(200)),
            ),
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));
        let address_replay = EventBuilder::new(Kind::from(30_003u16), "older replay")
            .tag(Tag::identifier("g1"))
            .custom_created_at(Timestamp::from(70))
            .sign_with_keys(&k)
            .expect("sign address replay");
        assert!(matches!(
            store.insert(
                address_replay,
                RelayObserved::new(relay, Timestamp::from(201)),
            ),
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));
    });
}

/// codex-nova P0 falsifier: an exact-`Duplicate` kind:5 intent's own
/// promotion must commit the deletion for real — and, since A and B are
/// CO-OWNERS of the deletion event's own row (issue #2's ownership-set
/// model), B's promotion atomically transitions A's OWN journal to
/// `Signed` too (codex-nova ruling, tightened after review), so A's own
/// later `compensate_write` attempt correctly answers `NotFound` rather
/// than silently undoing B's already-promoted, permanent deletion. The
/// withdrawn Kind5Stash design got this backwards: a stashed row was the
/// ONLY thing giving the deletion effect, so promoting a claim-less
/// Duplicate committed nothing durable of its own, and cancelling the
/// canonical original could remove the shared row out from under the
/// duplicate's already-promoted deletion.
#[test]
fn duplicate_delete_b_promote_then_a_cancel_keeps_b_deletion() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(target, RelayObserved::new(relay, Timestamp::from(50)));

        let signed_deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let frozen_deletion = Event::new(
            signed_deletion.id,
            signed_deletion.pubkey,
            signed_deletion.created_at,
            signed_deletion.kind,
            signed_deletion.tags.clone(),
            signed_deletion.content.clone(),
            sentinel_signature(),
        );

        // A: the canonical, first-accepted draft — the one that actually
        // stages the suppression claim.
        let outcome_a = do_accept(store, accept(frozen_deletion.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_a, AcceptOutcome::Kind5Processed { .. }));

        // B: a byte-identical Duplicate — accept_write's dedup-by-id fast
        // path returns `Duplicate` WITHOUT staging its own claim.
        let outcome_b = do_accept(store, accept(frozen_deletion, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));

        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // Promote B — must commit the deletion for real, permanently, AND
        // atomically advance A's own routing obligation too (A is a
        // CO-OWNER of the deletion event's own row).
        let promoted_b = store
            .promote_signed(intent_b, signed_deletion.sig)
            .expect("promote persistence");
        match promoted_b {
            PromoteOutcome::Promoted { co_signed, .. } => {
                assert_eq!(co_signed, vec![intent_a]);
            }
            other => panic!("expected Promoted, got {other:?}"),
        }

        // A's OWN journal is already `Signed` (advanced by B's call
        // above) — its own (now redundant) compensation attempt correctly
        // answers `NotFound`, never undoing B's already-promoted,
        // permanent deletion.
        let compensated_a = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(
            matches!(compensated_a, CompensateOutcome::NotFound),
            "A's own journal is already Signed -- compensation is pre-signature only"
        );
        assert!(
            store.query(&Filter::new().id(target_id)).is_empty(),
            "the target must stay permanently deleted"
        );

        // The PERMANENT tombstone (retraction doc §7) now refuses a fresh
        // redelivery, same as any other promoted kind:5.
        let redelivered = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign redelivered target");
        assert_eq!(redelivered.id, target_id);
        let relay2 = RelayUrl::parse("wss://relay2.example").expect("relay url");
        let reinsert = store.insert(
            redelivered,
            RelayObserved::new(relay2, Timestamp::from(300)),
        );
        assert!(matches!(
            reinsert,
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));
    });
}

/// codex-nova requirement: the suppression-claim model handles a-tag
/// (addressable) targets the SAME way as e-tag ones — no longer deferred
/// to promotion (the withdrawn Kind5Stash design's ceiling-based PERMANENT
/// addr-tombstone mechanism was not safely provisional; a suppression
/// claim, being pure reversible metadata, has no such problem — see
/// `AcceptOutcome::Kind5Processed`'s doc).
#[test]
fn a_tag_kind5_claim_hides_addressable_winner_then_commits_on_promote() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_g1, _signed_g1) = compose_with_tags(
            &k,
            Kind::from(30_003u16),
            "g1 body",
            50,
            vec![Tag::identifier("g1")],
        );
        let g1_id = frozen_g1.id;
        let outcome_g1 = do_accept(store, accept(frozen_g1, k.public_key(), 50));
        assert!(matches!(outcome_g1, AcceptOutcome::Inserted { .. }));
        assert_eq!(store.query(&Filter::new().id(g1_id)).len(), 1);

        let coord = Coordinate::new(Kind::from(30_003u16), k.public_key()).identifier("g1");
        let signed_deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::coordinate(coord, None))
            .custom_created_at(Timestamp::from(100))
            .sign_with_keys(&k)
            .expect("sign a-tag deletion");
        let frozen_deletion = Event::new(
            signed_deletion.id,
            signed_deletion.pubkey,
            signed_deletion.created_at,
            signed_deletion.kind,
            signed_deletion.tags.clone(),
            signed_deletion.content.clone(),
            sentinel_signature(),
        );

        let outcome_d = do_accept(store, accept(frozen_deletion, k.public_key(), 100));
        let intent_d = outcome_d.journaled_intent_id().expect("journaled");
        match &outcome_d {
            AcceptOutcome::Kind5Processed { hidden, .. } => {
                assert_eq!(hidden.len(), 1);
                assert_eq!(hidden[0].event.id, g1_id);
            }
            other => panic!("expected Kind5Processed, got {other:?}"),
        }
        assert!(
            store.query(&Filter::new().id(g1_id)).is_empty(),
            "the addressable winner must be hidden immediately"
        );

        let promoted_d = store
            .promote_signed(intent_d, signed_deletion.sig)
            .expect("promote persistence");
        assert!(matches!(promoted_d, PromoteOutcome::Promoted { .. }));
        assert!(
            store.query(&Filter::new().id(g1_id)).is_empty(),
            "must stay gone — now permanently deleted"
        );

        // The PERMANENT addr-tombstone ceiling now blocks an
        // at-or-before-ceiling redelivery, same as `insert`'s own kind:5
        // a-tag path.
        let older_replay = EventBuilder::new(Kind::from(30_003u16), "old g1")
            .tag(Tag::identifier("g1"))
            .custom_created_at(Timestamp::from(70))
            .sign_with_keys(&k)
            .expect("sign older replay");
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        let stale_reinsert = store.insert(
            older_replay,
            RelayObserved::new(relay, Timestamp::from(300)),
        );
        assert!(matches!(
            stale_reinsert,
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));
    });
}

/// Issue #61 P0 required falsifier: a candidate created AFTER a pending
/// kind:5's own `created_at` must remain visible while that deletion is
/// still open — an address claim with no ceiling would incorrectly hide
/// every future winner at that address forever, which even a PERMANENT
/// tombstone does not do (retraction-and-negative-deltas.md §2's ceiling
/// rule: "a fresh post-deletion event at the same address wins normally").
#[test]
fn address_claim_ceiling_does_not_hide_post_ceiling_winner() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_r1, _signed_r1) = compose(&k, Kind::ContactList, "v1", 50);
        do_accept(store, accept(frozen_r1, k.public_key(), 50));

        let coord = Coordinate::new(Kind::ContactList, k.public_key());
        let signed_deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::coordinate(coord, None))
            .custom_created_at(Timestamp::from(100))
            .sign_with_keys(&k)
            .expect("sign a-tag deletion");
        let frozen_deletion = Event::new(
            signed_deletion.id,
            signed_deletion.pubkey,
            signed_deletion.created_at,
            signed_deletion.kind,
            signed_deletion.tags.clone(),
            signed_deletion.content.clone(),
            sentinel_signature(),
        );
        let outcome_d = do_accept(store, accept(frozen_deletion, k.public_key(), 100));
        let intent_d = outcome_d.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_d, AcceptOutcome::Kind5Processed { .. }));
        assert!(store
            .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
            .is_empty());

        // A NEW winner, created AFTER the pending deletion's own
        // timestamp, must NOT be hidden by D's claim — the provisional
        // ceiling must match the permanent one exactly. `v1` was only
        // ever HIDDEN, never removed, so it is still the current
        // `addr_index` winner ordinary supersession competes against —
        // `v2` (created later) correctly supersedes it.
        let (frozen_r2, _signed_r2) = compose(&k, Kind::ContactList, "v2 post-ceiling", 200);
        let r2_id = frozen_r2.id;
        let outcome_r2 = do_accept(store, accept(frozen_r2, k.public_key(), 200));
        assert!(matches!(outcome_r2, AcceptOutcome::Superseded { .. }));

        let rows = store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
        assert_eq!(
            rows.len(),
            1,
            "a post-ceiling winner must remain visible while an EARLIER pending delete is still open"
        );
        assert_eq!(rows[0].event.id, r2_id);

        // Cancelling D must not disturb the post-ceiling winner either.
        let compensated_d = store
            .compensate_write(intent_d)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_d,
            CompensateOutcome::Compensated { .. }
        ));
        assert_eq!(
            store
                .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
                .len(),
            1
        );
    });
}

/// Issue #61 P0 required falsifier: two overlapping address claims with
/// DIFFERENT ceilings must compose correctly — a candidate stays hidden
/// while ANY covering claim remains, and becomes visible only once every
/// claim that covers it has cleared.
#[test]
fn overlapping_address_claims_with_different_ceilings_compose_correctly() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_r, _signed_r) = compose(&k, Kind::ContactList, "v1", 50);
        do_accept(store, accept(frozen_r, k.public_key(), 50));

        let coord = Coordinate::new(Kind::ContactList, k.public_key());

        // D1: an EARLIER-ceiling pending delete (created_at=80) -- covers
        // v1 (created_at=50) but would NOT cover anything created after 80.
        let signed_d1 = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::coordinate(coord.clone(), None))
            .custom_created_at(Timestamp::from(80))
            .sign_with_keys(&k)
            .expect("sign d1");
        let frozen_d1 = Event::new(
            signed_d1.id,
            signed_d1.pubkey,
            signed_d1.created_at,
            signed_d1.kind,
            signed_d1.tags.clone(),
            signed_d1.content.clone(),
            sentinel_signature(),
        );
        let outcome_d1 = do_accept(store, accept(frozen_d1, k.public_key(), 80));
        let intent_d1 = outcome_d1.journaled_intent_id().expect("journaled");

        // D2: a LATER-ceiling pending delete (created_at=150) -- also
        // covers v1.
        let signed_d2 = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::coordinate(coord, None))
            .custom_created_at(Timestamp::from(150))
            .sign_with_keys(&k)
            .expect("sign d2");
        let frozen_d2 = Event::new(
            signed_d2.id,
            signed_d2.pubkey,
            signed_d2.created_at,
            signed_d2.kind,
            signed_d2.tags.clone(),
            signed_d2.content.clone(),
            sentinel_signature(),
        );
        let outcome_d2 = do_accept(store, accept(frozen_d2, k.public_key(), 150));
        let intent_d2 = outcome_d2.journaled_intent_id().expect("journaled");

        assert!(store
            .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
            .is_empty());

        // Cancel D1 (the earlier ceiling) -- D2's LATER ceiling still
        // covers v1 (50 <= 150), so it must stay hidden.
        let compensated_d1 = store
            .compensate_write(intent_d1)
            .expect("compensate persistence");
        match compensated_d1 {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert!(
                    revealed.is_empty(),
                    "D2's later ceiling must keep v1 hidden after D1 alone clears"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert!(store
            .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
            .is_empty());

        // Cancel D2 too -- now nothing covers v1, it reappears.
        let compensated_d2 = store
            .compensate_write(intent_d2)
            .expect("compensate persistence");
        match compensated_d2 {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert_eq!(revealed.len(), 1);
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert_eq!(
            store
                .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
                .len(),
            1
        );
    });
}

/// Issue #61 P0 required falsifier: the address-claim ceiling's
/// correctness (post-ceiling winners stay visible; cancel still reverses
/// it) must survive a durable restart — `RedbStore`-only.
#[test]
fn address_claim_ceiling_survives_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();
    let (frozen_r, _signed_r) = compose(&k, Kind::ContactList, "v1", 50);

    let intent_d = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        do_accept(&mut store, accept(frozen_r, k.public_key(), 50));

        let coord = Coordinate::new(Kind::ContactList, k.public_key());
        let signed_deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::coordinate(coord, None))
            .custom_created_at(Timestamp::from(100))
            .sign_with_keys(&k)
            .expect("sign a-tag deletion");
        let frozen_deletion = Event::new(
            signed_deletion.id,
            signed_deletion.pubkey,
            signed_deletion.created_at,
            signed_deletion.kind,
            signed_deletion.tags.clone(),
            signed_deletion.content.clone(),
            sentinel_signature(),
        );
        let outcome_d = do_accept(&mut store, accept(frozen_deletion, k.public_key(), 100));
        let intent_d = outcome_d.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_d, AcceptOutcome::Kind5Processed { .. }));
        assert!(store
            .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
            .is_empty());
        intent_d
    };

    let mut store = RedbStore::open(&path).expect("reopen redb store");
    // Still hidden after reopen.
    assert!(store
        .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
        .is_empty());

    // A post-ceiling winner still isn't hidden after reopen.
    let (frozen_r2, _signed_r2) = compose(&k, Kind::ContactList, "v2 post-ceiling", 200);
    let r2_id = frozen_r2.id;
    do_accept(&mut store, accept(frozen_r2, k.public_key(), 200));
    let rows = store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.id, r2_id);

    // Cancelling D after reopen still works correctly.
    let compensated = store
        .compensate_write(intent_d)
        .expect("compensate persistence");
    assert!(matches!(compensated, CompensateOutcome::Compensated { .. }));
}

/// Both reversible terminal paths must operate on suppression claims
/// loaded from disk, not merely on process-local bookkeeping. This uses a
/// real close/reopen boundary and exercises cancellation and promotion on
/// different pending deletes after that boundary.
#[test]
fn pending_kind5_cancel_and_promote_both_survive_real_redb_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();
    let target_cancel = EventBuilder::new(Kind::TextNote, "cancel target")
        .custom_created_at(Timestamp::from(50))
        .sign_with_keys(&k)
        .expect("sign cancel target");
    let target_cancel_id = target_cancel.id;
    let target_promote = EventBuilder::new(Kind::TextNote, "promote target")
        .custom_created_at(Timestamp::from(51))
        .sign_with_keys(&k)
        .expect("sign promote target");
    let target_promote_id = target_promote.id;
    let signed_cancel_delete = deletion_event(&k, vec![Tag::event(target_cancel_id)], 100);
    let signed_promote_delete = deletion_event(&k, vec![Tag::event(target_promote_id)], 101);

    let (intent_cancel, intent_promote) = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(
            target_cancel.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(50)),
        );
        store.insert(
            target_promote.clone(),
            RelayObserved::new(relay, Timestamp::from(51)),
        );
        let cancel_outcome = do_accept(
            &mut store,
            accept(
                frozen_from_signed(&signed_cancel_delete),
                k.public_key(),
                100,
            ),
        );
        let promote_outcome = do_accept(
            &mut store,
            accept(
                frozen_from_signed(&signed_promote_delete),
                k.public_key(),
                101,
            ),
        );
        (
            cancel_outcome.journaled_intent_id().expect("journaled"),
            promote_outcome.journaled_intent_id().expect("journaled"),
        )
    };

    let mut store = RedbStore::open(&path).expect("reopen redb store");
    assert!(store.query(&Filter::new().id(target_cancel_id)).is_empty());
    assert!(store.query(&Filter::new().id(target_promote_id)).is_empty());
    let recovered = store.recover_outbox();
    assert!(recovered.iter().any(|row| row.intent_id == intent_cancel));
    assert!(recovered.iter().any(|row| row.intent_id == intent_promote));

    let cancelled = store
        .compensate_write(intent_cancel)
        .expect("post-restart compensation");
    assert!(matches!(
        cancelled,
        CompensateOutcome::Compensated { ref revealed, .. }
            if revealed.iter().any(|row| row.event.id == target_cancel_id)
    ));
    assert_eq!(store.query(&Filter::new().id(target_cancel_id)).len(), 1);

    let promoted = store
        .promote_signed(intent_promote, signed_promote_delete.sig)
        .expect("post-restart promotion");
    assert!(matches!(promoted, PromoteOutcome::Promoted { .. }));
    assert!(store.query(&Filter::new().id(target_promote_id)).is_empty());
    assert!(matches!(
        store.insert(
            target_promote,
            RelayObserved::new(
                RelayUrl::parse("wss://relay2.example").expect("relay url"),
                Timestamp::from(300),
            ),
        ),
        InsertOutcome::Refused(RefuseReason::Tombstoned)
    ));
}

/// A suppressed regular row is pinned against bounded GC because a later
/// cancellation still needs the canonical row to reveal. NIP-40 remains
/// an explicit semantic retraction: expiry may remove the hidden row, and
/// cancelling afterward must report no phantom reveal.
#[test]
fn suppressed_target_is_gc_pinned_but_nip40_expiry_still_removes_it() {
    for_each_backend(|store| {
        let k = keys();
        let gc_target = EventBuilder::new(Kind::TextNote, "gc target")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign gc target");
        let gc_target_id = gc_target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(
            gc_target,
            RelayObserved::new(relay.clone(), Timestamp::from(50)),
        );
        let gc_delete = deletion_event(&k, vec![Tag::event(gc_target_id)], 100);
        let gc_outcome = do_accept(
            store,
            accept(frozen_from_signed(&gc_delete), k.public_key(), 100),
        );
        let gc_intent = gc_outcome.journaled_intent_id().expect("journaled");

        let gc_report = store.gc(&ClaimSet::new(vec![]));
        assert_eq!(gc_report.events_evicted, 0);
        let cancelled = store
            .compensate_write(gc_intent)
            .expect("compensate persistence");
        assert!(matches!(
            cancelled,
            CompensateOutcome::Compensated { ref revealed, .. }
                if revealed.iter().any(|row| row.event.id == gc_target_id)
        ));
        assert_eq!(store.query(&Filter::new().id(gc_target_id)).len(), 1);
        let post_cancel_gc = store.gc(&ClaimSet::new(vec![]));
        assert_eq!(post_cancel_gc.events_evicted, 1);
        assert!(store.query(&Filter::new().id(gc_target_id)).is_empty());

        let expiring_target = EventBuilder::new(Kind::TextNote, "expiry target")
            .tag(Tag::expiration(Timestamp::from(250u64)))
            .custom_created_at(Timestamp::from(150))
            .sign_with_keys(&k)
            .expect("sign expiring target");
        let expiring_target_id = expiring_target.id;
        store.insert(
            expiring_target,
            RelayObserved::new(relay, Timestamp::from(150)),
        );
        let expiry_delete = deletion_event(&k, vec![Tag::event(expiring_target_id)], 200);
        let expiry_outcome = do_accept(
            store,
            accept(frozen_from_signed(&expiry_delete), k.public_key(), 200),
        );
        let expiry_intent = expiry_outcome.journaled_intent_id().expect("journaled");
        assert!(store
            .query(&Filter::new().id(expiring_target_id))
            .is_empty());

        let expired = store.expire_due(Timestamp::from(300u64));
        assert!(expired.iter().any(|row| row.event.id == expiring_target_id));
        let cancelled = store
            .compensate_write(expiry_intent)
            .expect("compensate persistence");
        assert!(matches!(
            cancelled,
            CompensateOutcome::Compensated { ref revealed, .. } if revealed.is_empty()
        ));
        assert!(store
            .query(&Filter::new().id(expiring_target_id))
            .is_empty());
    });
}

/// Persistence-shape falsifier for issue #61's canonical-owner rule. A
/// provisional deletion makes the target query-invisible but leaves its
/// one full row in `events`; no copy is moved into the only other table
/// that owns full event rows (`outbox_displaced`). Cancelling then exposes
/// that same one row again.
#[test]
fn pending_suppression_has_one_persisted_event_row_owner_and_no_visible_copy() {
    const EVENTS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("events");
    const DISPLACED_TABLE: TableDefinition<&str, &str> = TableDefinition::new("outbox_displaced");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();
    let target = EventBuilder::new(Kind::TextNote, "one canonical owner")
        .custom_created_at(Timestamp::from(50))
        .sign_with_keys(&k)
        .expect("sign target");
    let target_id = target.id;
    let intent = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        store.insert(
            target,
            RelayObserved::new(
                RelayUrl::parse("wss://relay.example").expect("relay url"),
                Timestamp::from(50),
            ),
        );
        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome = do_accept(
            &mut store,
            accept(frozen_from_signed(&deletion), k.public_key(), 100),
        );
        assert!(store.query(&Filter::new().id(target_id)).is_empty());
        outcome.journaled_intent_id().expect("journaled")
    };

    let db = Database::open(&path).expect("inspect redb");
    let read = db.begin_read().expect("begin inspection read");
    let events = read.open_table(EVENTS_TABLE).expect("open events");
    assert!(events
        .get(target_id.to_hex().as_str())
        .expect("read target row")
        .is_some());
    let persisted_event_rows = events
        .iter()
        .expect("iterate events")
        .map(|entry| entry.expect("read events entry"))
        .filter(|(key, _)| key.value() == target_id.to_hex())
        .count();
    assert_eq!(persisted_event_rows, 1);

    let displaced = read
        .open_table(DISPLACED_TABLE)
        .expect("open displaced rows");
    let target_hex = target_id.to_hex();
    let displaced_target_copies = displaced
        .iter()
        .expect("iterate displaced rows")
        .map(|entry| entry.expect("read displaced entry"))
        .filter(|(_, value)| value.value().contains(&target_hex))
        .count();
    assert_eq!(displaced_target_copies, 0);
    drop(displaced);
    drop(events);
    drop(read);
    drop(db);

    let mut store = RedbStore::open(&path).expect("reopen redb store");
    let cancelled = store
        .compensate_write(intent)
        .expect("compensate persistence");
    assert!(matches!(cancelled, CompensateOutcome::Compensated { .. }));
    let visible = store.query(&Filter::new().id(target_id));
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].event.id, target_id);
}

/// Issue #2 required falsifier #1 (team-lead decision, canonical-row
/// ownership for byte-identical intents): a pending `Duplicate` B's still-
/// open obligation must keep the canonical row alive when the canonical
/// original A is cancelled — the row is owned by a SET, not coalesced
/// into whichever intent happened to accept first (see `LocalOrigin`'s
/// doc). Only once EVERY owner cancels does the row actually retract.
#[test]
fn duplicate_pending_b_survives_cancel_of_canonical_a() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::TextNote, "shared body", 100);
        let frozen_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_a, AcceptOutcome::Inserted { .. }));

        let outcome_b = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));

        // Cancel the CANONICAL original -- B's still-open obligation must
        // keep the canonical row alive.
        let compensated_a = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_a,
            CompensateOutcome::Compensated { restored: None, .. }
        ));

        let rows = store.query(&Filter::new().id(frozen_id));
        assert_eq!(
            rows.len(),
            1,
            "B's still-open duplicate obligation must keep the canonical row alive"
        );
        let local = rows[0]
            .provenance
            .local
            .as_ref()
            .expect("still locally owned");
        assert_eq!(local.sig_state, SigState::Pending);
        assert!(local.owners.contains(&intent_b));
        assert!(
            !local.owners.contains(&intent_a),
            "A must be removed from the owner set once cancelled"
        );

        // Cancelling B afterward retracts it for real -- nothing sustains
        // it anymore.
        let compensated_b = store
            .compensate_write(intent_b)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_b,
            CompensateOutcome::Compensated { restored: None, .. }
        ));
        assert!(store.query(&Filter::new().id(frozen_id)).is_empty());
    });
}

/// Issue #2 required falsifier #2 (tightened by codex-nova after review):
/// if the `Duplicate` B signs first, its promotion sets the canonical
/// row's signature in place AND atomically transitions the OTHER co-owner
/// A's own journal/receipt to `Signed` too, in the SAME call — never
/// lazily deferred until (or unless) A's own signer calls back. An
/// offline co-owner signer must never strand a receipt behind an event
/// that's already validly signed. A's own (now redundant) later
/// `compensate_write` call correctly answers `NotFound` — the row stays
/// signed and queryable throughout.
#[test]
fn duplicate_b_signs_then_a_cancels_leaves_signed_row_queryable() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::TextNote, "shared body", 100);
        let frozen_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let outcome_b = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));

        // A real signature over the exact same frozen bytes (same
        // pubkey/created_at/kind/content — the NIP-01 id never depends on
        // `sig`).
        let signed = EventBuilder::new(Kind::TextNote, "shared body")
            .custom_created_at(Timestamp::from(100))
            .sign_with_keys(&k)
            .expect("sign matching event");
        assert_eq!(signed.id, frozen_id);

        let promoted_b = store
            .promote_signed(intent_b, signed.sig)
            .expect("promote persistence");
        match promoted_b {
            PromoteOutcome::Promoted { co_signed, .. } => {
                assert_eq!(
                    co_signed,
                    vec![intent_a],
                    "B's promotion must atomically advance A's own routing obligation too"
                );
            }
            other => panic!("expected Promoted, got {other:?}"),
        }

        // A's OWN journal is already `Signed` (advanced by B's call above)
        // — its own (now redundant) promotion/compensation attempts must
        // both answer `NotFound`, never resurrect or re-transition
        // anything.
        let compensated_a = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(
            matches!(compensated_a, CompensateOutcome::NotFound),
            "A's own journal is already Signed -- compensation is pre-signature only"
        );

        let rows = store.query(&Filter::new().id(frozen_id));
        assert_eq!(
            rows.len(),
            1,
            "the row must stay signed and queryable throughout"
        );
        assert_eq!(rows[0].event.sig, signed.sig);
        let local = rows[0]
            .provenance
            .local
            .as_ref()
            .expect("still locally tracked");
        assert_eq!(local.sig_state, SigState::Signed);
    });
}

/// Issue #2 required falsifier #3: neither sequence above may leave an
/// open obligation without its canonical row after a durable restart —
/// `RedbStore`-only, since `MemoryStore` never survives a real process
/// crash (Fable checkpoint Q4).
#[test]
fn duplicate_ownership_survives_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();
    let (frozen_a, _signed_a) = compose(&k, Kind::TextNote, "shared body", 100);
    let frozen_id = frozen_a.id;

    let (intent_a, intent_b) = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let outcome_a = do_accept(&mut store, accept(frozen_a.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");
        let outcome_b = do_accept(&mut store, accept(frozen_a, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));
        // Cancel A before ever reopening -- only B's obligation should
        // survive.
        store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        (intent_a, intent_b)
    };

    let store = RedbStore::open(&path).expect("reopen redb store");
    let rows = store.query(&Filter::new().id(frozen_id));
    assert_eq!(
        rows.len(),
        1,
        "no open obligation may survive restart without its canonical row"
    );
    let local = rows[0]
        .provenance
        .local
        .as_ref()
        .expect("still locally owned");
    assert!(local.owners.contains(&intent_b));
    assert!(!local.owners.contains(&intent_a));

    let recovered = store.recover_outbox();
    assert!(recovered.iter().any(|r| r.intent_id == intent_b));
    assert!(
        !recovered.iter().any(|r| r.intent_id == intent_a),
        "A's compensated journal row must be gone"
    );
}

/// codex-nova ruling (tightened after review): a NEW duplicate accepted
/// AFTER the row it duplicates is ALREADY signed (by an earlier LOCAL
/// promotion) must itself start `Signed` and route the CANONICAL bytes —
/// an offline co-owner signer must never strand a receipt behind an event
/// that's already validly signed, and there is nothing left for a fresh
/// duplicate to sign.
#[test]
fn duplicate_of_already_signed_local_row_starts_signed() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, signed_a) = compose(&k, Kind::TextNote, "shared body", 100);
        let frozen_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let promoted_a = store
            .promote_signed(intent_a, signed_a.sig)
            .expect("promote persistence");
        assert!(matches!(promoted_a, PromoteOutcome::Promoted { .. }));

        // C: a fresh duplicate accepted AFTER the row is already signed.
        let (frozen_c, _signed_c) = compose(&k, Kind::TextNote, "shared body", 100);
        assert_eq!(frozen_c.id, frozen_id);
        let outcome_c = do_accept(store, accept(frozen_c, k.public_key(), 100));
        let intent_c = outcome_c.journaled_intent_id().expect("journaled");
        let receipt_c = outcome_c.journaled_receipt_id().expect("journaled");
        match outcome_c {
            AcceptOutcome::Duplicate { row, .. } => {
                assert_eq!(
                    row.event.sig, signed_a.sig,
                    "must route the CANONICAL signature"
                );
                let local = row.provenance.local.expect("still locally owned");
                assert_eq!(local.sig_state, SigState::Signed);
                assert!(local.owners.contains(&intent_c));
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }

        // C's own receipt must ALREADY be Signed -- nothing left to sign,
        // no obligation strands.
        let receipt = store
            .reattach_receipt(receipt_c)
            .expect("receipt lookup readable")
            .expect("receipt retained");
        assert_eq!(receipt.state, ReceiptState::Signed);

        // C's own compensation/promotion attempts are both correctly
        // refused -- it never had anything pending to begin with.
        let compensated_c = store
            .compensate_write(intent_c)
            .expect("compensate persistence");
        assert!(matches!(compensated_c, CompensateOutcome::NotFound));
        let promoted_c = store
            .promote_signed(intent_c, signed_a.sig)
            .expect("promote persistence");
        assert!(matches!(promoted_c, PromoteOutcome::NotFound));
    });
}

/// codex-nova ruling (tightened after review), relay variant: a duplicate
/// of a row that was NEVER locally accepted at all — purely relay-
/// observed, `local: None` — must likewise start `Signed` the first time
/// a LOCAL intent duplicates against it: a relay-observed row's own
/// `event.sig` is by construction already real, never a sentinel, so
/// there is nothing provisional about it either.
#[test]
fn duplicate_of_already_signed_relay_row_starts_signed() {
    for_each_backend(|store| {
        let k = keys();
        let relay_event = EventBuilder::new(Kind::TextNote, "relay body")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign relay event");
        let relay_id = relay_event.id;
        let relay_sig = relay_event.sig;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(relay_event, RelayObserved::new(relay, Timestamp::from(50)));
        // Sanity: purely relay-observed, no local provenance at all yet.
        let rows = store.query(&Filter::new().id(relay_id));
        assert!(rows[0].provenance.local.is_none());

        // D: a fresh LOCAL duplicate accepted against the relay-only row.
        let (frozen_d, _signed_d) = compose(&k, Kind::TextNote, "relay body", 50);
        assert_eq!(frozen_d.id, relay_id);
        let outcome_d = do_accept(store, accept(frozen_d, k.public_key(), 50));
        let intent_d = outcome_d.journaled_intent_id().expect("journaled");
        let receipt_d = outcome_d.journaled_receipt_id().expect("journaled");
        match outcome_d {
            AcceptOutcome::Duplicate { row, .. } => {
                assert_eq!(
                    row.event.sig, relay_sig,
                    "must route the CANONICAL (relay) signature"
                );
                let local = row
                    .provenance
                    .local
                    .expect("D's acceptance must attach local ownership for the first time");
                assert_eq!(local.sig_state, SigState::Signed);
                assert!(local.owners.contains(&intent_d));
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }

        let receipt = store
            .reattach_receipt(receipt_d)
            .expect("receipt lookup readable")
            .expect("receipt retained");
        assert_eq!(receipt.state, ReceiptState::Signed);
    });
}

/// Issue #2 P0 correction (codex-nova ruling): a relay delivering the REAL
/// signed event for a still-`Pending` local row through the ordinary
/// `insert` door is functionally the SAME signature-adoption/fan-out
/// invariant `promote_signed` performs explicitly. With TWO co-owners on
/// the row (via a `Duplicate` accept), the relay delivery must adopt the
/// signature, mark BOTH owners' own journals/receipts `Signed`, and fan
/// out -- an offline co-owner signer must never strand a receipt behind
/// an event a relay has already confirmed.
#[test]
fn relay_redelivery_onto_pending_duplicate_row_adopts_signature_and_fans_out_all_owners() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, signed_a) = compose(&k, Kind::TextNote, "shared body", 100);
        let frozen_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let outcome_b = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));

        // Neither owner has signed yet -- the row is still Pending.
        let rows = store.query(&Filter::new().id(frozen_id));
        assert_eq!(
            rows[0].provenance.local.as_ref().unwrap().sig_state,
            SigState::Pending
        );

        // A relay independently delivers the REAL signed bytes.
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        let insert_outcome = store.insert(
            signed_a.clone(),
            RelayObserved::new(relay, Timestamp::from(100)),
        );
        match insert_outcome {
            InsertOutcome::Duplicate {
                provenance_grew: true,
                satisfied_intents,
            } => assert_eq!(
                satisfied_intents.into_iter().collect::<BTreeSet<_>>(),
                BTreeSet::from([intent_a, intent_b])
            ),
            other => panic!("expected signed duplicate adoption, got {other:?}"),
        }

        // The row must now carry the real signature and be Signed.
        let rows = store.query(&Filter::new().id(frozen_id));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.sig, signed_a.sig);
        let local = rows[0]
            .provenance
            .local
            .as_ref()
            .expect("still locally owned");
        assert_eq!(local.sig_state, SigState::Signed);
        assert!(local.owners.contains(&intent_a));
        assert!(local.owners.contains(&intent_b));

        // BOTH owners' own journals were fanned out to Signed -- neither
        // can be promoted or compensated again.
        let promoted_a = store
            .promote_signed(intent_a, signed_a.sig)
            .expect("promote persistence");
        assert!(matches!(promoted_a, PromoteOutcome::NotFound));
        let promoted_b = store
            .promote_signed(intent_b, signed_a.sig)
            .expect("promote persistence");
        assert!(matches!(promoted_b, PromoteOutcome::NotFound));
        let compensated_a = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(matches!(compensated_a, CompensateOutcome::NotFound));
        let compensated_b = store
            .compensate_write(intent_b)
            .expect("compensate persistence");
        assert!(matches!(compensated_b, CompensateOutcome::NotFound));
    });
}

/// Issue #2 P0 correction (codex-nova ruling): `accept_write`'s duplicate
/// detection must ALSO search the `OUTBOX_DISPLACED` stash, not only the
/// live `EVENTS` row — a duplicate accepted while its canonical
/// predecessor is currently sitting displaced (superseded by a later
/// local edit, not yet restored) must join that stash entry's owner set
/// too, and that shared ownership must survive both an unrelated owner's
/// cancellation AND the later restore of the whole stash entry back to
/// live.
#[test]
fn duplicate_accepted_while_stashed_joins_owner_set_and_survives_restore_and_cancel() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::ContactList, "a", 100);
        let frozen_a_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_a, AcceptOutcome::Inserted { .. }));

        // C supersedes A -- A is now displaced into C's stash, owned by
        // {A}, still Pending.
        let (frozen_c, _signed_c) = compose(&k, Kind::ContactList, "c", 200);
        let outcome_c = do_accept(store, accept(frozen_c, k.public_key(), 200));
        let intent_c = outcome_c.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_c, AcceptOutcome::Superseded { .. }));

        // D: a fresh intent with BYTE-IDENTICAL frozen bytes to A, accepted
        // WHILE A sits displaced (not live). Must be detected as a
        // `Duplicate` against the STASHED entry, not treated as a fresh
        // insert.
        let outcome_d = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_d = outcome_d.journaled_intent_id().expect("journaled");
        match outcome_d {
            AcceptOutcome::Duplicate { row, .. } => {
                assert_eq!(row.event.id, frozen_a_id);
                let local = row
                    .provenance
                    .local
                    .expect("stash entry already carried local provenance");
                assert_eq!(local.sig_state, SigState::Pending);
                assert!(local.owners.contains(&intent_a));
                assert!(local.owners.contains(&intent_d));
            }
            other => panic!("expected Duplicate (joined via the displaced stash), got {other:?}"),
        }

        // Cancelling A (an unrelated owner of the SAME stash entry) must
        // only drop A's own ownership -- D's still-open obligation keeps
        // the stash entry alive, nothing to restore under A's own key
        // (A never displaced anyone itself).
        let compensated_a = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_a,
            CompensateOutcome::Compensated { restored: None, .. }
        ));

        // Cancelling C restores the shared stash entry to live -- now
        // owned ONLY by D (A already dropped out above).
        let compensated_c = store
            .compensate_write(intent_c)
            .expect("compensate persistence");
        match compensated_c {
            CompensateOutcome::Compensated { restored, .. } => {
                let restored = restored.expect("A's slot must survive cancelling C");
                assert_eq!(restored.event.id, frozen_a_id);
                let local = restored.provenance.local.expect("still locally owned");
                assert_eq!(local.sig_state, SigState::Pending);
                assert!(local.owners.contains(&intent_d));
                assert!(
                    !local.owners.contains(&intent_a),
                    "A must stay removed from the owner set"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }

        // Cancelling D last retracts it for real -- nothing sustains the
        // row anymore.
        let compensated_d = store
            .compensate_write(intent_d)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_d,
            CompensateOutcome::Compensated { restored: None, .. }
        ));
        assert!(store.query(&Filter::new().id(frozen_a_id)).is_empty());
    });
}

/// `RedbStore`-only durable-reopen variant of the falsifier above: an
/// owner set joined via the DISPLACED stash (not the live row) must
/// survive a real process restart just like the live-row case does
/// (`duplicate_ownership_survives_restart`).
#[test]
fn duplicate_accepted_while_stashed_survives_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let k = keys();
    let (frozen_a, _signed_a) = compose(&k, Kind::ContactList, "a", 100);
    let frozen_a_id = frozen_a.id;

    let (intent_a, intent_c, intent_d) = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let outcome_a = do_accept(&mut store, accept(frozen_a.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let (frozen_c, _signed_c) = compose(&k, Kind::ContactList, "c", 200);
        let outcome_c = do_accept(&mut store, accept(frozen_c, k.public_key(), 200));
        let intent_c = outcome_c.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_c, AcceptOutcome::Superseded { .. }));

        let outcome_d = do_accept(&mut store, accept(frozen_a, k.public_key(), 100));
        let intent_d = outcome_d.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_d, AcceptOutcome::Duplicate { .. }));

        (intent_a, intent_c, intent_d)
    };

    let mut store = RedbStore::open(&path).expect("reopen redb store");
    let recovered = store.recover_outbox();
    assert!(recovered.iter().any(|r| r.intent_id == intent_a));
    assert!(recovered.iter().any(|r| r.intent_id == intent_c));
    assert!(recovered.iter().any(|r| r.intent_id == intent_d));

    // Restore C after reopen -- the shared stash entry (owned by A and D)
    // must come back intact.
    let compensated_c = store
        .compensate_write(intent_c)
        .expect("compensate persistence");
    match compensated_c {
        CompensateOutcome::Compensated { restored, .. } => {
            let restored = restored.expect("A's slot must survive reopen + cancelling C");
            assert_eq!(restored.event.id, frozen_a_id);
            let local = restored.provenance.local.expect("still locally owned");
            assert!(local.owners.contains(&intent_a));
            assert!(local.owners.contains(&intent_d));
        }
        other => panic!("expected Compensated, got {other:?}"),
    }
}

/// codex-nova ruling (cross-door reachability finding, backend-parity
/// falsifier for `reinsert_stashed`'s own id-collision branch): this branch
/// is NOT unreachable through the public `EventStore` door -- it is reached
/// by mixing the ordinary `remove`/`insert` doors with the outbox doors.
/// Concrete path: accept local addressable A; accept newer local B so A is
/// displaced into B's stash; remove B through the ORDINARY store door
/// (bypassing `compensate_write` entirely -- e.g. the caller is reacting to
/// B's own NIP-40 expiry or an unrelated GC-adjacent removal); a relay then
/// delivers the REAL signed form of A's exact event id through the
/// ordinary `insert` door -- since A is not live (B's slot, now empty,
/// governs address competition) and A is not yet in EVENTS either, this is
/// a plain fresh insert, landing A live with `local: None` (purely
/// relay-observed) while A's OLD sentinel-signed, locally-owned copy still
/// sits in B's stash. Compensating B then unconditionally restores B's OWN
/// stash entry (A) through `reinsert_stashed`, colliding with the row the
/// relay just planted. The union + Signed-dominance + adopt-signature +
/// fan-out logic must all fire: A's canonical row keeps the relay's real
/// signature, A's owner set is preserved, and -- the specific bug this
/// falsifier catches -- A's own still-open local intent must be told the
/// row is `Signed` even though the EXISTING (not the stashed) side was the
/// one already carrying the real signature.
#[test]
fn reinsert_stashed_collision_with_relay_signed_row_adopts_and_fans_out() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, signed_a) = compose(&k, Kind::ContactList, "a", 100);
        let frozen_a_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_a, AcceptOutcome::Inserted { .. }));

        // B supersedes A -- A is displaced into B's stash, owned by {A},
        // still Pending.
        let (frozen_b, _signed_b) = compose(&k, Kind::ContactList, "b", 200);
        let frozen_b_id = frozen_b.id;
        let outcome_b = do_accept(store, accept(frozen_b, k.public_key(), 200));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Superseded { .. }));

        // Remove B through the ORDINARY store door -- NOT compensate_write
        // -- freeing the address slot while B's own OUTBOX_INTENTS/
        // OUTBOX_DISPLACED entries (still holding A) are left untouched.
        let removed_b = store.remove(frozen_b_id, RetractReason::Rejected);
        assert!(removed_b.is_some(), "B must have been live to remove");
        assert!(store.query(&Filter::new().id(frozen_b_id)).is_empty());

        // A relay delivers A's REAL signed bytes through the ordinary
        // `insert` door. A is not live (B's slot is now empty) and not yet
        // in EVENTS -- this is a plain fresh insert: A becomes live with
        // `local: None`, a genuine (non-sentinel) signature.
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        let insert_outcome = store.insert(
            signed_a.clone(),
            RelayObserved::new(relay, Timestamp::from(100)),
        );
        assert!(matches!(insert_outcome, InsertOutcome::Inserted));
        let rows = store.query(&Filter::new().id(frozen_a_id));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.sig, signed_a.sig);
        assert!(
            rows[0].provenance.local.is_none(),
            "the relay-planted row must start purely relay-observed"
        );

        // Compensating B unconditionally restores B's OWN displaced stash
        // entry (A, sentinel-signed, owned by {intent_a}, Pending) --
        // colliding with the relay-planted live row for A's exact id.
        let compensated_b = store
            .compensate_write(intent_b)
            .expect("compensate persistence");
        match compensated_b {
            CompensateOutcome::Compensated { restored, .. } => {
                let restored = restored.expect("A's stash entry must survive the collision");
                assert_eq!(restored.event.id, frozen_a_id);
                assert_eq!(
                    restored.event.sig, signed_a.sig,
                    "the row must keep the REAL (relay) signature, never regress to A's stale sentinel"
                );
                let local = restored
                    .provenance
                    .local
                    .expect("A's stash-side ownership must be adopted onto the collided row");
                assert_eq!(
                    local.sig_state,
                    SigState::Signed,
                    "Signed dominance: the relay side was already signed"
                );
                assert!(local.owners.contains(&intent_a));
            }
            other => panic!("expected Compensated, got {other:?}"),
        }

        // The specific bug this falsifier targets: A's own still-open
        // local intent must have been told the row is Signed -- NOT left
        // stranded at Pending just because the EXISTING (relay) side, not
        // the stashed side, was the one already carrying the real
        // signature.
        let promoted_a = store
            .promote_signed(intent_a, signed_a.sig)
            .expect("promote persistence");
        assert!(
            matches!(promoted_a, PromoteOutcome::NotFound),
            "A's own journal must already be Signed (fanned out during the collision), not still open"
        );
        let compensated_a = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        assert!(
            matches!(compensated_a, CompensateOutcome::NotFound),
            "compensation is pre-signature only -- A's journal is already Signed"
        );
    });
}

/// Issue #2 required falsifier #4 (kind:5 variant): an exact-`Duplicate`
/// kind:5 intent's OWN independent suppression claim (issue #61 P0
/// correction) must keep a target hidden after the canonical original is
/// cancelled — the ownership-set model and the suppression-claim model
/// reinforce each other here.
#[test]
fn duplicate_kind5_intent_b_keeps_target_hidden_after_canonical_a_cancels() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(target, RelayObserved::new(relay, Timestamp::from(50)));

        let signed_deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let frozen_deletion = Event::new(
            signed_deletion.id,
            signed_deletion.pubkey,
            signed_deletion.created_at,
            signed_deletion.kind,
            signed_deletion.tags.clone(),
            signed_deletion.content.clone(),
            sentinel_signature(),
        );

        let outcome_a = do_accept(store, accept(frozen_deletion.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_a, AcceptOutcome::Kind5Processed { .. }));

        let outcome_b = do_accept(store, accept(frozen_deletion, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));

        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // Cancel the CANONICAL original A -- B's own independent claim
        // must keep the target hidden.
        let compensated_a = store
            .compensate_write(intent_a)
            .expect("compensate persistence");
        match compensated_a {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert!(
                    revealed.is_empty(),
                    "B's independent claim must keep the target hidden"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // Cancel B too -- now nothing claims it, the target reappears.
        let compensated_b = store
            .compensate_write(intent_b)
            .expect("compensate persistence");
        match compensated_b {
            CompensateOutcome::Compensated { revealed, .. } => {
                assert_eq!(revealed.len(), 1);
                assert_eq!(revealed[0].event.id, target_id);
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
        assert_eq!(store.query(&Filter::new().id(target_id)).len(), 1);
    });
}

/// codex-nova finding: the displaced-stash lookup `promote_signed`/
/// `compensate_write` use for a non-live intent must match on the STASH
/// ENTRY'S OWN owner-SET membership, not merely on frozen event id — two
/// DIFFERENT intents can share the same frozen event id (a real intent
/// and a byte-identical `Duplicate` of it). Under the ownership-set model
/// (issue #2, team-lead decision) B is NOT unrelated to A here — an exact
/// `Duplicate` is a CO-OWNER of the SAME stash slot — so compensating B
/// must only remove B from that shared slot's owner set, never touch A's
/// own membership or drop the slot outright while A still owns it.
/// Falsifier: accept A for event E; accept B as a `Duplicate` of the
/// identical E; accept C (a newer replaceable) so the shared A+B row is
/// stashed under C; compensate B — A's ownership, and the stash entry
/// itself, must survive (a later cancel of C must still restore A).
#[test]
fn compensating_a_duplicate_intent_only_drops_its_own_stash_ownership() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::ContactList, "a", 100);
        let frozen_a_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_a, AcceptOutcome::Inserted { .. }));

        // B: a byte-identical Duplicate of A's exact frozen body -- joins
        // A's owner set.
        let outcome_b = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));

        // C: a newer replaceable candidate that supersedes the shared A+B
        // row, stashing it under C's OUTBOX_DISPLACED entry.
        let (frozen_c, _signed_c) = compose(&k, Kind::ContactList, "c", 200);
        let outcome_c = do_accept(store, accept(frozen_c, k.public_key(), 200));
        let intent_c = outcome_c.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_c, AcceptOutcome::Superseded { .. }));

        // Compensating B only removes B from the shared stash entry's
        // owner set -- A's still-open obligation keeps it alive, so
        // nothing is returned as `restored` by THIS call (B never
        // displaced anything of its own).
        let compensated_b = store
            .compensate_write(intent_b)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_b,
            CompensateOutcome::Compensated { restored: None, .. }
        ));

        // A's stash entry must still be intact (now owned by A alone) —
        // cancelling C must still restore A.
        let compensated_c = store
            .compensate_write(intent_c)
            .expect("compensate persistence");
        match compensated_c {
            CompensateOutcome::Compensated { restored, .. } => {
                let restored =
                    restored.expect("A's still-open ownership must survive compensating B");
                assert_eq!(restored.event.id, frozen_a_id);
                let local = restored.provenance.local.expect("still locally owned by A");
                assert!(local.owners.contains(&intent_a));
                assert!(
                    !local.owners.contains(&intent_b),
                    "B must have been removed from the shared owner set"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
    });
}

/// codex-nova finding, promote variant: under the ownership-set model
/// (issue #2, team-lead decision) B is a CO-OWNER of A's shared stash
/// slot, so `promote_signed` on B legitimately syncs B's signature into
/// it — that is the whole point of "promotion by any owner promotes the
/// canonical event-level state." What must NOT happen is a SECOND,
/// DIFFERENT signature landing on the same row afterward: once B has
/// signed the shared slot, A's own (distinct) promotion attempt must be
/// refused, not silently overwrite B's real signature with a second one.
#[test]
fn promoting_a_duplicate_intent_syncs_shared_stash_and_blocks_a_second_signature() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, signed_a) = compose(&k, Kind::ContactList, "a", 100);
        let frozen_a_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a.clone(), k.public_key(), 100));
        let intent_a = outcome_a.journaled_intent_id().expect("journaled");

        let outcome_b = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));

        let (frozen_c, _signed_c) = compose(&k, Kind::ContactList, "c", 200);
        let outcome_c = do_accept(store, accept(frozen_c, k.public_key(), 200));
        let intent_c = outcome_c.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_c, AcceptOutcome::Superseded { .. }));

        // Promote B -- syncs B's signature into the SHARED stash entry
        // (B is a co-owner of it, not an unrelated bystander).
        let promoted_b = store
            .promote_signed(intent_b, signed_a.sig)
            .expect("promote persistence");
        assert!(matches!(promoted_b, PromoteOutcome::Promoted { .. }));

        // A's own (distinct) promotion attempt must now be refused: the
        // shared row already carries a real signature via co-owner B.
        let promoted_a = store
            .promote_signed(intent_a, signed_a.sig)
            .expect("promote persistence");
        assert!(
            matches!(promoted_a, PromoteOutcome::NotFound),
            "a second owner's promotion attempt on an already-signed shared row must be refused"
        );

        // Cancelling C restores the shared entry, still correctly Signed
        // with B's synced signature, still owned by both A and B (A never
        // successfully transitioned, so it stays a co-owner alongside B).
        let compensated_c = store
            .compensate_write(intent_c)
            .expect("compensate persistence");
        match compensated_c {
            CompensateOutcome::Compensated { restored, .. } => {
                let restored = restored.expect("the shared row must survive cancelling C");
                assert_eq!(restored.event.id, frozen_a_id);
                assert_eq!(restored.event.sig, signed_a.sig);
                let local = restored.provenance.local.expect("still locally owned");
                assert_eq!(local.sig_state, SigState::Signed);
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
    });
}

/// codex-nova finding: `promote_signed` did not guard `IntentSigState::Signed`
/// — a duplicate signer completion could re-promote, overwrite the
/// signature, and return `Promoted` again, risking a double-publish
/// (especially under `AtMostOnce`). A repeat promotion must be a no-op.
#[test]
fn repeat_promotion_of_an_already_signed_intent_is_a_no_op() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, signed) = compose(&k, Kind::TextNote, "hello", 100);
        let frozen_id = frozen.id;
        let outcome = do_accept(store, accept(frozen, k.public_key(), 100));
        let intent = outcome.journaled_intent_id().expect("journaled");

        let promoted = store
            .promote_signed(intent, signed.sig)
            .expect("promote persistence");
        assert!(matches!(promoted, PromoteOutcome::Promoted { .. }));

        // A second, distinct valid signature over the SAME frozen body
        // (e.g. a duplicate signer completion racing the first).
        let (_, other_signed) = compose(&k, Kind::TextNote, "hello", 100);
        assert_ne!(
            other_signed.sig, signed.sig,
            "need a genuinely different signature to prove no overwrite occurred"
        );

        let repeat = store
            .promote_signed(intent, other_signed.sig)
            .expect("promote persistence");
        assert!(
            matches!(repeat, PromoteOutcome::NotFound),
            "a repeat promotion must be a no-op, got {repeat:?}"
        );

        // The row must still carry the FIRST signature, never overwritten.
        let rows = store.query(&Filter::new().id(frozen_id));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.sig, signed.sig);
    });
}
