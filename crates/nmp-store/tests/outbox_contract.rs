//! The durable write-outbox door contract (issues #2/#3, Unit U1 —
//! `docs/design/crashsafe-accepted-2-3-plan.md` + its Fable checkpoint
//! verdict R1-R8, plus the post-build architecture-review corrections on
//! `IntentId` allocation and receipt retention). Mirrors
//! `store_contract.rs`'s convention of running shared-contract tests
//! against BOTH `MemoryStore` and a fresh `RedbStore`; recovery/atomicity
//! tests that specifically need a durable reopen are `RedbStore`-only.

use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, ClaimSet, CompensateOutcome, EventStore,
    IntentSigState, LocalOrigin, MemoryStore, PromoteOutcome, ReceiptState, RedbStore,
    RefuseReason, SigState, WriteDurability,
};
use nostr::{Event, EventBuilder, Filter, Keys, Kind, Tag, Timestamp};

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

/// An `AcceptWrite` for `frozen`, tagged with `receipt_id`. `IntentId` is
/// deliberately NOT a parameter here — the store allocates it (architecture
/// review correction; see `nmp_store::IntentId`'s doc) and hands it back on
/// every journaled `AcceptOutcome` variant via `.journaled_intent_id()`.
fn accept(
    receipt_id: u64,
    frozen: Event,
    expected_pubkey: nostr::PublicKey,
    accepted_at: u64,
) -> AcceptWrite {
    AcceptWrite {
        receipt_id,
        frozen,
        expected_pubkey,
        signing_identity_ref: "local".to_string(),
        durability: WriteDurability::Durable,
        routing: "author-outbox".to_string(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(accepted_at),
    }
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

        let outcome = store.accept_write(accept(1, frozen, k.public_key(), 100));
        match outcome {
            AcceptOutcome::Inserted { intent_id, row } => {
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
        let outcome = store.accept_write(accept(2, frozen, k.public_key(), 200));
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
    store.accept_write(accept(10, frozen_a, k.public_key(), 100));

    let (frozen_b, signed_b) = compose(&k, Kind::ContactList, "v2", 200);
    let frozen_b_id = frozen_b.id;
    let outcome = store.accept_write(accept(11, frozen_b, k.public_key(), 200));
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
    let promoted = store.promote_signed(frozen_b_id, real_sig);
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
        store.accept_write(accept(20, frozen_a, k.public_key(), 100));

        let (frozen_b, _signed_b) = compose(&k, Kind::ContactList, "v2", 200);
        let frozen_b_id = frozen_b.id;
        let outcome = store.accept_write(accept(21, frozen_b.clone(), k.public_key(), 200));
        assert!(matches!(outcome, AcceptOutcome::Superseded { .. }));

        let compensated = store.compensate_write(frozen_b_id);
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
        use nmp_store::{RelayObserved, RetractReason};
        let _ = RetractReason::Rejected; // documents which reason `compensate_write` used
        let relay = nostr::RelayUrl::parse("wss://relay.example").expect("relay url");
        let reinsert = store.insert(frozen_b, RelayObserved::new(relay, Timestamp::from(300)));
        match reinsert {
            nmp_store::InsertOutcome::Superseded { replaced } => {
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
        let frozen_id = frozen.id;

        let outcome = store.accept_write(accept(50, frozen, k.public_key(), 50));
        assert!(matches!(
            outcome,
            AcceptOutcome::Refused(RefuseReason::AlreadyExpired)
        ));
        assert!(
            outcome.journaled_intent_id().is_none(),
            "a refused call must never allocate an IntentId either"
        );

        // No row.
        assert!(store.query(&Filter::new().id(frozen_id)).is_empty());
        // No journal residue either: there is nothing to compensate.
        assert!(matches!(
            store.compensate_write(frozen_id),
            CompensateOutcome::NotFound
        ));
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
    store.accept_write(accept(51, frozen, k.public_key(), 50));
    assert!(store.recover_outbox().is_empty());
}

#[test]
fn pending_row_is_not_gc_evicted_while_intent_open() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, signed) = compose(&k, Kind::TextNote, "unsigned draft", 100);
        let frozen_id = frozen.id;
        store.accept_write(accept(60, frozen, k.public_key(), 100));

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
        store.promote_signed(frozen_id, signed.sig);
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
        let ok = store.accept_write(accept(40, frozen_ok, k.public_key(), 100));
        let ok_intent_id = ok.journaled_intent_id().expect("journaled");
        assert!(matches!(ok, AcceptOutcome::Inserted { .. }));

        let refused = store.accept_write(accept(41, frozen_exp, k.public_key(), 50));
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

    let accepted_intent_id = {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let outcome = store.accept_write(accept(30, frozen, k.public_key(), 100));
        let intent_id = outcome.journaled_intent_id().expect("journaled");
        assert!(matches!(outcome, AcceptOutcome::Inserted { .. }));
        // Dropped here WITHOUT ever calling `promote_signed` — simulates a
        // crash between acceptance and the signer's response.
        intent_id
    };

    let store = RedbStore::open(&path).expect("reopen redb store");
    let recovered = store.recover_outbox();
    assert_eq!(recovered.len(), 1);
    let intent = &recovered[0];
    assert_eq!(intent.intent_id, accepted_intent_id);
    assert_eq!(intent.receipt_id, 30);
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
        let frozen1_id = frozen1.id;
        let outcome1 = store.accept_write(accept(1, frozen1, k.public_key(), 100));
        let id1 = outcome1.journaled_intent_id().expect("journaled");
        store.compensate_write(frozen1_id);

        let (frozen2, _signed2) = compose(&k, Kind::TextNote, "two", 200);
        let frozen2_id = frozen2.id;
        let outcome2 = store.accept_write(accept(2, frozen2, k.public_key(), 200));
        let id2 = outcome2.journaled_intent_id().expect("journaled");
        store.compensate_write(frozen2_id);

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
    let outcome3 = store.accept_write(accept(3, frozen3, k.public_key(), 300));
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

    let (intent_signed, intent_comp) = {
        let mut store = RedbStore::open(&path).expect("open redb store");

        let outcome_a = store.accept_write(accept(500, frozen_signed, k.public_key(), 100));
        let intent_signed = outcome_a.journaled_intent_id().expect("journaled");
        store.promote_signed(frozen_signed_id, signed.sig);

        let outcome_b = store.accept_write(accept(600, frozen_comp, k.public_key(), 200));
        let intent_comp = outcome_b.journaled_intent_id().expect("journaled");
        store.compensate_write(frozen_comp_id);

        (intent_signed, intent_comp)
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
        .reattach_receipt(500)
        .expect("signed receipt must still be reattachable");
    assert_eq!(receipt_signed.intent_id, Some(intent_signed));
    assert_eq!(receipt_signed.frozen_id, frozen_signed_id);
    assert_eq!(receipt_signed.state, ReceiptState::Signed);

    let receipt_comp = store
        .reattach_receipt(600)
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
    let outcome_fresh = mem.accept_write(accept(700, frozen_fresh, k.public_key(), 300));
    let intent_fresh = outcome_fresh.journaled_intent_id().expect("journaled");
    let receipt_fresh = mem
        .reattach_receipt(700)
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

    {
        let mut store = RedbStore::open(&path).expect("open redb store");
        store.accept_ephemeral(800, frozen_id, k.public_key());

        // No pending row: `accept_ephemeral` never touches `EVENTS`.
        assert!(store.query(&Filter::new().id(frozen_id)).is_empty());
        // No open-work/journal row: `recover_outbox` (OUTBOX_INTENTS-only)
        // sees nothing.
        assert!(store.recover_outbox().is_empty());

        // The receipt itself IS there, `Accepted`, receipt-only.
        let receipt = store
            .reattach_receipt(800)
            .expect("ephemeral receipt persists immediately");
        assert_eq!(receipt.intent_id, None, "receipt-only: nothing backs it");
        assert_eq!(receipt.frozen_id, frozen_id);
        assert_eq!(receipt.state, ReceiptState::Accepted);
        // Dropped here with no further transition — simulates the process
        // dying before any dispatch/ack tracking (out of this unit's
        // scope) ever advanced this receipt past `Accepted`.
    }

    let store = RedbStore::open(&path).expect("reopen redb store");

    // Still no pending row, still no open-work row, after reopen.
    assert!(store.query(&Filter::new().id(frozen_id)).is_empty());
    assert!(store.recover_outbox().is_empty());

    // But the receipt is reattachable, now correctly `Abandoned` — the
    // boot-time reconciliation `RedbStore::open()` runs.
    let receipt = store
        .reattach_receipt(800)
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
    mem.accept_ephemeral(900, frozen2_id, k.public_key());
    assert!(mem.query(&Filter::new().id(frozen2_id)).is_empty());
    assert!(mem.recover_outbox().is_empty());
    let mem_receipt = mem
        .reattach_receipt(900)
        .expect("ephemeral receipt reattachable on MemoryStore too");
    assert_eq!(mem_receipt.intent_id, None);
    assert_eq!(mem_receipt.state, ReceiptState::Accepted);
}
