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

use std::sync::mpsc::Receiver;
use std::sync::Mutex;

use nmp_engine::outbox::{WriteIntent, WriteStatus};
use nmp_engine::runtime::{EngineThread, Handle};
use nmp_resolver::LiveQuery;
use nmp_signer::{LocalKeySigner, SigningCapability};
use nmp_store::{MemoryStore, RedbStore};
use nmp_transport::PoolConfig;
use nostr::{Keys, PublicKey};

use crate::config::{build_directory, EngineConfig};
use crate::error::EngineError;
use crate::subscription::{DiagnosticsSubscription, Subscription};

/// The router compiler's per-tick atom-count cap. Both `nmp-ffi` and
/// `nmp-demo` hardcoded their own copy of this constant before #52; the
/// facade now owns the one value.
const ROUTER_CAP: usize = 10;

/// The one supported Rust product surface (canonical-facade-52-plan.md §1).
/// Owns the `EngineThread` + `Handle` pair; every mechanism crate
/// (`nmp-store`/`nmp-router`/`nmp-transport`/`nmp-resolver`) is reached only
/// through here.
pub struct Engine {
    handle: Handle,
    // `EngineThread` isn't `Clone`; parked behind a `Mutex<Option<_>>` purely
    // so `shutdown` (an `&self` method) can `take()` it once and `join` --
    // same discipline as `nmp-ffi::NmpEngine`.
    engine_thread: Mutex<Option<EngineThread>>,
}

impl Engine {
    /// The ONE construction call: config -> store/directory selection,
    /// router cap, everything `nmp-ffi::facade::build_directory` and
    /// `nmp-demo`'s hand-rolled assembly used to duplicate independently.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        let directory = build_directory(&config)?;

        let (engine_thread, handle) = match &config.store_path {
            Some(path) => {
                let store = RedbStore::open(path).map_err(|e| EngineError::StoreOpenFailed {
                    reason: e.to_string(),
                })?;
                EngineThread::spawn(store, directory, ROUTER_CAP, PoolConfig::default())
            }
            None => {
                let store = MemoryStore::new();
                EngineThread::spawn(store, directory, ROUTER_CAP, PoolConfig::default())
            }
        };

        Ok(Self {
            handle,
            engine_thread: Mutex::new(Some(engine_thread)),
        })
    }

    /// #52 Q3's unstable escape hatch: construct directly from an
    /// already-built store/directory pair, bypassing `EngineConfig`
    /// entirely. `#[doc(hidden)]` and gated behind the `unstable-mechanism`
    /// feature -- the ONLY sanctioned way to inject a store (needed by
    /// `nmp-bdd`, which spawns the real `EngineThread` against scripted
    /// in-process relays). This is a stability exception, not a security
    /// one: an engine built this way still verifies every `Signed` payload
    /// at the acceptance boundary (Unit A0), same as every other entry
    /// point.
    #[cfg(feature = "unstable-mechanism")]
    #[doc(hidden)]
    pub fn from_parts<S, D>(store: S, directory: D, cap: usize, pool_config: PoolConfig) -> Self
    where
        S: nmp_store::EventStore + Send + 'static,
        D: nmp_router::RelayDirectory + Send + 'static,
    {
        let (engine_thread, handle) = EngineThread::spawn(store, directory, cap, pool_config);
        Self {
            handle,
            engine_thread: Mutex::new(Some(engine_thread)),
        }
    }

    /// Noun 1: open a live query. The returned [`Subscription`] withdraws
    /// itself on `Drop` (see that type's doc).
    #[must_use]
    pub fn observe(&self, query: LiveQuery) -> Subscription {
        let (query_handle, rows) = self.handle.subscribe(query);
        Subscription::new(self.handle.clone(), query_handle, rows)
    }

    /// Noun 2: enqueue a write. Fire-and-forget: the returned `Receiver`
    /// streams every `WriteStatus` this intent ever reaches (ledger #9 --
    /// enqueue is not converged). A tampered `WritePayload::Signed` is
    /// rejected at the engine's acceptance boundary and surfaces here as a
    /// `WriteStatus::Failed` with no preceding `Accepted` -- see this
    /// module's doc.
    #[must_use]
    pub fn publish(&self, intent: WriteIntent) -> Receiver<WriteStatus> {
        self.handle.publish(intent)
    }

    /// Register an account from its secret key (hex or bech32 `nsec`). Does
    /// NOT make the account active -- call [`Self::set_active_account`] for
    /// that. Returns the account's public key.
    pub fn add_account(&self, secret_key: &str) -> Result<PublicKey, EngineError> {
        let keys = Keys::parse(secret_key).map_err(|_| EngineError::InvalidSecretKey)?;
        self.handle
            .add_signer(LocalKeySigner::new(keys))
            .ok_or(EngineError::SignerHasNoPublicKey)
    }

    /// Register an arbitrary signing capability (e.g. a NIP-46/bunker
    /// remote signer) -- the lower-level verb [`Self::add_account`] sits on
    /// top of for the common local-key case. Same "does not activate it"
    /// caveat as `add_account`.
    pub fn add_signer<Sig>(&self, signer: Sig) -> Option<PublicKey>
    where
        Sig: SigningCapability + Send + 'static,
    {
        self.handle.add_signer(signer)
    }

    /// Re-root every reactive query AND the active signing capability
    /// together onto `pubkey` (`None` -> logged-out / read-only). `pubkey`
    /// need not have been registered via [`Self::add_account`]/
    /// [`Self::add_signer`] -- read-only browsing of an account this app
    /// holds no key for is legal; any publish attempted while active in
    /// that state terminates `WriteStatus::Failed`, never a panic.
    pub fn set_active_account(&self, pubkey: Option<PublicKey>) {
        self.handle.set_active_account(pubkey);
    }

    /// Open a live diagnostics stream. Same `Drop` discipline as
    /// [`Self::observe`] -- see [`DiagnosticsSubscription`]'s doc.
    #[must_use]
    pub fn observe_diagnostics(&self) -> DiagnosticsSubscription {
        let (diag_handle, snapshots) = self.handle.observe_diagnostics();
        DiagnosticsSubscription::new(diag_handle, snapshots)
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

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_engine::outbox::{Durability, WritePayload, WriteRouting};
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
        let mut event = nostr::EventBuilder::new(nostr::Kind::TextNote, "original")
            .sign_with_keys(&keys)
            .expect("test fixture must sign cleanly");
        // Tamper the content after signing: id/sig no longer match it, but
        // the event otherwise still looks well-formed.
        event.content = "tampered".to_string();

        let rx = engine.publish(WriteIntent {
            payload: WritePayload::Signed(event),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        });

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
    /// finds the engine thread already taken and no-ops rather than
    /// panicking.
    #[test]
    fn shutdown_is_idempotent() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        engine.shutdown();
        engine.shutdown();
    }
}
