//! [`Engine`] -- the one supported construction call plus the two nouns
//! (canonical-facade-52-plan.md §1). Owns config -> store/directory
//! selection and the router cap both `nmp-ffi` and `nmp-demo` used to
//! duplicate by hand.
//!
//! No `Signed`-payload verify lives here: that guarantee moved to
//! `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary (Unit
//! A0, #56) precisely so it holds for every entry point -- this facade,
//! `nmp-ffi`, and any `from_parts`/raw-`EngineThread` caller alike -- not
//! only the one that happens to verify locally. See [`crate::error`]'s doc.
//!
//! ## The serialized lifecycle gate
//!
//! `inner` holds `Some(Inner)` while the engine is open, `None` once
//! [`Engine::shutdown`] has run. Every verb takes the SAME mutex, checks
//! that state, and either runs its `Handle` call while still holding the
//! lock or returns [`EngineError::EngineClosed`] immediately -- it never
//! reaches a raw `Handle` call that could race the engine thread's own exit
//! and panic through `Handle`'s internal `.expect(...)`s. `shutdown` takes
//! the same lock to `Option::take` it, so a verb call and a `shutdown` call
//! can never interleave: one strictly precedes the other. `Engine`'s `Drop`
//! calls `shutdown` too, so a dropped-without-`shutdown` `Engine` still
//! tears down `EngineThread` cleanly rather than detaching it.

use std::sync::mpsc::Receiver;
use std::sync::Mutex;

use nmp_engine::core::ReceiptId;
use nmp_engine::outbox::WriteStatus;
use nmp_engine::runtime::{EngineThread, Handle, ReceiptReattachment, ReceiptStream};
use nmp_grammar::WriteIntent;
use nmp_resolver::LiveQuery;
use nmp_store::{MemoryStore, RedbStore};
use nmp_transport::PoolConfig;
use nostr::{Keys, PublicKey};

use crate::config::{build_admission_policy, build_directory, EngineConfig};
use crate::error::EngineError;
use crate::subscription::{DiagnosticsSubscription, Subscription};

/// The router compiler's per-tick atom-count cap. Both `nmp-ffi` and
/// `nmp-demo` hardcoded their own copy of this constant before #52; the
/// facade now owns the one value.
const ROUTER_CAP: usize = 10;

/// The open state: the `Handle` verbs are driven through, plus the
/// `EngineThread` `shutdown` eventually joins. Not `Clone` (`EngineThread`
/// isn't), so it lives behind `Engine`'s own mutex rather than a
/// `Mutex<Option<EngineThread>>` alongside a separately-held `Handle`.
struct Inner {
    handle: Handle,
    engine_thread: EngineThread,
}

/// The one supported Rust product surface (canonical-facade-52-plan.md §1).
/// Owns the `EngineThread` + `Handle` pair; every mechanism crate
/// (`nmp-store`/`nmp-router`/`nmp-transport`/`nmp-resolver`) is reached only
/// through here. See this module's doc for the serialized lifecycle gate
/// `inner` implements.
pub struct Engine {
    inner: Mutex<Option<Inner>>,
}

impl Engine {
    /// The ONE construction call: config -> store/directory selection,
    /// router cap, everything `nmp-ffi::facade::build_directory` and
    /// `nmp-demo`'s hand-rolled assembly used to duplicate independently.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        let directory = build_directory(&config)?;
        let admission = build_admission_policy(&config);
        // Issue #121: the operator's relay-count ceiling rides the transport
        // pool (the worker-exhaustion backstop); `0` leaves it uncapped.
        let pool_config = PoolConfig {
            max_relays: config.max_relays,
            ..PoolConfig::default()
        };

        let (engine_thread, handle) = match &config.store_path {
            Some(path) => {
                let store = RedbStore::open(path).map_err(|e| EngineError::StoreOpenFailed {
                    reason: e.to_string(),
                })?;
                EngineThread::spawn(store, directory, ROUTER_CAP, pool_config, admission)
            }
            None => {
                let store = MemoryStore::new();
                EngineThread::spawn(store, directory, ROUTER_CAP, pool_config, admission)
            }
        };

        Ok(Self {
            inner: Mutex::new(Some(Inner {
                handle,
                engine_thread,
            })),
        })
    }

    /// #52 Q3's unstable escape hatch: construct directly from an
    /// already-built store/directory pair, bypassing `EngineConfig`
    /// entirely. `#[doc(hidden)]` and gated behind the `unstable-mechanism`
    /// feature -- the ONLY sanctioned way to inject a store (needed by
    /// `nmp-bdd`, which spawns the real `EngineThread` against scripted
    /// in-process relays). This is an in-workspace/test hatch, not an
    /// alternative app contract: it may freely require mechanism-crate
    /// types in its own signature (it is not expected to be reachable from
    /// an `nmp`-only dependency the way the default surface is). It is a
    /// stability exception only, not a security one -- an engine built this
    /// way still verifies every `Signed` payload at the acceptance boundary
    /// (Unit A0), same as every other entry point.
    #[cfg(feature = "unstable-mechanism")]
    #[doc(hidden)]
    pub fn from_parts<S, D>(
        store: S,
        directory: D,
        cap: usize,
        pool_config: PoolConfig,
        admission: nmp_engine::core::RelayAdmissionPolicy,
    ) -> Self
    where
        S: nmp_store::EventStore + Send + 'static,
        D: nmp_router::RelayDirectory + Send + 'static,
    {
        let (engine_thread, handle) =
            EngineThread::spawn(store, directory, cap, pool_config, admission);
        Self {
            inner: Mutex::new(Some(Inner {
                handle,
                engine_thread,
            })),
        }
    }

    /// Run `f` against the live `Handle` while holding `inner`'s lock for
    /// the duration of the call -- see this module's doc for why that,
    /// rather than cloning the `Handle` and releasing the lock first, is
    /// what actually closes the post-`shutdown` race.
    fn with_handle<F, T>(&self, f: F) -> Result<T, EngineError>
    where
        F: FnOnce(&Handle) -> T,
    {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match &*guard {
            Some(inner) => Ok(f(&inner.handle)),
            None => Err(EngineError::EngineClosed),
        }
    }

    /// Noun 1: open a live query. The returned [`Subscription`] withdraws
    /// itself on `Drop` (see that type's doc).
    pub fn observe(&self, query: LiveQuery) -> Result<Subscription, EngineError> {
        self.with_handle(|handle| {
            let (query_handle, rows) = handle.subscribe(query);
            Subscription::new(handle.clone(), query_handle, rows)
        })
    }

    /// Noun 2: enqueue a write -- the call itself never blocks on routing/
    /// wire/ack, but its return value is not fire-and-forget: the
    /// `Receiver` is the caller's one way to observe how the intent
    /// resolved, and every `WriteStatus` it ever reaches streams through it
    /// (ledger #9 -- enqueue is not converged). A tampered
    /// `WritePayload::Signed` is rejected at the engine's acceptance
    /// boundary and surfaces here as a `WriteStatus::Failed` with no
    /// preceding `Accepted` -- see this module's doc.
    /// Exhaustion of the disjoint pre-acceptance correlation namespace is a
    /// synchronous [`EngineError::ReceiptCorrelationIdExhausted`], because
    /// no truthful receipt stream can exist without an id.
    pub fn publish(&self, intent: WriteIntent) -> Result<Receiver<WriteStatus>, EngineError> {
        self.with_handle(|handle| handle.publish(intent))?
            .map_err(EngineError::from_publish_error)
    }

    /// Enqueue a write while retaining the stable store-issued receipt id
    /// needed for process-later reattachment. Pre-acceptance correlation-id
    /// exhaustion returns a typed error without creating a receipt.
    pub fn publish_tracked(&self, intent: WriteIntent) -> Result<ReceiptStream, EngineError> {
        self.with_handle(|handle| handle.publish_tracked(intent))?
            .map_err(EngineError::from_publish_error)
    }

    /// Reattach to durable receipt facts after a restart. Missing ids and
    /// retained obligations with unreadable evidence are distinct outcomes.
    pub fn reattach_receipt(&self, id: ReceiptId) -> Result<ReceiptReattachment, EngineError> {
        self.with_handle(|handle| handle.reattach_receipt(id))
    }

    /// Register an account from its secret key (hex or bech32 `nsec`). Does
    /// NOT make the account active -- call [`Self::set_active_account`] for
    /// that. Returns the account's public key.
    ///
    /// This builds a `LocalKeySigner` internally, whose `public_key()`
    /// always reports `Some` -- there is no reachable "signer has no
    /// public key" state on this path (unlike an arbitrary third-party
    /// `SigningCapability`, which the `unstable-mechanism`-gated
    /// `add_signer` covers instead).
    pub fn add_account(&self, secret_key: &str) -> Result<PublicKey, EngineError> {
        let keys = Keys::parse(secret_key).map_err(|_| EngineError::InvalidSecretKey)?;
        self.with_handle(|handle| {
            handle
                .add_signer(nmp_signer::LocalKeySigner::new(keys))
                .expect("LocalKeySigner::public_key() always returns Some")
        })
    }

    /// Register an arbitrary signing capability (e.g. a NIP-46/bunker
    /// remote signer) -- the lower-level verb [`Self::add_account`] sits on
    /// top of for the common local-key case. Same "does not activate it"
    /// caveat as `add_account`.
    ///
    /// Gated behind `unstable-mechanism` until #2/#3's Unit U3 lands: the
    /// runtime does not yet validate a signer's OUTPUT against the frozen
    /// unsigned template (matching body/pubkey/id) before routing it to the
    /// wire -- `EngineCore::on_signer_completed` forwards whatever
    /// `Ok(event)` a registered `SigningCapability` returns straight into
    /// `on_signed`, so a misbehaving/compromised custom signer can get a
    /// tampered event published verbatim today. `add_account`'s
    /// `LocalKeySigner` is exempt from this gate -- it signs the exact
    /// frozen template itself rather than accepting an external signer's
    /// output -- so this only concerns a THIRD-PARTY `SigningCapability`
    /// impl, and the facade must not present that path as supported before
    /// U3 closes the gap.
    #[cfg(feature = "unstable-mechanism")]
    pub fn add_signer<Sig>(&self, signer: Sig) -> Result<Option<PublicKey>, EngineError>
    where
        Sig: nmp_signer::SigningCapability + Send + 'static,
    {
        self.with_handle(|handle| handle.add_signer(signer))
    }

    /// Re-root every reactive query AND the active signing capability
    /// together onto `pubkey` (`None` -> logged-out / read-only). `pubkey`
    /// need not have been registered via [`Self::add_account`] -- read-only
    /// browsing of an account this app holds no key for is legal; any
    /// publish attempted while active in that state terminates
    /// `WriteStatus::Failed`, never a panic.
    pub fn set_active_account(&self, pubkey: Option<PublicKey>) -> Result<(), EngineError> {
        self.with_handle(|handle| handle.set_active_account(pubkey))
    }

    /// Open a live diagnostics stream. Same `Drop` discipline as
    /// [`Self::observe`] -- see [`DiagnosticsSubscription`]'s doc.
    pub fn observe_diagnostics(&self) -> Result<DiagnosticsSubscription, EngineError> {
        self.with_handle(|handle| {
            let (diag_handle, snapshots) = handle.observe_diagnostics();
            DiagnosticsSubscription::new(diag_handle, snapshots)
        })
    }

    /// Stop the engine. Idempotent: a second call (or a call racing another
    /// thread's call) finds `inner` already `None` and no-ops. Every verb
    /// above shares this same lock, so no call that starts after this one
    /// completes can ever reach the raw `Handle`/`EngineThread` again --
    /// see this module's doc.
    pub fn shutdown(&self) {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(Inner {
            handle,
            engine_thread,
        }) = inner
        {
            handle.shutdown();
            engine_thread.join();
        }
    }
}

impl Drop for Engine {
    /// A dropped-without-`shutdown` `Engine` must still tear down
    /// `EngineThread` cleanly rather than detaching its join handles while
    /// `engine_loop` keeps running with nothing left to stop it --
    /// `shutdown` is already idempotent, so `Drop` simply reuses it.
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{Durability, WritePayload, WriteRouting};
    use nostr::ToBech32;

    /// `EngineConfig::default()` (no `store_path`) must select the
    /// in-memory store and construct cleanly with no network at all -- no
    /// indexer/app/fallback relay configured.
    #[test]
    fn config_with_no_store_path_selects_memory_store() {
        let engine = Engine::new(EngineConfig::default()).expect("in-memory engine must build");
        engine.shutdown();
    }

    /// A `store_path` must select the on-disk store, opened at that exact
    /// path -- the config -> store-selection branch `nmp-ffi`/`nmp-demo`
    /// used to each hand-roll.
    #[test]
    fn config_with_store_path_selects_redb_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.redb");
        let config = EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        };
        let engine = Engine::new(config).expect("redb-backed engine must build");
        engine.shutdown();
        assert!(path.exists(), "RedbStore::open must have created the file");
    }

    /// An invalid relay URL in the config is a typed construction error, not
    /// a panic.
    #[test]
    fn config_with_invalid_relay_url_is_a_typed_error() {
        let config = EngineConfig {
            indexer_relays: vec!["not a url".to_string()],
            ..EngineConfig::default()
        };
        match Engine::new(config) {
            Err(err) => assert_eq!(
                err,
                EngineError::InvalidRelayUrl {
                    url: "not a url".to_string()
                }
            ),
            Ok(_) => panic!("a malformed relay URL must fail closed, not construct"),
        }
    }

    /// `add_account` must accept both hex and bech32 `nsec` secret keys and
    /// return the same public key either way.
    #[test]
    fn add_account_accepts_hex_and_nsec_secret_keys() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();

        let via_hex = engine
            .add_account(&keys.secret_key().to_secret_hex())
            .expect("hex secret key must parse");
        assert_eq!(via_hex, keys.public_key());

        let via_nsec = engine
            .add_account(
                &keys
                    .secret_key()
                    .to_bech32()
                    .expect("secret key must encode as bech32"),
            )
            .expect("bech32 nsec must parse");
        assert_eq!(via_nsec, keys.public_key());

        engine.shutdown();
    }

    /// A malformed secret key is a typed error, not a panic.
    #[test]
    fn add_account_rejects_malformed_secret_key() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        assert_eq!(
            engine.add_account("not-a-key"),
            Err(EngineError::InvalidSecretKey)
        );
        engine.shutdown();
    }

    /// #52's headline falsifier, exercised through the facade: a tampered
    /// `WritePayload::Signed` is rejected at `EngineCore::on_publish`'s
    /// acceptance boundary (Unit A0) regardless of entry point -- the
    /// receipt stream this facade's `publish` returns delivers `Failed` as
    /// its FIRST and ONLY status, with no preceding `Accepted` and no
    /// relay ever contacted (this test configures zero relays, so any
    /// routing attempt would hang/panic rather than silently pass).
    #[test]
    fn tampered_signed_publish_fails_closed_with_no_accepted() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        // An arbitrary caller-owned kind, not any NIP-01 core schema --
        // docs/known-gaps.md's v2-contract promotion forbids baking a
        // kind:1-first bias into the facade's own acceptance fixtures.
        let mut event = nostr::EventBuilder::new(nostr::Kind::Custom(9999), "original")
            .sign_with_keys(&keys)
            .expect("test fixture must sign cleanly");
        // Tamper the content after signing: id/sig no longer match it, but
        // the event otherwise still looks well-formed.
        event.content = "tampered".to_string();

        let rx = engine
            .publish(WriteIntent {
                payload: WritePayload::Signed(event),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            })
            .expect("engine is open");

        match rx.recv().expect("a Durable intent must yield a status") {
            WriteStatus::Failed(_) => {}
            other => panic!("expected WriteStatus::Failed, got {other:?}"),
        }
        assert!(
            rx.recv().is_err(),
            "Failed must be the sole terminal status -- no Accepted, nothing further"
        );

        engine.shutdown();
    }

    /// `shutdown` must be safe to call more than once -- a second call
    /// finds `inner` already taken and no-ops rather than panicking.
    #[test]
    fn shutdown_is_idempotent() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        engine.shutdown();
        engine.shutdown();
    }

    /// Every verb must fail closed with `EngineClosed` after `shutdown` --
    /// never panic, never silently hand back a dead-on-arrival value. This
    /// is the fix for the review finding that `observe`/`observe_diagnostics`
    /// used to panic through `Handle`'s internal `.expect(...)` once the
    /// engine thread had actually exited, and `publish` used to silently
    /// return an already-disconnected receiver with no signal that the
    /// engine was closed.
    #[test]
    fn every_verb_fails_closed_after_shutdown() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        engine.shutdown();

        assert_eq!(
            engine.observe(probe_query()).err(),
            Some(EngineError::EngineClosed)
        );
        assert_eq!(
            engine.observe_diagnostics().err(),
            Some(EngineError::EngineClosed)
        );
        assert_eq!(
            engine.set_active_account(None).err(),
            Some(EngineError::EngineClosed)
        );
        assert_eq!(
            engine.add_account(&Keys::generate().secret_key().to_secret_hex()),
            Err(EngineError::EngineClosed)
        );
        let publish_result = engine.publish(WriteIntent {
            payload: WritePayload::Unsigned(nostr::UnsignedEvent::new(
                Keys::generate().public_key(),
                nostr::Timestamp::now(),
                nostr::Kind::Custom(9999),
                Vec::new(),
                "unreachable",
            )),
            durability: Durability::Ephemeral,
            routing: WriteRouting::AuthorOutbox,
        });
        assert_eq!(publish_result.err(), Some(EngineError::EngineClosed));
    }

    /// A second, concurrent `shutdown` racing the first must still only
    /// ever see the gate flip exactly once -- both calls are safe, and
    /// after both return the engine is closed exactly as if only one had
    /// been called.
    #[test]
    fn concurrent_shutdown_calls_are_race_free() {
        use std::sync::Arc;

        let engine = Arc::new(Engine::new(EngineConfig::default()).expect("engine must build"));
        let other = Arc::clone(&engine);
        let joined = std::thread::spawn(move || other.shutdown());
        engine.shutdown();
        joined.join().expect("concurrent shutdown must not panic");

        assert_eq!(
            engine.set_active_account(None).err(),
            Some(EngineError::EngineClosed)
        );
    }

    /// Dropping an `Engine` that was never explicitly `shutdown` must not
    /// panic and must still run the same teardown path (the review's
    /// RAII-shutdown blocker: a bare `Mutex<Option<Inner>>` drop would
    /// detach `EngineThread`'s join handles while `engine_loop` kept
    /// running with `self_inbox` still open). This variant has no live
    /// observer at all; [`drop_with_live_observers_tears_down_within_bound_and_disconnects_cleanly`]
    /// below is the same claim with a query AND a diagnostics subscription
    /// still open at drop time.
    #[test]
    fn drop_without_explicit_shutdown_does_not_panic() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        drop(engine);
    }

    /// The RAII-shutdown claim, proven with LIVE handles rather than an
    /// idle engine: drop an `Engine` while a query [`Subscription`] AND a
    /// [`DiagnosticsSubscription`] are still open, and prove (a) `Drop`'s
    /// `shutdown`+`join` completes within a bounded wait rather than
    /// hanging -- the regression this whole fix guards against is
    /// detaching `EngineThread`'s join handles while `engine_loop` kept
    /// running with live subscribers still registered; (b) both channels
    /// observe a clean disconnect afterward, not a hang; (c) dropping the
    /// surviving handles once the engine is already gone does not panic --
    /// `Handle::unsubscribe`/`DiagnosticsHandle::cancel` are already
    /// fire-and-forget (`let _ = self.inbox.send(...)`), so this pins that
    /// tolerance holds end-to-end through a real `Drop`, not only in
    /// isolation.
    ///
    /// The bound in (a) is enforced by dropping `engine` on a WORKER
    /// thread and awaiting its completion signal via
    /// `Receiver::recv_timeout` on THIS thread -- not by dropping inline
    /// and checking elapsed time afterward. A synchronous inline `drop`
    /// that deadlocked inside `shutdown`+`join` would never reach an
    /// elapsed-time check at all, so that shape is not a real liveness
    /// bound (it only hangs until the outer test-runner's own timeout);
    /// `recv_timeout` is what turns a `Drop` deadlock into an ordinary
    /// assertion failure here instead.
    #[test]
    fn drop_with_live_observers_tears_down_within_bound_and_disconnects_cleanly() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");

        let subscription = engine.observe(probe_query()).expect("engine is open");
        let diagnostics = engine.observe_diagnostics().expect("engine is open");

        // Drain the one proactive delivery each stream makes on open (a
        // fresh subscribe always gets one -- possibly empty -- batch;
        // `observe_diagnostics` delivers the CURRENT snapshot immediately)
        // so the post-drop assertions below observe a disconnect, not
        // leftover backlog.
        subscription
            .recv()
            .expect("a fresh subscribe delivers one batch before anything else happens");
        diagnostics
            .recv()
            .expect("observe_diagnostics delivers the current snapshot immediately");

        // Drop `engine` on a WORKER thread and signal completion over a
        // channel, rather than dropping it inline on this thread and
        // checking elapsed time afterward -- a synchronous `drop` that
        // deadlocked inside `shutdown`+`join` would never reach an
        // `elapsed` check at all, so that shape isn't a real liveness
        // bound (it just hangs until the outer test-runner's own
        // timeout). `recv_timeout` on THIS thread is what makes a `Drop`
        // deadlock trip the bound as an ordinary assertion failure
        // instead of a hang.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(engine);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("Drop must tear EngineThread down within a bounded wait, not hang");

        match subscription.recv() {
            Err(_) => {}
            Ok(msg) => panic!(
                "query channel must disconnect once the dropped engine's thread has \
                 fully exited, got another batch instead: {msg:?}"
            ),
        }
        assert!(
            diagnostics.recv().is_none(),
            "diagnostics channel must disconnect (None) once the engine is dropped"
        );

        // Both surviving handles' own `Drop` (unsubscribe/cancel) must not
        // panic even though the engine that owned them is already gone.
        drop(subscription);
        drop(diagnostics);
    }

    /// codex-nova's non-negotiable proof #1: `ObservationCancel::cancel()`
    /// called from ANOTHER handle must unblock a drain loop genuinely
    /// parked inside `Subscription::recv()`, within a bounded wait -- not
    /// rely on that loop's own next `recv()` call to eventually notice a
    /// disconnect on its own timescale. This is exactly the shape
    /// `nmp-ffi`'s drain thread depends on: it owns the `Subscription`
    /// (`recv()` blocks, so nothing else can), while a caller-held
    /// `cancel_handle()` clone triggers withdrawal from elsewhere.
    #[test]
    fn cancel_handle_unblocks_a_genuinely_blocked_recv_within_a_bound() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let subscription = engine.observe(probe_query()).expect("engine is open");

        // Drain the one proactive delivery a fresh subscribe always makes,
        // so the drain thread's `recv()` below has nothing already queued
        // and must genuinely block.
        subscription
            .recv()
            .expect("a fresh subscribe delivers one batch before anything else happens");

        let cancel = subscription.cancel_handle();

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // No further events are ever published against this probe
            // query (no relays configured, arbitrary caller-owned kind) --
            // absent cancellation, this call blocks forever.
            let result = subscription.recv();
            let _ = result_tx.send(result.is_err());
        });

        cancel.cancel();

        let disconnected = result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect(
                "cancel() from a separate handle must unblock the drain thread's recv() \
                 within a bounded wait, not hang",
            );
        assert!(
            disconnected,
            "the unblocked recv() must observe a disconnect (Err), the same outcome \
             Drop-driven withdrawal produces"
        );

        engine.shutdown();
    }

    /// codex-nova's non-negotiable proof #3: an `Engine` with a LIVE query
    /// subscription AND a live diagnostics subscription -- neither
    /// cancelled, both still holding an outstanding `cancel_handle()` clone
    /// nobody ever calls -- must still `shutdown()` cleanly within a
    /// bounded wait. An outstanding, never-invoked cancel token must not
    /// become a reason `shutdown` hangs or panics.
    #[test]
    fn shutdown_stays_clean_with_outstanding_cancel_tokens_for_query_and_diagnostics() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");

        let subscription = engine.observe(probe_query()).expect("engine is open");
        let diagnostics = engine.observe_diagnostics().expect("engine is open");

        // Obtain (but deliberately never call before shutdown) a cancel
        // token for each -- an outstanding, uninvoked token is the scenario
        // under test.
        let query_cancel = subscription.cancel_handle();
        let diagnostics_cancel = diagnostics.cancel_handle();

        subscription
            .recv()
            .expect("a fresh subscribe delivers one batch before anything else happens");
        diagnostics
            .recv()
            .expect("observe_diagnostics delivers the current snapshot immediately");

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            engine.shutdown();
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect(
                "shutdown() must complete within a bounded wait even with outstanding, \
             never-cancelled tokens still alive",
            );

        // The outstanding tokens themselves must still be safe to cancel
        // (or simply drop) after the engine they named is already gone.
        query_cancel.cancel();
        diagnostics_cancel.cancel();
    }

    fn probe_query() -> LiveQuery {
        LiveQuery::from_filter(nmp_grammar::Filter {
            // An arbitrary caller-owned kind, not any NIP-01 core schema --
            // see this module's other fixtures for why.
            kinds: Some(std::collections::BTreeSet::from([9999u16])),
            ..nmp_grammar::Filter::default()
        })
    }
}
