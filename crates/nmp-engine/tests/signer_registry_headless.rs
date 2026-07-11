//! M4 §5 — `SignerRegistry` headless falsifier: two accounts registered via
//! [`nmp_engine::runtime::Handle::add_signer`], `set_active_account` must
//! move BOTH halves together (P3, ledger #10) -- the reactive read root AND
//! the capability `Effect::RequestSign` dispatches through. Deliberately
//! offline (an empty `FixtureDirectory`, `MemoryStore` pre-seeded directly
//! via `EventStore::insert` rather than a live relay round trip): the read
//! side's first batch is computed purely from the local store
//! (`EngineCore::on_subscribe`, zero I/O -- the same fact
//! `integration_capstone.rs`'s `watermark_cold_start_offline` documents), and
//! the write side's `WriteStatus::Signed` is delivered by `on_signed` BEFORE
//! routing is even attempted, so a directory with no known write relays
//! (routing later fails closed) does not stop this test from observing
//! whether the SIGN step itself used the correct account's key.

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use nmp_engine::core::RowDelta;
use nmp_engine::outbox::{Durability, WriteIntent, WritePayload, WriteRouting, WriteStatus};
use nmp_engine::runtime::{EngineThread, RowsMsg};
use nmp_grammar::{Binding, Filter, IdentityField};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_signer::LocalKeySigner;
use nmp_store::{EventStore, MemoryStore, RelayObserved};
use nostr::{EventId, Keys, Kind, RelayUrl, Timestamp, UnsignedEvent};

/// Same accumulate-deltas-into-a-snapshot idiom as the other runtime tests
/// (`nmp_engine::core::RowDelta`'s doc: the wire is deltas, never snapshots).
fn wait_for_rows(
    rx: &Receiver<RowsMsg>,
    timeout: Duration,
    pred: impl Fn(&BTreeSet<EventId>) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut current: BTreeSet<EventId> = BTreeSet::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok((deltas, _coverage)) => {
                for delta in deltas {
                    match delta {
                        RowDelta::Added(event) => {
                            current.insert(event.id);
                        }
                        RowDelta::Removed(id) => {
                            current.remove(&id);
                        }
                    }
                }
                if pred(&current) {
                    return true;
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Waits until `pred` matches some status on the stream (never assumes the
/// FIRST value is a terminal -- ledger #9).
fn wait_for_status(
    rx: &Receiver<WriteStatus>,
    timeout: Duration,
    pred: impl Fn(&WriteStatus) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(status) if pred(&status) => return true,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn reactive_kind1() -> LiveQuery {
    LiveQuery(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
        ..Filter::default()
    })
}

#[test]
fn set_active_account_switches_read_root_and_publish_signer_together() {
    let a = Keys::generate();
    let b = Keys::generate();
    let seed_relay = RelayUrl::parse("wss://seed.invalid").expect("parse seed relay url");

    // Pre-seed the store directly (no live relay in this test): one kind:1
    // post per account, so the reactive-authors subscription's very first
    // batch already distinguishes "a active" from "b active" with zero
    // network round trips.
    let mut store = MemoryStore::new();
    let a_post = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "a",
    )
    .sign_with_keys(&a)
    .expect("sign a's seed post");
    let b_post = UnsignedEvent::new(
        b.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "b",
    )
    .sign_with_keys(&b)
    .expect("sign b's seed post");
    store.insert(
        a_post.clone(),
        RelayObserved::new(seed_relay.clone(), Timestamp::now()),
    );
    store.insert(
        b_post.clone(),
        RelayObserved::new(seed_relay.clone(), Timestamp::now()),
    );

    // Empty directory -- no write relays known for anyone. The read side
    // never needs one (the local store already answers the first batch);
    // the write side's routing will fail closed AFTER `Signed` is already
    // observed, which is all this test needs (see the module doc).
    let dir = FixtureDirectory::new();

    let (engine_thread, handle) = EngineThread::spawn(store, dir, 10, Default::default());

    let pk_a = handle
        .add_signer(LocalKeySigner::new(a.clone()))
        .expect("LocalKeySigner always reports a public key");
    let pk_b = handle
        .add_signer(LocalKeySigner::new(b.clone()))
        .expect("LocalKeySigner always reports a public key");
    assert_eq!(pk_a, a.public_key());
    assert_eq!(pk_b, b.public_key());

    // ---- read root: active = a -> only a's post visible ------------------
    handle.set_active_account(Some(pk_a));
    let (_qh, rows_rx) = handle.subscribe(reactive_kind1());
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(5), |rows| rows
            .contains(&a_post.id)
            && !rows.contains(&b_post.id)),
        "active=a must resolve $currentPubkey to a, surfacing only a's seeded post"
    );

    // ---- switch: active = b -> read root re-roots to b's post ------------
    handle.set_active_account(Some(pk_b));
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(5), |rows| rows
            .contains(&b_post.id)
            && !rows.contains(&a_post.id)),
        "set_active_account(b) must re-root the SAME live subscription onto b's post, \
         dropping a's -- the read half of the coupled switch"
    );

    // ---- write: still active = b -> publishing AS b must sign ------------
    let unsigned_as_b = UnsignedEvent::new(
        b.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "published while b is the active account",
    );
    let receipt_as_b = handle.publish(WriteIntent {
        payload: WritePayload::Unsigned(unsigned_as_b),
        durability: Durability::AtMostOnce,
        routing: WriteRouting::AuthorOutbox,
    });
    assert!(
        wait_for_status(&receipt_as_b, Duration::from_secs(5), |s| matches!(
            s,
            WriteStatus::Signed(_)
        )),
        "publish after switching active to b must sign successfully with b's OWN key -- \
         the write half of the coupled switch"
    );

    // ---- write: still active = b, template authored as a -> must FAIL ----
    // Proves the switch actually moved the signing capability: a's key is
    // still registered (via add_signer above) but is no longer active, so
    // b's LocalKeySigner rejects a template whose pubkey doesn't match its
    // own (nmp-signer/src/local.rs's pubkey-mismatch guard) rather than
    // silently falling back to a's still-registered capability.
    let unsigned_as_a_while_b_active = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "wrongly templated for a while b is active",
    );
    let receipt_wrong = handle.publish(WriteIntent {
        payload: WritePayload::Unsigned(unsigned_as_a_while_b_active),
        durability: Durability::AtMostOnce,
        routing: WriteRouting::AuthorOutbox,
    });
    assert!(
        wait_for_status(&receipt_wrong, Duration::from_secs(5), |s| matches!(
            s,
            WriteStatus::Failed(_)
        )),
        "a template authored for a, published while b is active, must be REJECTED -- the \
         active signer never silently falls back to a different registered account"
    );

    // ---- switch back: active = a -> publish as a succeeds again ----------
    handle.set_active_account(Some(pk_a));
    let unsigned_as_a = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "published after switching back to a",
    );
    let receipt_as_a = handle.publish(WriteIntent {
        payload: WritePayload::Unsigned(unsigned_as_a),
        durability: Durability::AtMostOnce,
        routing: WriteRouting::AuthorOutbox,
    });
    assert!(
        wait_for_status(&receipt_as_a, Duration::from_secs(5), |s| matches!(
            s,
            WriteStatus::Signed(_)
        )),
        "switching active back to a must restore a's signing capability for the next publish"
    );

    handle.shutdown();
    engine_thread.join();
}

/// No active account at all (a fresh engine, or an explicit
/// `set_active_account(None)`) must fail every publish CLOSED --
/// `WriteStatus::Failed`, never a panic, never a silent fallback to whatever
/// signer happens to be registered (M4 §5: "the engine may start with zero
/// accounts, matching a logged-out launch").
#[test]
fn publish_with_no_active_account_fails_closed_never_panics() {
    let a = Keys::generate();
    let store = MemoryStore::new();
    let dir = FixtureDirectory::new();
    let (engine_thread, handle) = EngineThread::spawn(store, dir, 10, Default::default());

    // Register a signer but NEVER activate it.
    handle.add_signer(LocalKeySigner::new(a.clone()));

    let unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "nobody is active",
    );
    let receipt_rx = handle.publish(WriteIntent {
        payload: WritePayload::Unsigned(unsigned),
        durability: Durability::AtMostOnce,
        routing: WriteRouting::AuthorOutbox,
    });

    assert!(
        wait_for_status(&receipt_rx, Duration::from_secs(5), |s| matches!(
            s,
            WriteStatus::Failed(_)
        )),
        "publish with a registered-but-inactive signer must still fail closed -- \
         `active_signer()` must require BOTH registration AND the active pointer"
    );

    handle.shutdown();
    engine_thread.join();
}
