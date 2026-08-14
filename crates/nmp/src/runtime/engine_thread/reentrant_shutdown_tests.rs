use super::*;
use nmp_local_signer::LocalKeySigner;
use nmp_store::RedbStore;
use nostr::{Keys, Kind};
use std::time::Instant;

/// #765: `LocalKeySigner` now owns its scalar in one canonical zeroizing
/// allocation and no longer accepts a `nostr::Keys`. These fixtures still
/// build identities as `Keys`, so hand the raw scalar across exactly here.
fn local_signer(keys: &Keys) -> LocalKeySigner {
    LocalKeySigner::from_secret_bytes(keys.secret_key().as_secret_bytes())
        .expect("fixture keys are valid secp256k1 scalars")
}

fn runtime() -> (EngineThread, Handle) {
    // #680 removed the configurable native-task limit; the blocking-adapter
    // pool is a fixed internal capacity, so spawn takes no limit argument.
    EngineThread::spawn(
        RedbStore::temporary().expect("temporary Redb store"),
        1,
        PoolConfig::default(),
    )
    .expect("engine construction")
}

fn unsigned(keys: &Keys, content: &str) -> UnsignedEvent {
    UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(1),
        Kind::TextNote,
        Vec::new(),
        content.to_string(),
    )
}

#[test]
fn external_shutdown_first_then_callback_owned_join_exempts_exact_origin() {
    let (engine, handle) = runtime();
    let keys = Keys::generate();
    handle
        .add_signer(local_signer(&keys))
        .expect("signer registration");
    handle.set_current_account(Some(keys.public_key()));

    let (entered_tx, entered_rx) = mpsc::channel();
    let (join_tx, join_rx) = mpsc::channel();
    let (returned_tx, returned_rx) = mpsc::channel();
    handle
        .sign_event_with_completion(unsigned(&keys, "reentrant shutdown"), move |result| {
            assert!(result.is_ok(), "local signer must complete: {result:?}");
            let _ = entered_tx.send(());
            let _ = join_rx.recv();
            engine.join();
            let _ = returned_tx.send(());
        })
        .expect("sign-event admission");

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("completion callback must start");
    handle.shutdown();
    assert_eq!(
        handle.add_signer(LocalKeySigner::generate()),
        Err(AddSignerError::EngineShuttingDown),
        "the external shutdown must enter its drain before callback-owned join"
    );
    join_tx.send(()).expect("allow callback-owned join");

    returned_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("callback-owned join must exempt itself after shutdown already began");
}

#[test]
fn callback_handle_shutdown_does_not_weaken_external_join_drain() {
    let (engine, handle) = runtime();
    let keys = Keys::generate();
    handle
        .add_signer(local_signer(&keys))
        .expect("signer registration");
    handle.set_current_account(Some(keys.public_key()));

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let callback_handle = handle.clone();
    handle
        .sign_event_with_completion(unsigned(&keys, "external shutdown"), move |result| {
            assert!(result.is_ok(), "local signer must complete: {result:?}");
            callback_handle.shutdown();
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        })
        .expect("sign-event admission");
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("completion callback must start");
    assert_eq!(
        handle.add_signer(LocalKeySigner::generate()),
        Err(AddSignerError::EngineShuttingDown),
        "callback shutdown must enter its drain before external join"
    );

    let (returned_tx, returned_rx) = mpsc::channel();
    let shutdown = thread::spawn(move || {
        engine.join();
        let _ = returned_tx.send(());
    });
    assert!(
        returned_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "a callback Handle::shutdown cannot exempt an externally-owned join"
    );
    release_tx.send(()).expect("release callback");
    returned_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("external shutdown must return after callback completion");
    shutdown.join().expect("shutdown thread");
}

#[test]
fn panicking_callback_still_finishes_external_shutdown_drain() {
    let (engine, handle) = runtime();
    let keys = Keys::generate();
    handle
        .add_signer(local_signer(&keys))
        .expect("signer registration");
    handle.set_current_account(Some(keys.public_key()));

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    handle
        .sign_event_with_completion(unsigned(&keys, "panicking callback"), move |result| {
            assert!(result.is_ok(), "local signer must complete: {result:?}");
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            panic!("injected completion panic");
        })
        .expect("sign-event admission");
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("completion callback must start");

    handle.shutdown();
    assert_eq!(
        handle.add_signer(LocalKeySigner::generate()),
        Err(AddSignerError::EngineShuttingDown),
        "external shutdown must enter its drain before the callback panics"
    );
    let (returned_tx, returned_rx) = mpsc::channel();
    let shutdown = thread::spawn(move || {
        engine.join();
        let _ = returned_tx.send(());
    });
    assert!(
        returned_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "external join must retain the callback until its panic unwinds"
    );
    release_tx.send(()).expect("release callback to panic");
    returned_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("panic-safe Finished guard must release the shutdown drain");
    shutdown.join().expect("shutdown thread");
}

#[test]
fn callback_owned_join_exempts_only_itself_and_drains_another_callback() {
    let (engine, handle) = runtime();
    let keys = Keys::generate();
    handle
        .add_signer(local_signer(&keys))
        .expect("signer registration");
    handle.set_current_account(Some(keys.public_key()));

    let (other_entered_tx, other_entered_rx) = mpsc::channel();
    let (release_other_tx, release_other_rx) = mpsc::channel();
    handle
        .sign_event_with_completion(unsigned(&keys, "other callback"), move |result| {
            assert!(result.is_ok(), "local signer must complete: {result:?}");
            let _ = other_entered_tx.send(());
            let _ = release_other_rx.recv();
        })
        .expect("other sign-event admission");
    other_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("other callback must start");

    let callback_handle = handle.clone();
    let (joining_entered_tx, joining_entered_rx) = mpsc::channel();
    let (returned_tx, returned_rx) = mpsc::channel();
    handle
        .sign_event_with_completion(unsigned(&keys, "joining callback"), move |result| {
            assert!(result.is_ok(), "local signer must complete: {result:?}");
            callback_handle.shutdown();
            let _ = joining_entered_tx.send(());
            engine.join();
            let _ = returned_tx.send(());
        })
        .expect("joining sign-event admission");

    joining_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("joining callback must start");
    assert!(
        returned_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "exact-origin exemption must retain every other callback in the drain"
    );
    release_other_tx.send(()).expect("release other callback");
    returned_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("callback-owned join must return after the other callback completes");
}

#[test]
fn repeated_engine_shutdown_returns_runtime_threads_to_exact_baseline() {
    let _serial = RUNTIME_LIFECYCLE_TEST_LOCK.lock().unwrap();
    for _ in 0..16 {
        let (engine, handle) = EngineThread::spawn(
            RedbStore::temporary().expect("temporary Redb store"),
            1,
            PoolConfig::default(),
        )
        .expect("engine construction");
        let runtime_threads = Arc::clone(&engine.runtime_threads);
        let deadline = Instant::now() + Duration::from_secs(5);
        // #704: the auth-release bridge is gone (the adapter executor was
        // replaced by the tokio runtime, whose workers are NOT counted by
        // this reducer/bridge guard). One engine now owns exactly the
        // reducer thread + the pool-bridge thread.
        while runtime_threads.load(std::sync::atomic::Ordering::SeqCst) != 2
            && Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert_eq!(
            runtime_threads.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one engine must own exactly its reducer and pool-bridge threads"
        );
        handle.shutdown();
        engine.join();
        assert_eq!(
            runtime_threads.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "join must be an exact engine/bridge teardown barrier"
        );
    }
}
