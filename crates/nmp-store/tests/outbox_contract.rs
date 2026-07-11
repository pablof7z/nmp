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

use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, ClaimSet, CompensateOutcome, EventStore,
    InsertOutcome, IntentSigState, LocalOrigin, MemoryStore, PromoteOutcome, ReceiptState,
    RedbStore, RefuseReason, RelayObserved, RetractReason, SigState, WriteDurability,
};
use nostr::{Event, EventBuilder, Filter, Keys, Kind, RelayUrl, Tag, Timestamp};

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
                assert_eq!(local.intent_id, intent_id);
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
            .expect("app surface must be able to tell this row is pending");
        assert_eq!(local.sig_state, SigState::Pending);
        assert_eq!(local.intent_id, intent_id);
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
        PromoteOutcome::Promoted { row } => {
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
            CompensateOutcome::Compensated { restored } => {
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
        assert!(store.reattach_receipt(receipt1).is_some());
        assert!(store.reattach_receipt(receipt2).is_some());
        assert!(store.reattach_receipt(receipt3).is_some());

        (receipt1, receipt2, receipt3)
    };

    // Restart — the retained receipts still answer, the open set is empty.
    let mut store = RedbStore::open(&path).expect("reopen redb store");
    assert!(store.recover_outbox().is_empty());
    assert!(store.reattach_receipt(receipt1).is_some());
    assert!(store.reattach_receipt(receipt2).is_some());
    assert!(store.reattach_receipt(receipt3_ephemeral).is_some());

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
        .expect("signed receipt must still be reattachable");
    assert_eq!(receipt_signed.intent_id, Some(intent_signed));
    assert_eq!(receipt_signed.frozen_id, frozen_signed_id);
    assert_eq!(receipt_signed.state, ReceiptState::Signed);

    let receipt_comp = store
        .reattach_receipt(receipt_comp_id)
        .expect("compensated receipt must still be reattachable");
    assert_eq!(receipt_comp.intent_id, Some(intent_comp));
    assert_eq!(receipt_comp.frozen_id, frozen_comp_id);
    assert_eq!(receipt_comp.state, ReceiptState::Compensated);

    assert!(
        store.reattach_receipt(99_999).is_none(),
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
            PromoteOutcome::Promoted { row } => {
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
            CompensateOutcome::Compensated { restored: None }
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
        PromoteOutcome::Promoted { row } => {
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
        CompensateOutcome::Compensated { restored } => {
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
            CompensateOutcome::Compensated { restored: None }
        ));

        // Cancel B — must find NOTHING to restore (A was permanently
        // rejected, not merely superseded).
        let compensated_b = store
            .compensate_write(intent_b)
            .expect("compensate persistence");
        match compensated_b {
            CompensateOutcome::Compensated { restored } => {
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
/// issue #2's "no app optimistic mirror" promise. `accept_write` must run
/// the SAME author-verified tombstone-write processing `insert` runs, in
/// the SAME transaction, so the delete is immediate and local.
#[test]
fn kind5_immediate_delete_removes_target_before_relay_echo() {
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
        match &outcome {
            AcceptOutcome::Kind5Processed { deleted, .. } => {
                assert_eq!(deleted.len(), 1);
                assert_eq!(deleted[0].event.id, target_id);
            }
            other => panic!("expected Kind5Processed, got {other:?}"),
        }

        // Immediate, local, optimistic delete — the target is gone right
        // now, no relay round-trip needed.
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // A later redelivery of the (byte-identical) target is refused as
        // tombstoned.
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

/// Architecture-review requirement (codex-nova verdict: "all provisional
/// semantic side effects must compensate atomically before signature
/// promotion... especially delete/tombstone effects"): cancelling a still-
/// PENDING kind:5 draft must reverse its accept-time optimistic delete
/// atomically, restoring every target it removed — not merely close the
/// journal and leave the content gone.
#[test]
fn pending_kind5_delete_reverses_on_cancel_restoring_targets() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(target, RelayObserved::new(relay, Timestamp::from(50)));

        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome = do_accept(store, accept(deletion, k.public_key(), 100));
        let intent = outcome.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome, AcceptOutcome::Kind5Processed { .. }));
        assert!(store.query(&Filter::new().id(target_id)).is_empty());

        // Cancel BEFORE signing — the provisional delete must reverse
        // atomically.
        let compensated = store
            .compensate_write(intent)
            .expect("compensate persistence");
        assert!(matches!(compensated, CompensateOutcome::Compensated { .. }));

        let rows = store.query(&Filter::new().id(target_id));
        assert_eq!(
            rows.len(),
            1,
            "cancelling a pending kind:5 delete must restore its target"
        );
        assert_eq!(rows[0].event.id, target_id);
    });
}

/// Architecture-review requirement (the other half of the same fork): once
/// a pending kind:5 draft actually SIGNS, its provisional tombstone claims
/// become AUTHORITATIVE — permanent, per retraction-and-negative-deltas.md
/// §7 — and can no longer be reversed by a (now-invalid) later
/// `compensate_write`.
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

/// Architecture-review requirement: a provisional tombstone must refuse
/// redelivery of its target WHILE the intent is still pending — exactly
/// like a permanent one would — and that refusal must clear the instant
/// the intent is cancelled.
#[test]
fn provisional_tombstone_refuses_redelivery_while_pending_then_clears_on_cancel() {
    for_each_backend(|store| {
        let k = keys();
        let target = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign target");
        let target_id = target.id;
        let relay = RelayUrl::parse("wss://relay.example").expect("relay url");
        store.insert(target, RelayObserved::new(relay, Timestamp::from(50)));

        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 100);
        let outcome = do_accept(store, accept(deletion, k.public_key(), 100));
        let intent = outcome.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome, AcceptOutcome::Kind5Processed { .. }));

        // Still pending — a fresh redelivery of the byte-identical target
        // must be refused, same as a permanent tombstone would.
        let redelivered = EventBuilder::new(Kind::TextNote, "please delete me")
            .custom_created_at(Timestamp::from(50))
            .sign_with_keys(&k)
            .expect("sign redelivered target");
        assert_eq!(redelivered.id, target_id);
        let relay2 = RelayUrl::parse("wss://relay2.example").expect("relay url");
        let reinsert = store.insert(
            redelivered.clone(),
            RelayObserved::new(relay2.clone(), Timestamp::from(300)),
        );
        assert!(matches!(
            reinsert,
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        ));

        // Cancel — the provisional claim clears, and the SAME redelivery
        // must now be accepted.
        let compensated = store
            .compensate_write(intent)
            .expect("compensate persistence");
        assert!(matches!(compensated, CompensateOutcome::Compensated { .. }));

        let reinsert2 = store.insert(
            redelivered,
            RelayObserved::new(relay2, Timestamp::from(300)),
        );
        assert!(
            !matches!(reinsert2, InsertOutcome::Refused(_)),
            "cancelling a pending kind:5 delete must clear its provisional tombstone claim, got {reinsert2:?}"
        );
    });
}

/// codex-nova finding: the displaced-stash lookup `promote_signed`/
/// `compensate_write` use for a non-live intent must match on the STASH
/// ENTRY'S OWN `intent_id`, not merely on frozen event id — two DIFFERENT
/// intents can share the same frozen event id (a real intent and a
/// byte-identical `Duplicate` of it). Falsifier: accept A for event E;
/// accept B as a `Duplicate` of the identical E; accept C (a newer
/// replaceable) so A's row is stashed under C; compensate B — this must
/// NOT touch A's stash entry (a later cancel of C must still restore A).
#[test]
fn compensating_a_duplicate_intent_never_touches_an_unrelated_intents_stash() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen_a, _signed_a) = compose(&k, Kind::ContactList, "a", 100);
        let frozen_a_id = frozen_a.id;
        let outcome_a = do_accept(store, accept(frozen_a.clone(), k.public_key(), 100));
        assert!(matches!(outcome_a, AcceptOutcome::Inserted { .. }));

        // B: a byte-identical Duplicate of A's exact frozen body.
        let outcome_b = do_accept(store, accept(frozen_a, k.public_key(), 100));
        let intent_b = outcome_b.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_b, AcceptOutcome::Duplicate { .. }));

        // C: a newer replaceable candidate that supersedes A, stashing A's
        // row under C's OUTBOX_DISPLACED entry.
        let (frozen_c, _signed_c) = compose(&k, Kind::ContactList, "c", 200);
        let outcome_c = do_accept(store, accept(frozen_c, k.public_key(), 200));
        let intent_c = outcome_c.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome_c, AcceptOutcome::Superseded { .. }));

        // Compensating B — an unrelated intent that merely shares A's
        // event id — must be a no-op with respect to A's stash entry.
        let compensated_b = store
            .compensate_write(intent_b)
            .expect("compensate persistence");
        assert!(matches!(
            compensated_b,
            CompensateOutcome::Compensated { restored: None }
        ));

        // A's stash entry must still be intact — cancelling C must still
        // restore A.
        let compensated_c = store
            .compensate_write(intent_c)
            .expect("compensate persistence");
        match compensated_c {
            CompensateOutcome::Compensated { restored } => {
                let restored = restored
                    .expect("A's stash must survive compensating the unrelated Duplicate intent B");
                assert_eq!(restored.event.id, frozen_a_id);
            }
            other => panic!("expected Compensated, got {other:?}"),
        }
    });
}

/// codex-nova finding, promote variant: `promote_signed` on B similarly
/// must not write B's signature into A's unrelated stash entry, nor mark
/// A's `LocalOrigin` as `Signed`.
#[test]
fn promoting_a_duplicate_intent_never_touches_an_unrelated_intents_stash() {
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

        // Promote B with a signature that is NOT A's own.
        let (_, signed_unrelated) = compose(&k, Kind::ContactList, "unrelated-sig-source", 999);
        let promoted_b = store
            .promote_signed(intent_b, signed_unrelated.sig)
            .expect("promote persistence");
        assert!(matches!(promoted_b, PromoteOutcome::Promoted { .. }));

        // A's stash entry must still carry A's OWN sentinel signature,
        // never mutated by promoting the unrelated intent B.
        let compensated_c = store
            .compensate_write(intent_c)
            .expect("compensate persistence");
        match compensated_c {
            CompensateOutcome::Compensated { restored } => {
                let restored = restored
                    .expect("A's stash must survive promoting the unrelated Duplicate intent B");
                assert_eq!(restored.event.id, frozen_a_id);
                assert_eq!(
                    restored.event.sig,
                    sentinel_signature(),
                    "A's stash entry must not have been mutated by promoting the unrelated intent B"
                );
            }
            other => panic!("expected Compensated, got {other:?}"),
        }

        // A itself, now restored live by cancelling C, still promotes
        // independently with its OWN signature.
        let promoted_a = store
            .promote_signed(intent_a, signed_a.sig)
            .expect("promote persistence");
        assert!(matches!(promoted_a, PromoteOutcome::Promoted { .. }));
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
