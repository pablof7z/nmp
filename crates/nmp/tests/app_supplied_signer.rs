//! #1238 — a signing capability the APP owns, reached through an engine-owned
//! pull mailbox.
//!
//! The gap this closes was total on Swift and Kotlin: `Engine::add_signer` is
//! generic over a Rust trait whose `sign` returns a poll-thunk, so nothing
//! about it can cross UniFFI, and no other signer door existed. An app on
//! those platforms could register no signer at all — the only identity NMP
//! could hold for it was a local secret key handed to `add_account`, which is
//! why the one real Swift consumer shipped a plaintext `nsec` on disk.
//!
//! These are the direct-Rust proofs of the door's behaviour. The FFI tier's
//! own proofs live in `crates/nmp-ffi`, and the cross-tier oracle in
//! `crates/nmp-parity`.
//!
//! What is deliberately NOT asserted here: that NMP never calls app code.
//! That is not a runtime property to test, it is the shape of the API —
//! `add_signer_mailbox` accepts no callable of any kind, so there is nothing
//! for NMP to invoke (#783's falsifier 1, held by construction).

use std::time::Duration;

use nmp::{Engine, EngineConfig, SignEventRequest, SignerError, SignerOp, SigningCapability};
use nmp_local_signer::LocalKeySigner;
use nostr::{Keys, Kind, Timestamp};

/// Drain one request from `mailbox` and answer it with a real signature made
/// from `keys` — the app half of the loop, exactly as a Swift app would do it
/// on its own executor.
async fn sign_one_from_the_mailbox(mailbox: &nmp::SignerMailbox, keys: &Keys) {
    let request = mailbox
        .next()
        .await
        .expect("the mailbox is open")
        .expect("the engine asked this signer for something");

    let signer = LocalKeySigner::from_secret_bytes(keys.secret_key().as_secret_bytes())
        .expect("fixture keys are a valid secp256k1 scalar");
    let signed = match signer.sign(request.unsigned_event().clone()) {
        SignerOp::Ready(result) => result.expect("a local key always signs"),
        SignerOp::Pending(pending) => pending.recv().expect("a local key always signs"),
    };
    request
        .resolve(signed)
        .expect("the engine is still waiting");
}

/// The headline. A key whose signer lives entirely in the app produces a real
/// signature through the engine's ordinary sign path — no local secret, no
/// `add_account`, nothing NMP holds but a public key and a mailbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_supplied_signer_signs_through_its_mailbox() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();

    let (_registration, mailbox) = engine
        .add_signer_mailbox(keys.public_key())
        .expect("a public key is all this door needs");
    engine
        .set_active_account(Some(keys.public_key()))
        .expect("the key may be made active like any other");

    let operation = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(5),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "signed by the app".to_string(),
        })
        .expect("the operation is admitted");

    // The engine is now blocked on this signer. Answer it from the app side.
    sign_one_from_the_mailbox(&mailbox, &keys).await;

    let signed = tokio::task::spawn_blocking(move || operation.recv())
        .await
        .expect("join")
        .expect("the app's signature must satisfy the engine");

    assert_eq!(signed.pubkey, keys.public_key());
    assert_eq!(signed.content, "signed by the app");
    assert!(
        signed.verify().is_ok(),
        "the engine must only surface a signature it verified"
    );
    engine.shutdown();
}

/// A refusal from the app is the person saying no, and it reaches the caller
/// as the signer's own terminal rejection rather than as a timeout or a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_refusal_reaches_the_caller_as_a_rejection() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();

    let (_registration, mailbox) = engine.add_signer_mailbox(keys.public_key()).unwrap();
    engine.set_active_account(Some(keys.public_key())).unwrap();

    let operation = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(5),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "the user will decline this".to_string(),
        })
        .expect("the operation is admitted");

    let request = mailbox.next().await.unwrap().unwrap();
    request
        .reject(SignerError::Rejected("user declined".to_string()))
        .expect("the engine is still waiting");

    let outcome = tokio::task::spawn_blocking(move || operation.recv())
        .await
        .expect("join");
    assert!(
        matches!(outcome, Err(nmp::SignEventError::SignerRejected { .. })),
        "an app's refusal must arrive as a rejection, got {outcome:?}"
    );
    engine.shutdown();
}

/// The mailbox is a real member of the one signer registry: the exact-instance
/// registration proof removes it, and a stale proof cannot detach the
/// replacement registered for the same key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_registration_cannot_detach_a_replacement_mailbox() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();

    let (first, _first_mailbox) = engine.add_signer_mailbox(keys.public_key()).unwrap();
    let (second, second_mailbox) = engine.add_signer_mailbox(keys.public_key()).unwrap();

    assert!(
        !engine.remove_signer(first).unwrap(),
        "the superseded registration must not detach its replacement"
    );

    // The replacement is still the live signer for that key.
    engine.set_active_account(Some(keys.public_key())).unwrap();
    let operation = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(5),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "still registered".to_string(),
        })
        .expect("the operation is admitted");
    sign_one_from_the_mailbox(&second_mailbox, &keys).await;
    tokio::task::spawn_blocking(move || operation.recv())
        .await
        .expect("join")
        .expect("the replacement mailbox is the live signer");

    assert!(
        engine.remove_signer(second).unwrap(),
        "the exact registration detaches it"
    );
    engine.shutdown();
}

/// A registered mailbox nobody ever reads must not wedge the engine. The
/// operation resolves — as the ordinary retryable unavailable — rather than
/// blocking forever on app code that is never going to run (#783 falsifier 5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mailbox_the_app_never_reads_does_not_wedge_the_engine() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();

    let (_registration, mailbox) = engine.add_signer_mailbox(keys.public_key()).unwrap();
    engine.set_active_account(Some(keys.public_key())).unwrap();
    // The app registered a signer and then went away without ever draining.
    drop(mailbox);

    let operation = engine
        .sign_event(SignEventRequest {
            created_at: Timestamp::from(5),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "nobody is listening".to_string(),
        })
        .expect("the operation is still admitted");

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || operation.recv()),
    )
    .await
    .expect("an unread mailbox must not block the engine indefinitely")
    .expect("join");

    assert!(
        matches!(outcome, Err(nmp::SignEventError::SignerUnavailable { .. })),
        "an abandoned mailbox reports unavailable, got {outcome:?}"
    );
    engine.shutdown();
}
