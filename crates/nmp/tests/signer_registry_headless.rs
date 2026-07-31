//! M4 §5 — `SignerRegistry` headless falsifier: two accounts registered via
//! [`nmp::mechanism::runtime::Handle::add_signer`]. `set_active_account` re-roots
//! reactive reads and authorizes default unsigned acceptance. Once accepted,
//! a write resolves the exact signer frozen at that boundary; later read-root
//! changes cannot redirect it. Deliberately
//! offline (an empty `FixtureRoutingFacts`, `MemoryStore` pre-seeded directly
//! via `EventStore::insert` rather than a live relay round trip): the read
//! side's first batch is computed purely from the local store
//! (`EngineCore::on_subscribe`, zero I/O -- the same fact
//! `integration_capstone.rs`'s `watermark_cold_start_offline` documents), and
//! the write side's `WriteStatus::Signed` is delivered by `on_signed` BEFORE
//! routing is even attempted, so a directory with no known write relays
//! (routing later fails closed) does not stop this test from observing
//! whether the SIGN step itself used the correct account's key.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nmp::mechanism::core::RelayAdmissionPolicy;
use nmp::mechanism::core::RowDelta;
use nmp::mechanism::delivery::WriteStatus;
use nmp::mechanism::runtime::{EngineThread, FifoReceiver, FifoRecvTimeoutError, RowsReceiver};
use nmp_grammar::{Binding, Filter, IdentityField};
use nmp_grammar::{Durability, EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_local_signer::LocalKeySigner;
use nmp_resolver::LiveQuery;
use nmp_router::FixtureRoutingFacts;
use nmp_signer::{
    SignerError, SignerOp, SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent,
    SigningCapability,
};
use nmp_store::{EventStore, MemoryStore, RelayObserved};
use nostr::{EventId, Keys, Kind, PublicKey, RelayUrl, Timestamp, UnsignedEvent};

/// #765: `LocalKeySigner` now owns its scalar in one canonical zeroizing
/// allocation and no longer accepts a `nostr::Keys`. These fixtures still
/// build identities as `Keys`, so hand the raw scalar across exactly here.
fn local_signer(keys: &Keys) -> LocalKeySigner {
    LocalKeySigner::from_secret_bytes(keys.secret_key().as_secret_bytes())
        .expect("fixture keys are valid secp256k1 scalars")
}

struct CountingSigner {
    pubkey: PublicKey,
    calls: Arc<AtomicUsize>,
}

impl SigningCapability for CountingSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(SignerPublicKey::new(self.pubkey.to_bytes()))
    }

    fn sign(&self, _unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        SignerOp::err(SignerError::Rejected(
            "counting signer must not be reached".to_string(),
        ))
    }
}

struct PubkeylessSigner;

impl SigningCapability for PubkeylessSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        None
    }

    fn sign(&self, _unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        SignerOp::err(SignerError::Unavailable)
    }
}

/// Same accumulate-deltas-into-a-snapshot idiom as the other runtime tests
/// (`nmp::mechanism::core::RowDelta`'s doc: the wire is deltas, never snapshots).
fn wait_for_rows(
    rx: &RowsReceiver,
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
            Ok((deltas, _coverage, _execution)) => {
                for delta in deltas {
                    match delta {
                        RowDelta::Added(row) => {
                            current.insert(row.event.id);
                        }
                        RowDelta::SourcesGrew { .. } => {}
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
    rx: &FifoReceiver<WriteStatus>,
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
            Err(FifoRecvTimeoutError::Timeout | FifoRecvTimeoutError::Closed) => return false,
            Err(FifoRecvTimeoutError::Lagged) => {
                panic!("fixture receipt stream must not lag")
            }
        }
    }
}

fn reactive_kind1() -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
        ..Filter::default()
    })
}

#[test]
fn active_account_reroots_reads_but_each_write_uses_its_frozen_author() {
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
    store
        .insert(
            a_post.clone(),
            RelayObserved::new(seed_relay.clone(), Timestamp::now()),
        )
        .unwrap();
    store
        .insert(
            b_post.clone(),
            RelayObserved::new(seed_relay.clone(), Timestamp::now()),
        )
        .unwrap();

    // Empty directory -- no write relays known for anyone. The read side
    // never needs one (the local store already answers the first batch);
    // the write side's routing will fail closed AFTER `Signed` is already
    // observed, which is all this test needs (see the module doc).
    let dir = FixtureRoutingFacts::new();

    let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
        store,
        dir,
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    let registration_a = handle
        .add_signer(local_signer(&a))
        .expect("LocalKeySigner always reports a public key");
    let registration_b = handle
        .add_signer(local_signer(&b))
        .expect("LocalKeySigner always reports a public key");
    let pk_a = registration_a.public_key();
    let pk_b = registration_b.public_key();
    assert_eq!(pk_a, a.public_key());
    assert_eq!(pk_b, b.public_key());

    // ---- read root: active = a -> only a's post visible ------------------
    handle.set_active_account(Some(pk_a));
    let (_qh, rows_rx) = handle
        .subscribe(reactive_kind1())
        .expect("test subscription construction");
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
    let receipt_as_b = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned_as_b)),
            durability: Durability::AtMostOnce,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(
        wait_for_status(&receipt_as_b, Duration::from_secs(5), |s| matches!(
            s,
            WriteStatus::Signed(_)
        )),
        "publish after switching active to b must sign successfully with b's OWN key -- \
         the write half of the coupled switch"
    );

    // ---- write: still active = b, a body composed "for a" -> still B -----
    // A builder structurally cannot carry an author (#1005), so a body
    // composed while A was active is not "A's draft" -- there is no field in
    // it that says A. Publishing it while B is active is a write AS B, and
    // the only signer that ever sees it is B's.
    let unsigned_as_a_while_b_active = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "composed for a while b is active",
    );
    let receipt_wrong = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned_as_a_while_b_active)),
            durability: Durability::AtMostOnce,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(
        wait_for_status(&receipt_wrong, Duration::from_secs(5), |s| matches!(
            s,
            WriteStatus::Signed(_)
        )),
        "an author-free body published while B is active signs as B, whatever it was \
         composed alongside"
    );

    // ---- switch back: read identity changes; author-pinned signing stays --
    handle.set_active_account(Some(pk_a));
    let unsigned_as_a = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "published after switching back to a",
    );
    let receipt_as_a = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned_as_a)),
            durability: Durability::AtMostOnce,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(
        wait_for_status(&receipt_as_a, Duration::from_secs(5), |s| matches!(
            s,
            WriteStatus::Signed(_)
        )),
        "A-authored work continues to use A's signer after another read-root change"
    );

    handle.shutdown();
    engine_thread.join();
}

/// A registered signer is not authority to publish when no account is
/// active. Default unsigned publish fails before acceptance.
#[test]
fn no_active_account_cannot_select_an_arbitrary_registered_signer() {
    let a = Keys::generate();
    let store = MemoryStore::new();
    let dir = FixtureRoutingFacts::new();
    let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
        store,
        dir,
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    // Register a signer but NEVER activate it.
    handle
        .add_signer(local_signer(&a))
        .expect("local signer has a public key");

    let unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "nobody is active",
    );
    let receipt_rx = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&unsigned)),
            durability: Durability::AtMostOnce,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");

    assert!(wait_for_status(&receipt_rx, Duration::from_secs(5), |s| {
        matches!(s, WriteStatus::Failed(_))
    }));

    handle.shutdown();
    engine_thread.join();
}

/// A cold directory is a reason to WAIT, never a reason to destroy a durable
/// obligation (#975, `docs/internals/routing/resolution-lifecycle.md` §8).
///
/// This test used to assert the opposite under the name
/// `active_a_rejects_b_authored_default_even_when_b_is_registered`. Two
/// changes hollowed that claim out: a builder structurally cannot carry an
/// author (#1005), so there was nothing "b-authored" left to reject, and the
/// only `Failed` it was actually observing came from `AuthorOutbox` erroring
/// on an empty `FixtureRoutingFacts` — the exact defect this issue fixes. What
/// the fixture really pins is restated here as the property that replaced it.
#[test]
fn an_auto_write_on_a_cold_directory_parks_instead_of_failing() {
    let a = Keys::generate();
    let b = Keys::generate();
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    handle
        .add_signer(local_signer(&a))
        .expect("local signer has a public key");
    handle
        .add_signer(local_signer(&b))
        .expect("local signer has a public key");
    handle.set_active_account(Some(a.public_key()));

    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("unauthorized default author").into(),
                created_at: Some(Timestamp::now()),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(
        wait_for_status(&receipt, Duration::from_secs(5), |status| {
            matches!(status, WriteStatus::AwaitingRoute { .. })
        }),
        "an Auto write with no relay list known yet parks, naming what it waits for"
    );
    assert!(
        !wait_for_status(&receipt, Duration::from_millis(500), |status| {
            matches!(status, WriteStatus::Failed(_))
        }),
        "the park is not a terminal failure -- the event is signed, journaled and durable, \
         and only the directory was young"
    );

    handle.shutdown();
    engine_thread.join();
}

/// #156's account-switch falsifier at the capability seam, restated for a
/// payload that carries no author. A builder composed while A was active is
/// not "A's draft" -- there is no field in it that says A -- so publishing
/// it after switching to B is not a stale write, it is a write as B. The
/// resolution happens at acceptance, and exactly one signer sees it: B's.
/// (The account-switch bug #47 exists to prevent is about writes ALREADY
/// accepted; acceptance pins the resolved key, which the override tests
/// below still cover.)
#[test]
fn a_builder_composed_before_a_switch_publishes_as_the_account_active_at_acceptance() {
    let a = Keys::generate();
    let b = Keys::generate();
    let a_calls = Arc::new(AtomicUsize::new(0));
    let b_calls = Arc::new(AtomicUsize::new(0));
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    handle
        .add_signer(CountingSigner {
            pubkey: a.public_key(),
            calls: Arc::clone(&a_calls),
        })
        .expect("A signer must register");
    handle
        .add_signer(CountingSigner {
            pubkey: b.public_key(),
            calls: Arc::clone(&b_calls),
        })
        .expect("B signer must register");

    handle.set_active_account(Some(a.public_key()));
    let composed_while_a_was_active = EventBuilder::new(Kind::Custom(9))
        .content("composed while A is active")
        .created_at(Timestamp::now());
    handle.set_active_account(Some(b.public_key()));

    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(composed_while_a_was_active),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("the engine is open");
    assert!(matches!(
        receipt
            .recv_timeout(Duration::from_secs(5))
            .expect("acceptance must be deterministic"),
        WriteStatus::Accepted
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && b_calls.load(Ordering::SeqCst) == 0 {
        let _ = receipt.recv_timeout(Duration::from_millis(100));
    }
    assert_eq!(
        b_calls.load(Ordering::SeqCst),
        1,
        "the account active AT ACCEPTANCE is the one asked to sign"
    );
    assert_eq!(
        a_calls.load(Ordering::SeqCst),
        0,
        "the account that merely happened to be active at compose time signs nothing"
    );

    handle.shutdown();
    engine_thread.join();
}

#[test]
fn attaching_matching_signer_rearms_awaiting_intent() {
    let a = Keys::generate();
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    // Pin the active identity before its capability exists.
    handle.set_active_account(Some(a.public_key()));
    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("reattach me").into(),
                created_at: Some(Timestamp::now()),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(wait_for_status(
        &receipt,
        Duration::from_secs(5),
        |status| {
            matches!(
                status,
                WriteStatus::AwaitingCapability { pubkey } if *pubkey == a.public_key()
            )
        }
    ));

    handle
        .add_signer(local_signer(&a))
        .expect("local signer has a public key");
    assert!(
        wait_for_status(&receipt, Duration::from_secs(5), |status| {
            matches!(status, WriteStatus::Signed(_))
        }),
        "attaching the matching signer must re-arm the durable accepted template"
    );

    handle.shutdown();
    engine_thread.join();
}

#[test]
fn accepted_b_intent_stays_pinned_after_switch_to_a_and_b_attach() {
    let a = Keys::generate();
    let b = Keys::generate();
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    handle
        .add_signer(local_signer(&a))
        .expect("local signer has a public key");
    handle.set_active_account(Some(b.public_key()));

    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("authored by b").into(),
                created_at: Some(Timestamp::now()),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(wait_for_status(
        &receipt,
        Duration::from_secs(5),
        |status| {
            matches!(
                status,
                WriteStatus::AwaitingCapability { pubkey } if *pubkey == b.public_key()
            )
        }
    ));

    handle.set_active_account(Some(a.public_key()));
    handle
        .add_signer(local_signer(&b))
        .expect("local signer has a public key");
    assert!(
        wait_for_status(&receipt, Duration::from_secs(5), |status| {
            matches!(status, WriteStatus::Signed(_))
        }),
        "the intent accepted while B was active must stay pinned to B after switching to A"
    );

    handle.shutdown();
    engine_thread.join();
}

#[test]
fn stale_registration_cannot_detach_replacement_for_same_pubkey() {
    let keys = Keys::generate();
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    // Exact replacement-race order: install A, install B for the same key,
    // detach stale A, then prove B still signs accepted work.
    let registration_a = handle
        .add_signer(local_signer(&keys))
        .expect("local signer A has a public key");
    let registration_b = handle
        .add_signer(local_signer(&keys))
        .expect("local signer B has a public key");
    assert_eq!(registration_a.public_key(), registration_b.public_key());
    assert!(
        !handle.remove_signer(registration_a),
        "stale registration A must not detach replacement B"
    );

    handle.set_active_account(Some(keys.public_key()));
    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("replacement remains usable").into(),
                created_at: Some(Timestamp::now()),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(wait_for_status(
        &receipt,
        Duration::from_secs(5),
        |status| matches!(status, WriteStatus::Signed(_))
    ));

    assert!(handle.remove_signer(registration_b.clone()));
    assert!(
        !handle.remove_signer(registration_b),
        "detaching one registration must be idempotent"
    );
    handle.shutdown();
    engine_thread.join();
}

// ---- explicit per-write identity (#47) ------------------------------------

/// Drains `rx` for `window`, panicking if any status matches `forbidden`.
/// The #47 no-retarget falsifiers need a bounded NEGATIVE observation: after
/// a read-root change, a pinned parked intent must emit no progress at all.
fn assert_no_status_within(
    rx: &FifoReceiver<WriteStatus>,
    window: Duration,
    forbidden: impl Fn(&WriteStatus) -> bool,
) {
    let deadline = Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match rx.recv_timeout(remaining) {
            Ok(status) if forbidden(&status) => {
                panic!("forbidden status arrived within the window: {status:?}")
            }
            Ok(_) => {}
            Err(FifoRecvTimeoutError::Timeout | FifoRecvTimeoutError::Closed) => return,
            Err(FifoRecvTimeoutError::Lagged) => {
                panic!("fixture receipt stream must not lag")
            }
        }
    }
}

/// #47 falsifier (a) at the registry seam: with A active and B merely
/// REGISTERED, a builder carrying `Identity::Explicit(B)`
/// signs with B's own key -- `Signed` carries the exact id of the frozen
/// B-authored body, which commits to both author and content -- and the
/// stored row's promoted event verifies cryptographically. A default
/// publish immediately after still signs as A, so naming B moved
/// nothing but its own write.
#[test]
fn an_explicit_identity_signs_as_a_registered_secondary_without_rerooting_active() {
    let a = Keys::generate();
    let b = Keys::generate();
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    handle
        .add_signer(local_signer(&a))
        .expect("local signer has a public key");
    handle
        .add_signer(local_signer(&b))
        .expect("local signer has a public key");
    handle.set_active_account(Some(a.public_key()));

    let draft = UnsignedEvent::new(
        b.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "published as b while a stays active",
    );
    // The frozen body's id commits to author+content; deriving it locally
    // with B's key is the cryptographic pin the Signed status must match.
    let expected = draft.clone().sign_with_keys(&b).expect("derive frozen id");
    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&draft)),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(b.public_key()),
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(
        wait_for_status(&receipt, Duration::from_secs(5), |status| {
            matches!(status, WriteStatus::Signed(id) if *id == expected.id)
        }),
        "the override write must sign as B with the exact frozen body/id"
    );

    // The promoted row carries B's REAL signature -- fetch it and verify.
    let (_qh, rows_rx) = handle
        .subscribe(LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([b.public_key().to_hex()]))),
            ..Filter::default()
        }))
        .expect("test subscription construction");
    let deadline = Instant::now() + Duration::from_secs(5);
    let verified = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break false;
        }
        match rows_rx.recv_timeout(remaining) {
            Ok((deltas, _coverage, _execution)) => {
                if deltas.iter().any(|delta| {
                    matches!(
                        delta,
                        RowDelta::Added(row) if row.event.id == expected.id
                            && row.event.pubkey == b.public_key()
                            && row.event.verify().is_ok()
                    )
                }) {
                    break true;
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break false,
        }
    };
    assert!(
        verified,
        "the promoted row must be B's cryptographically valid event"
    );

    // Active identity never moved: the default (no-override) path still
    // roots on A and signs with A's key.
    let a_draft = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "default path still signs as a",
    );
    let receipt_default = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&a_draft)),
            durability: Durability::AtMostOnce,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(wait_for_status(
        &receipt_default,
        Duration::from_secs(5),
        |status| matches!(status, WriteStatus::Signed(_))
    ));

    handle.shutdown();
    engine_thread.join();
}

/// #47 falsifier (d): an override naming a pubkey with NO registered
/// capability is a durable park (`Accepted` then `AwaitingCapability`),
/// never a silent failure. A later `set_active_account` to a DIFFERENT
/// registered identity must not retarget the parked intent -- only
/// registering the override key itself resumes it, and it signs as the
/// ORIGINAL override pubkey with the exact frozen body.
#[test]
fn unregistered_override_parks_durably_and_never_retargets_on_account_switch() {
    let a = Keys::generate();
    let b = Keys::generate();
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    // Only A's capability exists; B is the override target with none.
    handle
        .add_signer(local_signer(&a))
        .expect("local signer has a public key");
    handle.set_active_account(Some(a.public_key()));

    let draft = UnsignedEvent::new(
        b.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "parked until b's capability exists",
    );
    let expected = draft.clone().sign_with_keys(&b).expect("derive frozen id");
    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&draft)),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(b.public_key()),
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(
        wait_for_status(&receipt, Duration::from_secs(5), |status| {
            matches!(
                status,
                WriteStatus::AwaitingCapability { pubkey } if *pubkey == b.public_key()
            )
        }),
        "an override with no registered capability must park, not fail, and the parked pubkey \
         must be the frozen override B -- never the active account A"
    );

    // Re-rooting the ACTIVE account onto A (whose signer is attached and
    // eager) must not retarget the parked B-pinned intent: no Signed, no
    // Failed -- silence.
    handle.set_active_account(Some(a.public_key()));
    assert_no_status_within(&receipt, Duration::from_millis(500), |status| {
        matches!(status, WriteStatus::Signed(_) | WriteStatus::Failed(_))
    });

    // Registering the override key resumes the SAME intent as B.
    handle
        .add_signer(local_signer(&b))
        .expect("local signer has a public key");
    assert!(
        wait_for_status(&receipt, Duration::from_secs(5), |status| {
            matches!(status, WriteStatus::Signed(id) if *id == expected.id)
        }),
        "attaching the override key's signer must complete the original write as B"
    );

    handle.shutdown();
    engine_thread.join();
}

/// #47 falsifier (e): an explicit identity needs NO active account at all --
/// logged fully out, a builder with `Identity::Explicit(B)`
/// and B's registered capability still signs. (Contrast with
/// [`no_active_account_cannot_select_an_arbitrary_registered_signer`]: the
/// DEFAULT path in the same state fails closed.)
#[test]
fn an_explicit_identity_signs_while_logged_out() {
    let b = Keys::generate();
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    handle
        .add_signer(local_signer(&b))
        .expect("local signer has a public key");
    handle.set_active_account(None);

    let draft = UnsignedEvent::new(
        b.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "logged out, explicit consent",
    );
    let expected = draft.clone().sign_with_keys(&b).expect("derive frozen id");
    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(body_of(&draft)),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(b.public_key()),
            correlation: None,
        })
        .expect("receipt id allocation");
    assert!(
        wait_for_status(&receipt, Duration::from_secs(5), |status| {
            matches!(status, WriteStatus::Signed(id) if *id == expected.id)
        }),
        "an explicit override must not require an active account"
    );

    handle.shutdown();
    engine_thread.join();
}

#[test]
fn pubkeyless_capability_is_a_typed_registration_error() {
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    assert_eq!(
        handle.add_signer(PubkeylessSigner),
        Err(nmp::mechanism::runtime::AddSignerError::MissingPublicKey)
    );
    handle.shutdown();
    engine_thread.join();
}

/// The same body these fixtures already build, said the way an app says it:
/// a builder states the kind, the tags, the content and (here, so the
/// assertions can name exact ids) the timestamp. The author is not part of
/// it -- the write's identity decides that at acceptance.
fn body_of(unsigned: &nostr::UnsignedEvent) -> nmp_grammar::EventBuilder {
    nmp_grammar::EventBuilder {
        kind: unsigned.kind,
        tags: unsigned.tags.iter().cloned().collect(),
        content: unsigned.content.clone(),
        created_at: Some(unsigned.created_at),
    }
}
