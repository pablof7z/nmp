//! `NmpEngine` -- the UniFFI object wrapping [`nmp::Engine`] (M4 plan §2/§9;
//! rethreaded onto the `nmp` facade crate for #52 Unit B). This is the top of
//! the dependency graph: nothing in the workspace depends on `nmp-ffi`, it is
//! the native-only staticlib a Swift app links against in place of writing
//! its own app-loop over `nmp` directly.
//!
//! Construction, store/directory selection, the router cap, nsec parsing,
//! and the caller-supplied-`Signed` verify all used to be assembled by hand
//! HERE -- they now live in `nmp::Engine`/`nmp::EngineConfig` (and, for the
//! verify, `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary,
//! Unit A0/#56) so every entry point -- this facade, a direct-Rust app, any
//! `from_parts`/raw-`EngineThread` caller -- inherits the same guarantees.
//! `nmp-ffi` is now only: config/type mirroring across the FFI boundary, and
//! the drain-thread bridge from `nmp`'s blocking `recv()` verbs to UniFFI's
//! callback-interface observers (`convert`/`observer`).
//!
//! Directory: `nmp_router::LiveDirectory` (M5's self-bootstrapping outbox,
//! now assembled inside `nmp::Engine::new`) is what backs every `NmpEngine`
//! -- a Swift app supplies ONLY the operator indexer relay set; every
//! author's NIP-65 write relays (including the app's own account) are
//! discovered by the engine itself, live, via its own internal kind:10002
//! reads against those same indexers (`nmp_engine::core::EngineCore`'s
//! auto-discovery). `NmpEngineConfig` no longer accepts a pre-resolved
//! write-relay map -- there is nothing for a caller to resolve up front.

use std::sync::Arc;
use std::thread;

use nmp_engine::runtime::{DiagnosticsHandle, Handle, QueryHandle};

use crate::convert::{
    coverage_to_ffi, diagnostics_snapshot_to_ffi, filter_from_ffi, parse_pubkey, row_delta_to_ffi,
    write_intent_from_ffi, write_status_to_ffi, FfiError, WriteStatusRef,
};
use crate::observer::{DiagnosticsObserver, ReceiptObserver, RowObserver};
use crate::types::{FfiFilter, FfiWriteIntent};

/// Construction config for [`NmpEngine::new`]. See the module doc: the only
/// relay facts a caller ever supplies are the three operator-configured
/// lanes -- `indexer_relays`, `app_relays`, `fallback_relays`
/// (`routing-and-ownership.md` §2.1) -- everything else is discovered live.
#[derive(uniffi::Record, Clone, Debug, Default)]
pub struct NmpEngineConfig {
    /// `None` -> in-memory store (nothing survives a restart). `Some(path)`
    /// -> a persistent `RedbStore` opened at that path (the same file
    /// reopened across restarts is what makes a cold, offline read
    /// authoritative -- ledger #7).
    pub store_path: Option<String>,
    pub indexer_relays: Vec<String>,
    /// Operator app relay set (`Lane::AppRelay`). Default empty.
    pub app_relays: Vec<String>,
    /// Operator fallback relay set (`Lane::Fallback`). Default empty.
    pub fallback_relays: Vec<String>,
}

impl From<NmpEngineConfig> for nmp::EngineConfig {
    fn from(config: NmpEngineConfig) -> Self {
        nmp::EngineConfig {
            store_path: config.store_path,
            indexer_relays: config.indexer_relays,
            app_relays: config.app_relays,
            fallback_relays: config.fallback_relays,
        }
    }
}

/// The UniFFI-exported engine object. `new` is the ONE construction call the
/// M4 kill test (plan §7) requires -- everything past construction is a
/// method call on this object, never a second container the app must adopt.
/// Wraps a single [`nmp::Engine`] -- the one supported Rust product surface
/// -- rather than independently assembling `nmp-store`/`nmp-router`/
/// `nmp-transport`/`nmp-resolver` mechanism types (#52).
#[derive(uniffi::Object)]
pub struct NmpEngine {
    engine: nmp::Engine,
}

#[uniffi::export]
impl NmpEngine {
    #[uniffi::constructor]
    pub fn new(config: NmpEngineConfig) -> Result<Arc<Self>, FfiError> {
        let engine = nmp::Engine::new(config.into())?;
        Ok(Arc::new(Self { engine }))
    }

    /// Register an account from its secret key (hex or bech32 `nsec`). The
    /// key crosses this boundary exactly once, as a value, and lives in the
    /// engine from this point on (VISION ledger #12; M4 plan §5) -- this
    /// method does NOT make the account active, call
    /// [`Self::set_active_account`] for that. Returns the account's hex
    /// public key.
    pub fn add_account(&self, secret_key: String) -> Result<String, FfiError> {
        let pk = self.engine.add_account(&secret_key)?;
        Ok(pk.to_hex())
    }

    /// Re-root every reactive query AND the active signing capability
    /// together onto `pubkey` (`None` -> logged-out / read-only). `pubkey`
    /// need not have been added via [`Self::add_account`] -- read-only
    /// browsing of an account this app holds no key for is legal; any
    /// publish attempted while active in that state terminates
    /// `WriteStatus::Failed`, never a panic (M4 plan §5).
    pub fn set_active_account(&self, pubkey: Option<String>) -> Result<(), FfiError> {
        let pk = pubkey.as_deref().map(parse_pubkey).transpose()?;
        self.engine.set_active_account(pk)?;
        Ok(())
    }

    /// Open a live subscription. `observer` is driven from a dedicated drain
    /// thread (M4 plan §4b) -- never the engine thread itself. The returned
    /// [`NmpQueryHandle`]'s `Drop` withdraws the subscription (deinit-tied
    /// demand drop, plan §4c); call [`NmpQueryHandle::cancel`] for an
    /// explicit early teardown instead of waiting on Swift's own `deinit`
    /// timing.
    pub fn observe(
        &self,
        query: FfiFilter,
        observer: Box<dyn RowObserver>,
    ) -> Result<Arc<NmpQueryHandle>, FfiError> {
        let filter = filter_from_ffi(query)?;
        let subscription = self.engine.observe(nmp::LiveQuery(filter))?;
        let (handle, query_handle) = subscription.cancel_handle();

        thread::spawn(move || {
            while let Ok((deltas, coverage)) = subscription.recv() {
                let ffi_deltas = deltas.iter().map(row_delta_to_ffi).collect();
                observer.on_batch(ffi_deltas, coverage_to_ffi(coverage));
            }
            observer.on_closed();
        });

        Ok(Arc::new(NmpQueryHandle {
            handle,
            query_handle,
        }))
    }

    /// Enqueue a write. `observer` streams every `WriteStatus` this intent
    /// ever reaches (ledger #9 -- enqueue is not converged; the first value
    /// is never a terminal for a durable/at-most-once intent). A
    /// caller-supplied `Signed` payload that fails verification is no
    /// longer a synchronous error here (that guarantee moved to
    /// `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary,
    /// Unit A0/#56, so it holds for every entry point, not only this one) --
    /// it surfaces as `WriteStatus::Failed`, the FIRST and only status
    /// `observer` receives, with no preceding `Accepted`.
    pub fn publish(
        &self,
        intent: FfiWriteIntent,
        observer: Box<dyn ReceiptObserver>,
    ) -> Result<(), FfiError> {
        let write_intent = write_intent_from_ffi(intent)?;
        let receipt_rx = self.engine.publish(write_intent)?;

        thread::spawn(move || {
            while let Ok(status) = receipt_rx.recv() {
                observer.on_status(write_status_to_ffi(WriteStatusRef(&status)));
            }
        });

        Ok(())
    }

    /// Open a live diagnostics stream (M5 plan §1.2 step 5) -- "the
    /// acceptance test rendered on screen, permanently." `observer` is
    /// driven from a dedicated drain thread, mirroring [`Self::observe`];
    /// the returned [`NmpDiagnosticsHandle`]'s `Drop` withdraws the
    /// observer (deinit-tied teardown, same discipline as
    /// [`NmpQueryHandle`]). Delivers the CURRENT snapshot immediately, then
    /// a fresh one on every recompile/EOSE-driven coverage change --
    /// pushed reactively, never polled.
    pub fn observe_diagnostics(
        &self,
        observer: Box<dyn DiagnosticsObserver>,
    ) -> Result<Arc<NmpDiagnosticsHandle>, FfiError> {
        let subscription = self.engine.observe_diagnostics()?;
        let diag_handle = subscription.cancel_handle();

        thread::spawn(move || {
            while let Some(snapshot) = subscription.recv() {
                observer.on_snapshot(diagnostics_snapshot_to_ffi(snapshot));
            }
            observer.on_closed();
        });

        Ok(Arc::new(NmpDiagnosticsHandle { diag_handle }))
    }

    /// Stop the engine. Idempotent: a second call is a no-op (`nmp::Engine`'s
    /// own serialized lifecycle gate, see that type's doc).
    pub fn shutdown(&self) {
        self.engine.shutdown();
    }
}

/// The app-facing handle to a live subscription (returned by
/// [`NmpEngine::observe`]). `Drop` withdraws the subscription -- the SDK
/// never requires an app-owned container or lifecycle hook to make this
/// happen (plan §7's kill test). Holds only the cheap `(Handle,
/// QueryHandle)` cancel capability (`nmp::Subscription::cancel_handle`) --
/// the receiving half of the subscription is owned entirely by
/// [`NmpEngine::observe`]'s drain thread, since `recv()` blocks.
#[derive(uniffi::Object)]
pub struct NmpQueryHandle {
    handle: Handle,
    query_handle: QueryHandle,
}

#[uniffi::export]
impl NmpQueryHandle {
    /// Withdraw the subscription now, rather than waiting for `Drop` (a
    /// Swift `deinit` can be delayed by ARC in ways an app may want to
    /// preempt explicitly). Safe to call more than once, and safe to never
    /// call at all (in which case `Drop` is what withdraws it).
    pub fn cancel(&self) {
        self.handle.unsubscribe(self.query_handle);
    }
}

impl Drop for NmpQueryHandle {
    fn drop(&mut self) {
        self.handle.unsubscribe(self.query_handle);
    }
}

/// The app-facing handle to a live diagnostics stream (returned by
/// [`NmpEngine::observe_diagnostics`]). Same discipline as [`NmpQueryHandle`]
/// -- holds only the cheap `DiagnosticsHandle` cancel capability
/// (`nmp::DiagnosticsSubscription::cancel_handle`), already non-consuming
/// and idempotent on its own.
#[derive(uniffi::Object)]
pub struct NmpDiagnosticsHandle {
    diag_handle: DiagnosticsHandle,
}

#[uniffi::export]
impl NmpDiagnosticsHandle {
    /// Withdraw this diagnostics observer now, rather than waiting for
    /// `Drop`. Safe to call more than once; safe to never call at all.
    pub fn cancel(&self) {
        self.diag_handle.cancel();
    }
}

impl Drop for NmpDiagnosticsHandle {
    fn drop(&mut self) {
        self.diag_handle.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FfiDurability, FfiWritePayload, FfiWriteRouting, FfiWriteStatus};
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

    struct ChannelReceiptObserver {
        tx: Mutex<mpsc::Sender<FfiWriteStatus>>,
    }

    impl ReceiptObserver for ChannelReceiptObserver {
        fn on_status(&self, status: FfiWriteStatus) {
            let _ = self.tx.lock().unwrap().send(status);
        }
    }

    /// #52's headline falsifier, exercised through the FFI boundary this
    /// time (the direct-Rust equivalent lives in `nmp::Engine`'s own tests):
    /// a tampered `FfiWritePayload::Signed` is no longer a synchronous
    /// `FfiError` -- `NmpEngine::publish` accepts it and the rejection
    /// surfaces on the receipt stream as `WriteStatus::Failed`, the FIRST
    /// and only status delivered, proving the verify inherited from
    /// `nmp::Engine`'s acceptance boundary (Unit A0) covers this entry point
    /// too, not only direct-Rust.
    #[test]
    fn ffi_tampered_signed_publish_fails_closed_on_receipt_stream() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");

        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9999), "original")
            .sign_with_keys(&keys)
            .expect("test fixture must sign cleanly");

        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Signed {
                id: event.id.to_hex(),
                pubkey: event.pubkey.to_hex(),
                created_at: event.created_at.as_secs(),
                kind: event.kind.as_u16(),
                tags: event.tags.iter().map(|t| t.clone().to_vec()).collect(),
                // Tampered after signing: id/sig no longer match this
                // content, but every field still parses fine at the FFI
                // boundary (marshaling only, no verify here anymore).
                content: "tampered".to_string(),
                sig: event.sig.to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
        };

        let (tx, rx) = mpsc::channel();
        let observer = Box::new(ChannelReceiptObserver { tx: Mutex::new(tx) });

        engine
            .publish(intent, observer)
            .expect("a well-formed (if tampered) Signed payload must parse at the FFI boundary");

        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a Durable intent must yield a status")
        {
            FfiWriteStatus::Failed { .. } => {}
            other => panic!("expected FfiWriteStatus::Failed, got {other:?}"),
        }
        assert!(
            rx.recv_timeout(Duration::from_secs(1)).is_err(),
            "Failed must be the sole terminal status -- no Accepted, nothing further"
        );

        engine.shutdown();
    }
}
