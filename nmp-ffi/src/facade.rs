//! `NmpEngine` -- the UniFFI object wrapping `nmp_engine::runtime::
//! {EngineThread, Handle}` (M4 plan §2/§9). This is the top of the
//! dependency graph: nothing in the workspace depends on `nmp-ffi`, it is
//! the native-only staticlib a Swift app links against in place of writing
//! its own app-loop over `nmp-engine` directly.
//!
//! Directory scope note (honesty over gold-plating): `nmp_router::
//! RelayDirectory` has exactly one concrete implementation anywhere in this
//! workspace today -- `FixtureDirectory`, a static fact lookup with no
//! network (see that type's own module doc and `nmp-demo/src/directory.rs`'s
//! `BootstrapDirectory`, which wraps it with an app-owned one-shot NIP-65
//! resolution phase). Building a live, self-refreshing directory
//! implementation is a substantial separate piece of work already flagged
//! as a known gap by the demo crate -- it is NOT part of this crate's scope
//! (M4 plan §5's note: "M4 adds NO reducer behaviour... it does not touch
//! `EngineCore`, `nmp-resolver`, or `nmp-router`"). `NmpEngineConfig` below
//! accepts the SAME static snapshot shape `FixtureDirectory` already takes
//! (indexers + a per-author write-relay map) so a Swift app can hand in
//! whatever it already resolved (e.g. via its own bootstrap step) -- routing
//! for an author with no entry in that map fails closed
//! (`WriteStatus::Failed("no write relays known for author ...")`), exactly
//! as it does for every other caller of this directory today.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

use nmp_engine::runtime::{EngineThread, Handle, QueryHandle};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_signer::LocalKeySigner;
use nmp_store::{MemoryStore, RedbStore};
use nmp_transport::PoolConfig;
use nostr::Keys;

use crate::convert::{
    coverage_to_ffi, filter_from_ffi, parse_pubkey, parse_relay_url, row_delta_to_ffi,
    write_intent_from_ffi, write_status_to_ffi, FfiError, WriteStatusRef,
};
use crate::observer::{ReceiptObserver, RowObserver};
use crate::types::{FfiFilter, FfiWriteIntent};

/// Matches `nmp-demo`'s own constant (`nmp-demo/src/main.rs`) -- the router
/// compiler's per-tick atom-count cap; not tuned differently here.
const ROUTER_CAP: usize = 10;

/// Construction config for [`NmpEngine::new`]. See the module doc for why
/// `write_relays` is a static snapshot rather than a live-resolved fact
/// source.
#[derive(uniffi::Record, Clone, Debug, Default)]
pub struct NmpEngineConfig {
    /// `None` -> in-memory store (nothing survives a restart). `Some(path)`
    /// -> a persistent `RedbStore` opened at that path (the same file
    /// reopened across restarts is what makes a cold, offline read
    /// authoritative -- ledger #7).
    pub store_path: Option<String>,
    pub indexer_relays: Vec<String>,
    pub write_relays: HashMap<String, Vec<String>>,
}

fn build_directory(config: &NmpEngineConfig) -> Result<FixtureDirectory, FfiError> {
    let mut dir = FixtureDirectory::new();
    for relay in &config.indexer_relays {
        dir = dir.with_indexer(parse_relay_url(relay)?);
    }
    for (author, relays) in &config.write_relays {
        let urls = relays
            .iter()
            .map(|u| parse_relay_url(u))
            .collect::<Result<Vec<_>, _>>()?;
        dir = dir.with_write(author.clone(), urls);
    }
    Ok(dir)
}

/// The UniFFI-exported engine object. `new` is the ONE construction call the
/// M4 kill test (plan §7) requires -- everything past construction is a
/// method call on this object, never a second container the app must adopt.
#[derive(uniffi::Object)]
pub struct NmpEngine {
    handle: Handle,
    // `EngineThread` isn't `Clone`; parked behind a `Mutex<Option<_>>` purely
    // so `shutdown` (an `&self` method -- UniFFI objects are always shared,
    // never moved out of, across the FFI boundary) can `take()` it once and
    // `join`. `None` after the first `shutdown` call -- idempotent.
    engine_thread: Mutex<Option<EngineThread>>,
}

#[uniffi::export]
impl NmpEngine {
    #[uniffi::constructor]
    pub fn new(config: NmpEngineConfig) -> Result<Arc<Self>, FfiError> {
        let directory = build_directory(&config)?;

        let (engine_thread, handle) = match config.store_path {
            Some(path) => {
                let store = RedbStore::open(&path).map_err(|e| FfiError::StoreOpenFailed {
                    reason: e.to_string(),
                })?;
                EngineThread::spawn(store, directory, ROUTER_CAP, PoolConfig::default())
            }
            None => {
                let store = MemoryStore::new();
                EngineThread::spawn(store, directory, ROUTER_CAP, PoolConfig::default())
            }
        };

        Ok(Arc::new(Self {
            handle,
            engine_thread: Mutex::new(Some(engine_thread)),
        }))
    }

    /// Register an account from its secret key (hex or bech32 `nsec`). The
    /// key crosses this boundary exactly once, as a value, and lives in the
    /// engine from this point on (VISION ledger #12; M4 plan §5) -- this
    /// method does NOT make the account active, call
    /// [`Self::set_active_account`] for that. Returns the account's hex
    /// public key.
    pub fn add_account(&self, secret_key: String) -> Result<String, FfiError> {
        let keys = Keys::parse(&secret_key).map_err(|_| FfiError::InvalidSecretKey)?;
        let pk = self
            .handle
            .add_signer(LocalKeySigner::new(keys))
            .ok_or(FfiError::SignerHasNoPublicKey)?;
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
        self.handle.set_active_account(pk);
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
        let (query_handle, rows_rx) = self.handle.subscribe(LiveQuery(filter));

        thread::spawn(move || {
            while let Ok((deltas, coverage)) = rows_rx.recv() {
                let ffi_deltas = deltas.iter().map(row_delta_to_ffi).collect();
                observer.on_batch(ffi_deltas, coverage_to_ffi(coverage));
            }
            observer.on_closed();
        });

        Ok(Arc::new(NmpQueryHandle {
            handle: self.handle.clone(),
            query_handle,
        }))
    }

    /// Enqueue a write. `observer` streams every `WriteStatus` this intent
    /// ever reaches (ledger #9 -- enqueue is not converged; the first value
    /// is never a terminal for a durable/at-most-once intent).
    pub fn publish(
        &self,
        intent: FfiWriteIntent,
        observer: Box<dyn ReceiptObserver>,
    ) -> Result<(), FfiError> {
        let write_intent = write_intent_from_ffi(intent)?;
        let receipt_rx = self.handle.publish(write_intent);

        thread::spawn(move || {
            while let Ok(status) = receipt_rx.recv() {
                observer.on_status(write_status_to_ffi(WriteStatusRef(&status)));
            }
        });

        Ok(())
    }

    /// Stop the engine. Idempotent: a second call finds the thread already
    /// taken and no-ops.
    pub fn shutdown(&self) {
        self.handle.shutdown();
        if let Some(thread) = self.engine_thread.lock().unwrap().take() {
            thread.join();
        }
    }
}

/// The app-facing handle to a live subscription (returned by
/// [`NmpEngine::observe`]). `Drop` withdraws the subscription -- the SDK
/// never requires an app-owned container or lifecycle hook to make this
/// happen (plan §7's kill test).
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
