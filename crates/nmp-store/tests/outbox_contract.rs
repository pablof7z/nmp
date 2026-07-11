//! The durable write-outbox door contract (issues #2/#3, Unit U1 —
//! `docs/design/crashsafe-accepted-2-3-plan.md` + its Fable checkpoint
//! verdict R1-R8): `accept_write`/`promote_signed`/`compensate_write`/
//! `recover_outbox`. Mirrors `store_contract.rs`'s convention of running
//! shared-contract tests against BOTH `MemoryStore` and a fresh
//! `RedbStore`; recovery/atomicity tests that specifically need a durable
//! reopen are `RedbStore`-only.

use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, ClaimSet, CompensateOutcome, EventStore,
    IntentId, IntentSigState, LocalOrigin, MemoryStore, PromoteOutcome, RedbStore, RefuseReason,
    SigState, WriteDurability,
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

/// An `AcceptWrite` for `frozen`, tagged with `intent`/`receipt` (usually
/// equal — distinct params only so a test can tell them apart if it wants
/// to).
fn accept(
    intent: u64,
    receipt: u64,
    frozen: Event,
    expected_pubkey: nostr::PublicKey,
    accepted_at: u64,
) -> AcceptWrite {
    AcceptWrite {
        intent_id: IntentId(intent),
        receipt_id: receipt,
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

        let outcome = store.accept_write(accept(1, 1, frozen, k.public_key(), 100));
        match outcome {
            AcceptOutcome::Inserted { row } => {
                assert_eq!(row.event.id, frozen_id);
                assert_eq!(row.event.sig, sentinel_signature());
                let local = row
                    .provenance
                    .local
                    .expect("locally-accepted row carries local provenance");
                assert_eq!(local.intent_id, IntentId(1));
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
        store.accept_write(accept(2, 2, frozen, k.public_key(), 200));

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
        assert_eq!(local.intent_id, IntentId(2));
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
    store.accept_write(accept(10, 10, frozen_a, k.public_key(), 100));

    let (frozen_b, signed_b) = compose(&k, Kind::ContactList, "v2", 200);
    let frozen_b_id = frozen_b.id;
    let outcome = store.accept_write(accept(11, 11, frozen_b, k.public_key(), 200));
    match outcome {
        AcceptOutcome::Superseded { row, replaced } => {
            assert_eq!(replaced.event.id, frozen_a_id);
            assert_eq!(row.event.id, frozen_b_id);
        }
        other => panic!("expected Superseded, got {other:?}"),
    }

    // Before promotion, the intent's displaced stash is still open.
    let before = store.recover_outbox();
    let intent_before = before
        .iter()
        .find(|r| r.intent_id == IntentId(11))
        .expect("intent 11 still open");
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
    let intent_after = after.iter().find(|r| r.intent_id == IntentId(11)).expect(
        "intent 11 still open (not yet delivered — only compensate_write/full-delivery closes it)",
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
        store.accept_write(accept(20, 20, frozen_a, k.public_key(), 100));

        let (frozen_b, _signed_b) = compose(&k, Kind::ContactList, "v2", 200);
        let frozen_b_id = frozen_b.id;
        let outcome = store.accept_write(accept(21, 21, frozen_b.clone(), k.public_key(), 200));
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

        let outcome = store.accept_write(accept(50, 50, frozen, k.public_key(), 50));
        assert!(matches!(
            outcome,
            AcceptOutcome::Refused(RefuseReason::AlreadyExpired)
        ));

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
    store.accept_write(accept(51, 51, frozen, k.public_key(), 50));
    assert!(store.recover_outbox().is_empty());
}

#[test]
fn pending_row_is_not_gc_evicted_while_intent_open() {
    for_each_backend(|store| {
        let k = keys();
        let (frozen, signed) = compose(&k, Kind::TextNote, "unsigned draft", 100);
        let frozen_id = frozen.id;
        store.accept_write(accept(60, 60, frozen, k.public_key(), 100));

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

    {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let ok = store.accept_write(accept(40, 40, frozen_ok, k.public_key(), 100));
        assert!(matches!(ok, AcceptOutcome::Inserted { .. }));

        let refused = store.accept_write(accept(41, 41, frozen_exp, k.public_key(), 50));
        assert!(matches!(
            refused,
            AcceptOutcome::Refused(RefuseReason::AlreadyExpired)
        ));
        // Dropped here — reopening below is the only way to tell what
        // actually landed durably.
    }

    let store = RedbStore::open(&path).expect("reopen redb store");

    let ok_rows = store.query(&Filter::new().id(frozen_ok_id));
    assert_eq!(ok_rows.len(), 1, "the accepted row must survive reopen");
    let recovered = store.recover_outbox();
    assert!(
        recovered.iter().any(|r| r.intent_id == IntentId(40)),
        "its journal entry must survive TOGETHER with the row (same transaction)"
    );
    assert!(
        !recovered.iter().any(|r| r.intent_id == IntentId(41)),
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

    {
        let mut store = RedbStore::open(&path).expect("open redb store");
        let outcome = store.accept_write(accept(30, 30, frozen, k.public_key(), 100));
        assert!(matches!(outcome, AcceptOutcome::Inserted { .. }));
        // Dropped here WITHOUT ever calling `promote_signed` — simulates a
        // crash between acceptance and the signer's response.
    }

    let store = RedbStore::open(&path).expect("reopen redb store");
    let recovered = store.recover_outbox();
    assert_eq!(recovered.len(), 1);
    let intent = &recovered[0];
    assert_eq!(intent.intent_id, IntentId(30));
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
