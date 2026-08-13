//! [`Engine`] -- the one supported construction call plus the two nouns
//! (canonical-facade-52-plan.md §1). Owns config -> store/routing-fact
//! selection and the router cap both `nmp-ffi` and `nmp-demo` used to
//! duplicate by hand.
//!
//! No `Signed`-payload verify lives here: that guarantee moved to
//! `crate::core::EngineCore::on_publish`'s acceptance boundary (Unit
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

use std::sync::Mutex;

use crate::core::ReceiptId;
use crate::publish_queue::{
    PublishQueueEntry, PublishQueueReadError, ReceiptResult, ReceiptResultError,
    RemoveQueueEntryError,
};
#[cfg(any(test, feature = "test-instrumentation"))]
use crate::runtime::SignerRegistration;
use crate::runtime::{
    EngineThread, Handle, HistoryHandle, HistoryReceiver, QueryHandle, ReceiptReattachment,
    ReceiptReplayCursor, ReceiptStream, RowsReceiver, RuntimeConfig, SignEventError,
    SignEventOperation,
};
use nmp_grammar::LiveQuery;
use nmp_grammar::WriteIntent;
use nmp_signer::SigningCapability;
use nmp_store::{MemoryStore, RedbStore, RedbStoreOpenError, RedbStoreResetError};
use nmp_transport::PoolConfig;
use nostr::secp256k1::rand::{rngs::OsRng, RngCore};
use nostr::RelayUrl;
use nostr::{EventId, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

use crate::auth::{AuthPolicy, EngineAuthPolicyAdapter};
#[cfg(feature = "nip65")]
use crate::config::build_nip65_sources;
use crate::config::{build_routing_facts, EngineConfig};
use crate::error::EngineError;
use crate::relay_information::{
    RelayInformationCachePolicy, RelayInformationError, RelayInformationSnapshot,
};
use crate::subscription::{
    AsyncDiagnosticsSubscription, AsyncSubscription, DiagnosticsSubscription, Subscription, Window,
};

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

/// The only successful result from explicit pre-signature cancellation.
/// The closed success type cannot carry a status that cancellation did not
/// commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelWriteOutcome {
    Cancelled,
}

/// Typed refusal from explicit pre-signature write cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelWriteError {
    UnknownReceipt {
        receipt_id: ReceiptId,
    },
    AlreadySigned {
        receipt_id: ReceiptId,
        event_id: EventId,
    },
    AlreadyCompensated {
        receipt_id: ReceiptId,
    },
    AlreadySuperseded {
        receipt_id: ReceiptId,
    },
    /// The write was refused at acceptance and is already a permanently
    /// failed queue entry. There is nothing to cancel; remove it instead.
    AlreadyRefused {
        receipt_id: ReceiptId,
    },
    PersistenceFailed {
        receipt_id: ReceiptId,
        reason: String,
    },
    EngineClosed,
}

fn cancel_write_outcome_from_engine(
    outcome: crate::publish_queue::CancelWriteOutcome,
) -> CancelWriteOutcome {
    match outcome {
        crate::publish_queue::CancelWriteOutcome::Cancelled => CancelWriteOutcome::Cancelled,
    }
}

fn cancel_write_error_from_engine(
    error: crate::publish_queue::CancelWriteError,
) -> CancelWriteError {
    match error {
        crate::publish_queue::CancelWriteError::UnknownReceipt { receipt_id } => {
            CancelWriteError::UnknownReceipt { receipt_id }
        }
        crate::publish_queue::CancelWriteError::AlreadySigned {
            receipt_id,
            event_id,
        } => CancelWriteError::AlreadySigned {
            receipt_id,
            event_id,
        },
        crate::publish_queue::CancelWriteError::AlreadyCompensated { receipt_id } => {
            CancelWriteError::AlreadyCompensated { receipt_id }
        }
        crate::publish_queue::CancelWriteError::AlreadySuperseded { receipt_id } => {
            CancelWriteError::AlreadySuperseded { receipt_id }
        }
        crate::publish_queue::CancelWriteError::AlreadyRefused { receipt_id } => {
            CancelWriteError::AlreadyRefused { receipt_id }
        }
        crate::publish_queue::CancelWriteError::PersistenceFailed { receipt_id, reason } => {
            CancelWriteError::PersistenceFailed { receipt_id, reason }
        }
        crate::publish_queue::CancelWriteError::EngineClosed => CancelWriteError::EngineClosed,
    }
}

fn session_mutation_from_add_signer(
    error: crate::runtime::AddSignerError,
) -> crate::SessionMutationError {
    match error {
        crate::runtime::AddSignerError::RegistryFull { limit } => {
            crate::SessionMutationError::CapabilityRegistryFull { limit }
        }
        crate::runtime::AddSignerError::CapabilityInstanceExhausted => {
            crate::SessionMutationError::CapabilityInstanceExhausted
        }
        crate::runtime::AddSignerError::EngineShuttingDown
        | crate::runtime::AddSignerError::MissingPublicKey => {
            crate::SessionMutationError::EngineClosed
        }
    }
}

impl std::fmt::Display for CancelWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownReceipt { receipt_id } => write!(f, "unknown receipt {}", receipt_id.0),
            Self::AlreadySigned {
                receipt_id,
                event_id,
            } => write!(
                f,
                "receipt {} is already signed as {event_id}",
                receipt_id.0
            ),
            Self::AlreadyCompensated { receipt_id } => {
                write!(f, "receipt {} is already compensated", receipt_id.0)
            }
            Self::AlreadySuperseded { receipt_id } => {
                write!(
                    f,
                    "receipt {} was superseded by a newer write",
                    receipt_id.0
                )
            }
            Self::AlreadyRefused { receipt_id } => {
                write!(f, "receipt {} was refused at acceptance", receipt_id.0)
            }
            Self::PersistenceFailed { receipt_id, reason } => write!(
                f,
                "could not persist cancellation for receipt {}: {reason}",
                receipt_id.0
            ),
            Self::EngineClosed => f.write_str("engine already shut down"),
        }
    }
}

impl std::error::Error for CancelWriteError {}

/// Opaque ownership proof for one exact AUTH-policy installation (#8).
/// Replacement invalidates it, and a stale clone cannot detach the
/// replacement.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthPolicyRegistration {
    inner: crate::runtime::AuthPolicyRegistration,
}

impl AuthPolicyRegistration {
    /// The frozen account identity this policy decides for.
    #[must_use]
    pub fn expected_public_key(&self) -> PublicKey {
        self.inner.expected_pubkey()
    }
}

impl std::fmt::Debug for AuthPolicyRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPolicyRegistration")
            .field("expected_public_key", &self.expected_public_key())
            .finish_non_exhaustive()
    }
}

/// One event body to sign with the current account without publishing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignEventRequest {
    pub created_at: Timestamp,
    pub kind: Kind,
    pub tags: Vec<Tag>,
    pub content: String,
}

/// Failure of an explicit NIP-11 one-shot: lifecycle/URL validation stays
/// distinct from network/document acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayInformationRequestError {
    Engine(EngineError),
    Acquisition(RelayInformationError),
}

impl std::fmt::Display for RelayInformationRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(error) => error.fmt(f),
            Self::Acquisition(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RelayInformationRequestError {}

impl Engine {
    /// Configure one synchronous capability implementation and return the
    /// only supported constructor for operations bound to that installation.
    pub fn add_replaceable_materializer<M>(
        &self,
        program: [u8; 16],
        format: [u8; 16],
        materializer: M,
    ) -> Result<crate::RegisteredReplaceableMaterializer, EngineError>
    where
        M: crate::ReplaceableMaterializer,
    {
        let mut instance = [0u8; 16];
        OsRng.fill_bytes(&mut instance);
        let registration = crate::replaceable_materializer::ReplaceableMaterializerRegistration {
            instance,
            program,
            format,
            materializer: std::sync::Arc::new(materializer),
        };
        self.with_handle(|handle| handle.add_replaceable_materializer(registration))?
            .map_err(EngineError::from_start_error)?;
        Ok(crate::RegisteredReplaceableMaterializer { instance })
    }

    #[cfg(test)]
    fn install_test_local_provider(
        &self,
        secret_key: &str,
    ) -> Result<crate::SessionAccount, crate::SessionMutationError> {
        let signer = nmp_local_signer::LocalKeySigner::parse(secret_key)
            .map_err(|_| crate::SessionMutationError::InvalidSecretKey)?;
        self.with_handle(|handle| handle.add_private_key_account(signer, false))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .map_err(session_mutation_from_add_signer)
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    #[doc(hidden)]
    pub fn install_test_signing_capability<Sig>(
        &self,
        signer: Sig,
    ) -> Result<SignerRegistration, EngineError>
    where
        Sig: nmp_signer::SigningCapability + Send + Sync + 'static,
    {
        self.with_handle(|handle| handle.add_signer(signer))?
            .map_err(EngineError::from_add_signer_error)
    }

    #[cfg(test)]
    fn select_test_account(&self, public_key: Option<PublicKey>) -> Result<(), EngineError> {
        match public_key {
            Some(public_key) => {
                self.add_public_key_account(public_key, false)
                    .map_err(|_| EngineError::EngineClosed)?;
                self.make_current_account(public_key)
                    .map_err(|_| EngineError::EngineClosed)
            }
            None => {
                self.with_handle(|handle| handle.set_current_account(None))?;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    fn test_current_public_key(&self) -> Result<Option<PublicKey>, EngineError> {
        Ok(self.session()?.current_pubkey)
    }
    /// Destructively remove one closed persistent engine store.
    ///
    /// This clears NMP's canonical events, pending writes, receipts,
    /// coverage/evidence, and all other state held in that store. It does not
    /// touch the app-owned opaque session payload, which is independent from
    /// the event store.
    /// A live engine in THIS OR ANY OTHER process using the same canonical
    /// path is refused with [`EngineError::StoreStillOpen`] without touching
    /// the file. Call [`Engine::shutdown`] (or drop the engine) first. A
    /// missing path is already reset and succeeds.
    pub fn reset_persistent_store(path: impl AsRef<std::path::Path>) -> Result<(), EngineError> {
        match RedbStore::reset(path) {
            Ok(()) => Ok(()),
            Err(RedbStoreResetError::StoreStillOpen { path }) => Err(EngineError::StoreStillOpen {
                path: path.to_string_lossy().into_owned(),
            }),
            Err(error) => Err(EngineError::StoreResetFailed {
                reason: error.to_string(),
            }),
        }
    }

    /// The ONE construction call: config -> store/routing-fact selection,
    /// router cap, everything `nmp-ffi` and
    /// `nmp-demo`'s hand-rolled assembly used to duplicate independently.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        Self::new_with_initial_session(config, crate::session::RestoredSession::empty())
    }

    fn new_with_initial_session(
        config: EngineConfig,
        initial_session: crate::session::RestoredSession,
    ) -> Result<Self, EngineError> {
        let routing_facts = build_routing_facts(&config)?;
        // #20: one effective ceiling is threaded to both the whole-demand
        // compiler and transport. EngineThread normalizes legacy zero to the
        // finite default and resolves any mechanism-level mismatch downward.
        let pool_config = PoolConfig {
            max_relays: config.max_relays,
            ..PoolConfig::default()
        };

        let runtime_config = RuntimeConfig {
            max_auth_capabilities: config.max_auth_capabilities,
            max_publish_attempts: config.max_publish_attempts,
            #[cfg(feature = "nip65")]
            nip65_sources: build_nip65_sources(&config)?,
        };
        let (engine_thread, handle) = match &config.store_path {
            Some(path) => {
                // Exhaustive on purpose (#920). A catch-all here is what
                // silently collapsed the one open refusal a fresh store
                // fixes into the family it must never be confused with, and
                // it would collapse the next new refusal the same way. Every
                // arm below is a decision someone made; adding a
                // `RedbStoreOpenError` variant now fails this build until
                // someone makes the next one.
                let store = RedbStore::open(path).map_err(|error| match error {
                    RedbStoreOpenError::StoreAlreadyOpen { path } => {
                        EngineError::StoreAlreadyOpen {
                            path: path.to_string_lossy().into_owned(),
                        }
                    }
                    RedbStoreOpenError::UnsupportedSchema {
                        path,
                        expected,
                        found,
                    } => EngineError::StoreUnsupportedSchema {
                        path: path.to_string_lossy().into_owned(),
                        expected,
                        found,
                    },
                    // The rest of the family shares ONE app-visible fact:
                    // this store must not be discarded in response. A
                    // refused lock, an unresolvable path, a target swapped
                    // mid-open, damaged current-epoch bytes — recreating the
                    // file fixes none of them, and doing it to the damaged
                    // case destroys the only copy of unsent writes. They are
                    // one variant because the branch is one branch, not
                    // because nobody looked.
                    error @ (RedbStoreOpenError::PathResolutionFailed { .. }
                    | RedbStoreOpenError::LockFileOpenFailed { .. }
                    | RedbStoreOpenError::LockFailed { .. }
                    | RedbStoreOpenError::TargetChanged { .. }
                    | RedbStoreOpenError::Database(_)) => EngineError::StoreOpenFailed {
                        reason: error.to_string(),
                    },
                })?;
                EngineThread::spawn_with_routing_facts_and_runtime_config(
                    store,
                    routing_facts,
                    config.max_relays,
                    pool_config,
                    runtime_config,
                    initial_session,
                )
                .map_err(EngineError::from_start_error)?
            }
            None => {
                let store = MemoryStore::new();
                EngineThread::spawn_with_routing_facts_and_runtime_config(
                    store,
                    routing_facts,
                    config.max_relays,
                    pool_config,
                    runtime_config,
                    initial_session,
                )
                .map_err(EngineError::from_start_error)?
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
    /// already-built store, bypassing `EngineConfig`
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
    pub fn from_parts<S>(store: S, cap: usize, pool_config: PoolConfig) -> Result<Self, EngineError>
    where
        S: nmp_store::EventStore + Send + 'static,
    {
        let (engine_thread, handle) =
            EngineThread::spawn(store, cap, pool_config).map_err(EngineError::from_start_error)?;
        Ok(Self {
            inner: Mutex::new(Some(Inner {
                handle,
                engine_thread,
            })),
        })
    }

    /// Static-fact variant of [`Self::from_parts`] for deterministic
    /// in-workspace falsifiers such as the scripted BDD harness.
    #[cfg(feature = "unstable-mechanism")]
    #[doc(hidden)]
    pub fn from_parts_with_fixture_routing_facts<S>(
        store: S,
        facts: nmp_router::FixtureRoutingFacts,
        cap: usize,
        pool_config: PoolConfig,
    ) -> Result<Self, EngineError>
    where
        S: nmp_store::EventStore + Send + 'static,
    {
        let (engine_thread, handle) =
            EngineThread::spawn_with_fixture_routing_facts(store, facts, cap, pool_config)
                .map_err(EngineError::from_start_error)?;
        Ok(Self {
            inner: Mutex::new(Some(Inner {
                handle,
                engine_thread,
            })),
        })
    }

    /// Feature-on scripted-harness variant that also supplies the exact
    /// operator sources owned by the concrete NIP-65 assembly.
    ///
    /// This remains a static-fixture/test door: applications use
    /// [`Self::new`] and [`EngineConfig`].
    #[cfg(all(feature = "unstable-mechanism", feature = "nip65"))]
    #[doc(hidden)]
    pub fn from_parts_with_fixture_routing_facts_and_nip65_sources<S>(
        store: S,
        facts: nmp_router::FixtureRoutingFacts,
        nip65_sources: Vec<RelayUrl>,
        cap: usize,
        pool_config: PoolConfig,
    ) -> Result<Self, EngineError>
    where
        S: nmp_store::EventStore + Send + 'static,
    {
        let runtime_config = RuntimeConfig {
            max_auth_capabilities: crate::runtime::DEFAULT_MAX_AUTH_CAPABILITIES,
            max_publish_attempts: crate::config::DEFAULT_MAX_PUBLISH_ATTEMPTS,
            nip65_sources,
        };
        let (engine_thread, handle) = EngineThread::spawn_with_routing_facts_and_runtime_config(
            store,
            crate::core::RoutingFactStore::from_fixture(facts),
            cap,
            pool_config,
            runtime_config,
            crate::session::RestoredSession::empty(),
        )
        .map_err(EngineError::from_start_error)?;
        Ok(Self {
            inner: Mutex::new(Some(Inner {
                handle,
                engine_thread,
            })),
        })
    }

    /// The read side of [`Self::from_parts`]'s hatch: the live `Handle` this
    /// engine drives, cloned out.
    ///
    /// Same gating and same justification as the constructor -- `#[doc(hidden)]`
    /// and behind `unstable-mechanism`, an in-workspace/test exception rather
    /// than an app contract. `nmp-bdd` needs it because it drives ONE engine
    /// through two surfaces at once: the product verbs a scenario is about
    /// (`Engine::publish`, and the group door built on it), and the raw delta /
    /// diagnostics channels a `Then` step has to FOLD to assert anything (see
    /// that crate's `world::observe`). Rebuilding those accumulators on top of
    /// `Subscription` would put the thing under test between the harness and
    /// its own witness.
    ///
    /// Escaping the serialized lifecycle gate is the cost: a `Handle` taken
    /// here outlives a later [`Self::shutdown`] and is the caller's to stop
    /// using. That is acceptable for a fixture that owns both ends and
    /// unacceptable for an app, which is exactly what the gate expresses.
    #[cfg(feature = "unstable-mechanism")]
    #[doc(hidden)]
    pub fn mechanism_handle(&self) -> Result<Handle, EngineError> {
        self.with_handle(Handle::clone)
    }

    /// The engine thread's wall clock, so a harness can state what time this
    /// engine is running at.
    ///
    /// Same gating and same justification as the two hatches above --
    /// `#[doc(hidden)]` and behind `unstable-mechanism`, in-workspace only.
    /// `nmp-bdd` needs it because `features/writes/` is written in sentences
    /// about a stated instant (*"Given my device clock reads ..."*, *"And 2
    /// seconds later ..."*) and `features/routing/` in sentences about time
    /// passing (*"And 30 days pass with nothing learned"*). Acceptance-time
    /// stamping and every deadline sweep are computed against the reducer's
    /// clock, and the reducer's clock is whatever the runtime last ticked it
    /// with -- so a spec that names an instant is unassertable without this.
    ///
    /// It is a REAL clock, not a stub: an engine whose clock is never set
    /// reads `Timestamp::now()` at exactly the sites it always did.
    #[cfg(feature = "unstable-mechanism")]
    #[doc(hidden)]
    pub fn clock(&self) -> Result<crate::mechanism::runtime::EngineClock, EngineError> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match &*guard {
            Some(inner) => Ok(inner.engine_thread.clock()),
            None => Err(EngineError::EngineClosed),
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

    /// #704: the engine-owned adapter runtime handle. Protocol adapters
    /// Optional protocol adapters spawn their async tasks here instead of
    /// reserving a slot on the deleted blocking-adapter executor. Hidden
    /// mechanism, not an app scheduling API — the runtime has no app-visible
    /// capacity, census, or admission, and observations never touch it (they
    /// are pure-waker async since #680).
    #[doc(hidden)]
    pub fn adapter_runtime(&self) -> Result<tokio::runtime::Handle, EngineError> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match &*guard {
            Some(inner) => Ok(inner.engine_thread.adapter_runtime()),
            None => Err(EngineError::EngineClosed),
        }
    }

    /// Noun 1: open a live query (#485). `window: None` ⇒ the unbounded delta
    /// observation (semantics unchanged from the pre-#485 `observe`).
    /// `Some(`[`Window::Expandable`]`)` ⇒ a bounded newest-first snapshot
    /// observation, growable via [`Subscription::request_rows`]. Delivery mode
    /// is DERIVED from boundedness (see [`crate::Subscription`]'s doc), never a
    /// separate knob. The returned [`Subscription`] withdraws itself on `Drop`.
    ///
    /// Windowed validation (typed on [`EngineError`], caught here BEFORE the
    /// engine is touched):
    /// - `initial > max` ⇒ [`EngineError::WindowInitialExceedsMax`].
    /// - a selection that already carries a NIP-01 `limit` ⇒
    ///   [`EngineError::WindowSelectionHasLimit`] (a window and a `limit` would
    ///   be two competing owners of row membership).
    ///
    /// Zero-sized windows are unrepresentable: [`Window::Expandable`] uses
    /// `NonZeroUsize`.
    pub fn observe(
        &self,
        query: LiveQuery,
        window: Option<Window>,
    ) -> Result<Subscription, EngineError> {
        self.subscribe_observation(query, window, Subscription::new, Subscription::new_windowed)
    }

    /// The pull-based async twin of [`Self::observe`] (#680): returns an
    /// [`AsyncSubscription`] whose `next()` is awaited rather than blocked on.
    /// Identical demand, validation, windowing, and withdrawal semantics — only
    /// the delivery wakeup differs (a waker, not a dedicated OS thread). This is
    /// what the FFI/SDK observation handles are built on, so opening one costs
    /// no native thread. Doc-hidden: it is the FFI/SDK delivery mechanism, not
    /// the documented direct-Rust product noun (which is blocking [`Self::observe`]).
    #[doc(hidden)]
    pub fn observe_async(
        &self,
        query: LiveQuery,
        window: Option<Window>,
    ) -> Result<AsyncSubscription, EngineError> {
        self.subscribe_observation(
            query,
            window,
            AsyncSubscription::new,
            AsyncSubscription::new_windowed,
        )
    }

    /// Shared validation + engine-subscribe for both the blocking and async
    /// observation surfaces (#680). The two closures select which wrapper
    /// (blocking `Subscription` vs `AsyncSubscription`) receives the raw engine
    /// handle + receiver, so the window/limit validation lives in exactly one
    /// place.
    fn subscribe_observation<T>(
        &self,
        query: LiveQuery,
        window: Option<Window>,
        unbounded: impl FnOnce(Handle, QueryHandle, RowsReceiver) -> T,
        windowed: impl FnOnce(Handle, HistoryHandle, HistoryReceiver) -> T,
    ) -> Result<T, EngineError> {
        match window {
            None => self
                .with_handle(|handle| {
                    handle
                        .subscribe(query)
                        .map(|(query_handle, rows)| unbounded(handle.clone(), query_handle, rows))
                })?
                .map_err(EngineError::from_observe_error),
            Some(Window::Expandable { initial, max }) => {
                if initial > max {
                    return Err(EngineError::WindowInitialExceedsMax {
                        initial: initial.get(),
                        max: max.get(),
                    });
                }
                if query
                    .branches()
                    .iter()
                    .any(|branch| branch.selection.limit.is_some())
                {
                    return Err(EngineError::WindowSelectionHasLimit);
                }
                // A window and an aggregate result limit are two competing
                // owners of the same row-membership count. Refuse before the
                // engine is touched, exactly as a branch selection limit is.
                if query.aggregate_result_limit().is_some() {
                    return Err(EngineError::WindowAggregateResultLimit);
                }
                let history_query = crate::core::HistoryQuery::new(query, initial.get(), max.get());
                self.with_handle(|handle| {
                    handle
                        .subscribe_history(history_query)
                        .map(|(history_handle, batches)| {
                            windowed(handle.clone(), history_handle, batches)
                        })
                })?
                .map_err(EngineError::from_observe_error)
            }
        }
    }

    /// Noun 2: enqueue a write -- the call itself never blocks on routing/
    /// wire/ack, but its return value is not fire-and-forget: the returned
    /// [`ReceiptStream`] is the caller's one way to observe how the intent
    /// resolved, and every `WriteFact` it ever reaches streams through it
    /// (ledger #9 -- enqueue is not converged). Returning `Ok` IS
    /// acceptance, so there is no acceptance fact on the stream. A tampered
    /// `WritePayload::Signed` cannot resolve, so it is refused by this call
    /// itself and nothing is taken into custody -- see this module's doc.
    ///
    /// The receipt carries the stable store-issued
    /// [`ReceiptId`](crate::ReceiptId) that process-later reattachment
    /// needs, AND the event id acceptance froze
    /// ([`ReceiptStream::event_id`]) — the write's identity from acceptance
    /// onward, post-restamp in every case, and the same value
    /// [`Self::publish_queue`] later reports for that receipt. One
    /// transaction decided both, so acceptance never hands back less than the
    /// whole receipt (#1314). Pre-acceptance correlation-id exhaustion
    /// returns a typed error without creating a receipt at all.
    ///
    /// Identity (#47): with [`crate::Identity::Active`] — the default — a builder
    /// payload signs as the current account, and fails closed pre-acceptance
    /// when there is no current account (nothing is pinned, so nothing may
    /// park). [`crate::Identity::Explicit`] is explicit per-write consent to
    /// publish as that key — whether or not it is current — without
    /// touching the current account: it works even while logged out, and
    /// acceptance pins the key so later [`Self::make_current_account`] calls
    /// cannot retarget the write. A named key with no available signing
    /// provider parks durably as
    /// [`SigningState::AwaitingSigner`](crate::SigningState) until that
    /// exact key's configured provider becomes available. On a `Signed` payload the author is
    /// already frozen in the bytes, so an explicit identity may only
    /// RESTATE it: naming anybody else cannot resolve, so this call refuses
    /// it and takes nothing into custody.
    pub fn publish(&self, intent: WriteIntent) -> Result<ReceiptStream, EngineError> {
        self.with_handle(|handle| handle.publish(intent))?
            .map_err(EngineError::from_publish_error)
    }

    /// Reattach to durable receipt facts after a restart. Missing ids and
    /// retained obligations with unreadable evidence are distinct outcomes.
    pub fn reattach_receipt(&self, id: ReceiptId) -> Result<ReceiptReattachment, EngineError> {
        self.with_handle(|handle| handle.reattach_receipt(id))
    }

    /// Reattach after a restart and wait for NMP's terminal publication
    /// result without exposing replay pages or fact reduction to the app.
    pub fn receipt_result(&self, id: ReceiptId) -> Result<ReceiptResult, ReceiptResultError> {
        match self.with_handle(|handle| handle.receipt_result(id)) {
            Ok(result) => result,
            Err(_) => Err(ReceiptResultError::ReplayUnavailable),
        }
    }

    #[doc(hidden)]
    pub fn reattach_receipt_from(
        &self,
        id: ReceiptId,
        cursor: ReceiptReplayCursor,
    ) -> Result<ReceiptReattachment, EngineError> {
        self.with_handle(|handle| handle.reattach_receipt_from(id, cursor))
    }

    /// #591: recover a receipt after a crash that happened BEFORE the app
    /// could durably persist the `ReceiptId` `publish` returned --
    /// looked up by the caller's own crash-safe correlation token instead.
    /// Otherwise identical to [`Self::reattach_receipt`].
    pub fn reattach_by_correlation(
        &self,
        token: String,
    ) -> Result<ReceiptReattachment, EngineError> {
        self.with_handle(|handle| handle.reattach_by_correlation(token))
    }

    /// Read one bounded page of the app's own publish queue (#903/#1039).
    ///
    /// Every write NMP still holds a receipt for, with what it knows about
    /// each one right now: signing state, the intended destination set and
    /// whether it is closed, per-relay state, the whole-write outcome if it
    /// has one, and any latched persistence fault.
    ///
    /// INSPECTION, never waiting. Nothing here blocks on settlement, and a
    /// locally accepted write is already visible through the app's own live
    /// query long before it appears here as settled.
    ///
    /// `after` is an exclusive stable receipt-id cursor. `limit` is a `u8`
    /// so one request can never materialize more than 255 complete entries.
    pub fn publish_queue(
        &self,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PublishQueueReadError> {
        self.with_handle(|handle| handle.publish_queue_entries(after, limit))
            .map_err(|_| PublishQueueReadError::EngineClosed)?
    }

    /// Reach the currently open write obligations for one event id (#903).
    ///
    /// A LiveQuery row already carries this id. The result contains no event
    /// content and no terminal receipt history: it is the exact join from
    /// that row to each active `ReceiptId`, whose retained-plus-live facts the
    /// app can observe with [`Self::reattach_receipt`]. More than one receipt
    /// can own identical event bytes, so the result is bounded and paged
    /// rather than choosing one and hiding the rest.
    pub fn publish_queue_for_event(
        &self,
        event_id: EventId,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PublishQueueReadError> {
        self.with_handle(|handle| handle.publish_queue_entries_for_event(event_id, after, limit))
            .map_err(|_| PublishQueueReadError::EngineClosed)?
    }

    /// Forget one queue entry (#1039).
    ///
    /// A real TERMINATION path: a write parked forever on a signer that
    /// never attached, and a permanently-failed refused entry, end no other
    /// way. An entry whose obligation is still open is refused — [`Self::cancel`]
    /// it first, then remove the terminal receipt cancellation leaves behind.
    /// That pair is the whole termination path for a signer-parked write:
    /// cancelling ends the obligation and compensates the optimistic row the
    /// write promised, and removal forgets the receipt.
    ///
    /// This does NOT close #46. Retained receipts and correlation tokens
    /// still regrow without bound; enumerating them is what makes the growth
    /// visible.
    pub fn remove_publish_queue_entry(&self, id: ReceiptId) -> Result<(), RemoveQueueEntryError> {
        self.with_handle(|handle| handle.remove_publish_queue_entry(id))
            .map_err(|_| RemoveQueueEntryError::EngineClosed)?
    }

    /// Explicitly cancel one accepted unsigned write by its stable receipt
    /// id. [`CancelWriteOutcome::Cancelled`] means the durable
    /// not-sent fact committed; signed or otherwise terminal receipts return
    /// a precise typed refusal.
    pub fn cancel(&self, id: ReceiptId) -> Result<CancelWriteOutcome, CancelWriteError> {
        self.with_handle(|handle| handle.cancel_write(id))
            .map_err(|_| CancelWriteError::EngineClosed)?
            .map(cancel_write_outcome_from_engine)
            .map_err(cancel_write_error_from_engine)
    }

    pub fn new_with_session(
        config: EngineConfig,
        payload: crate::SessionPayload,
    ) -> Result<Self, crate::SessionRestoreError> {
        let restored = crate::session::decode(&payload)?;
        let provider_count = restored.provider_count();
        if provider_count > config.max_auth_capabilities {
            return Err(crate::SessionRestoreError::CapabilityRegistryFull {
                limit: config.max_auth_capabilities,
            });
        }
        Self::new_with_initial_session(config, restored).map_err(|error| {
            crate::SessionRestoreError::EngineStartFailed {
                reason: error.to_string(),
            }
        })
    }

    pub fn session(&self) -> Result<crate::SessionSnapshot, EngineError> {
        self.with_handle(|handle| handle.session_snapshot())?
            .ok_or(EngineError::EngineClosed)
    }

    pub fn export_session(&self) -> Result<crate::SessionPayload, EngineError> {
        // The reducer returns only cloned provider owners and metadata. Secret
        // descriptor callbacks then run here, after the reducer command and
        // after the facade lifecycle lock have both been released.
        let handle = self.with_handle(Clone::clone)?;
        let export = handle
            .session_export_sources()
            .ok_or(EngineError::EngineClosed)?;
        let descriptors = export
            .providers
            .into_iter()
            .filter_map(|(public_key, provider)| {
                provider
                    .persistence_descriptor()
                    .map(|descriptor| (public_key, descriptor))
            })
            .collect();
        Ok(crate::session::encode(&export.snapshot, descriptors))
    }

    pub fn add_private_key_account(
        &self,
        secret_key: &[u8; 32],
        make_current: bool,
    ) -> Result<crate::SessionAccount, crate::SessionMutationError> {
        let signer = nmp_local_signer::LocalKeySigner::from_secret_bytes(secret_key)
            .map_err(|_| crate::SessionMutationError::InvalidSecretKey)?;
        let result = self
            .with_handle(|handle| handle.add_private_key_account(signer, make_current))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?;
        result.map_err(session_mutation_from_add_signer)
    }

    pub fn add_public_key_account(
        &self,
        public_key: PublicKey,
        make_current: bool,
    ) -> Result<crate::SessionAccount, crate::SessionMutationError> {
        self.with_handle(|handle| handle.add_public_key_account(public_key, make_current))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .ok_or(crate::SessionMutationError::EngineClosed)
    }

    pub fn make_current_account(
        &self,
        public_key: PublicKey,
    ) -> Result<(), crate::SessionMutationError> {
        let found = self
            .with_handle(|handle| handle.make_current_account(public_key))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .ok_or(crate::SessionMutationError::EngineClosed)?;
        found
            .then_some(())
            .ok_or(crate::SessionMutationError::AccountNotFound { public_key })
    }

    pub fn remove_account(
        &self,
        account: &crate::SessionAccount,
    ) -> Result<bool, crate::SessionMutationError> {
        self.with_handle(|handle| handle.remove_session_account(account.public_key))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .ok_or(crate::SessionMutationError::EngineClosed)
    }

    pub fn clear_session(&self) -> Result<(), crate::SessionMutationError> {
        self.with_handle(|handle| handle.clear_session())
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .ok_or(crate::SessionMutationError::EngineClosed)
    }

    /// Install the NIP-42 authorization policy for one exact account
    /// identity (#8). The engine consults it -- nonblocking, ready-or-
    /// pending -- every time a relay challenges a protected session frozen
    /// to `expected_public_key`; see [`AuthPolicy`]'s doc. Registering a
    /// policy for the same key again replaces it, invalidates the prior
    /// registration AND every in-flight decision bound to the prior
    /// capability instance, and never grants the stale registration cleanup
    /// authority over its replacement. Shares the finite
    /// [`EngineConfig::max_auth_capabilities`](crate::EngineConfig::max_auth_capabilities)
    /// ceiling with account/signer registrations.
    pub fn add_auth_policy<P>(
        &self,
        expected_public_key: PublicKey,
        policy: P,
    ) -> Result<AuthPolicyRegistration, EngineError>
    where
        P: AuthPolicy + 'static,
    {
        let registration = self.with_handle(|handle| {
            handle
                .add_auth_policy(expected_public_key, EngineAuthPolicyAdapter::new(policy))
                .map_err(EngineError::from_add_auth_policy_error)
        })??;
        Ok(AuthPolicyRegistration {
            inner: registration,
        })
    }

    /// Remove only the exact policy installation proven by `registration`.
    /// Pending decisions bound to it are cancelled and their sessions fail
    /// closed. Repeated or stale removal returns `Ok(false)` and cannot
    /// detach a replacement installed for the same key.
    pub fn remove_auth_policy(
        &self,
        registration: &AuthPolicyRegistration,
    ) -> Result<bool, EngineError> {
        self.with_handle(|handle| handle.remove_auth_policy(registration.inner.clone()))
    }

    /// Sign one immutable unsigned event through the current session
    /// account's configured provider and return the exact signed event.
    ///
    /// This is intentionally orthogonal to [`Self::publish`]: it creates no
    /// write intent, pending row, receipt, delivery lane, relay plan, or
    /// publication. The current author is frozen while the same lifecycle /
    /// identity lock is held, and the runtime validates the returned body,
    /// author, id, and signature before completion.
    pub fn sign_event(
        &self,
        request: SignEventRequest,
    ) -> Result<SignEventOperation, SignEventError> {
        let (handle, pubkey) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let inner = guard.as_ref().ok_or(SignEventError::EngineClosed)?;
            let pubkey = inner
                .handle
                .current_session_pubkey()
                .flatten()
                .ok_or(SignEventError::NoCurrentSigningProvider)?;
            (inner.handle.clone(), pubkey)
        };
        let unsigned = UnsignedEvent::new(
            pubkey,
            request.created_at,
            request.kind,
            request.tags,
            request.content,
        );
        handle.sign_event(unsigned)
    }

    /// Native callback adapter for [`Self::sign_event`]. The runtime owns
    /// both signer waiting and callback delivery on the operation's single
    /// admitted executor task, so an FFI caller does not need a second
    /// bridge slot.
    #[doc(hidden)]
    pub fn sign_event_with_completion(
        &self,
        request: SignEventRequest,
        completion: impl FnOnce(Result<nostr::Event, SignEventError>) + Send + 'static,
    ) -> Result<crate::runtime::SignEventCancel, SignEventError> {
        let (handle, pubkey) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let inner = guard.as_ref().ok_or(SignEventError::EngineClosed)?;
            let pubkey = inner
                .handle
                .current_session_pubkey()
                .flatten()
                .ok_or(SignEventError::NoCurrentSigningProvider)?;
            (inner.handle.clone(), pubkey)
        };
        let unsigned = UnsignedEvent::new(
            pubkey,
            request.created_at,
            request.kind,
            request.tags,
            request.content,
        );
        handle.sign_event_with_completion(unsigned, completion)
    }

    /// Open a live diagnostics stream. Same `Drop` discipline as
    /// [`Self::observe`] -- see [`DiagnosticsSubscription`]'s doc.
    pub fn observe_diagnostics(&self) -> Result<DiagnosticsSubscription, EngineError> {
        self.with_handle(|handle| {
            let (diag_handle, snapshots) = handle.observe_diagnostics();
            DiagnosticsSubscription::new(diag_handle, snapshots)
        })
    }

    /// The pull-based async twin of [`Self::observe_diagnostics`] (#680).
    /// Doc-hidden FFI/SDK delivery mechanism (see [`Self::observe_async`]).
    #[doc(hidden)]
    pub fn observe_diagnostics_async(&self) -> Result<AsyncDiagnosticsSubscription, EngineError> {
        self.with_handle(|handle| {
            let (diag_handle, snapshots) = handle.observe_diagnostics();
            AsyncDiagnosticsSubscription::new(diag_handle, snapshots)
        })
    }

    /// Acquire a relay's NIP-11 document once through the engine-owned,
    /// bounded, single-flight cache. This is intentionally not `observe_*`:
    /// NIP-11 is one HTTP representation, not a stream. Callers choose when
    /// to refresh; ordinary relay reconnects reuse the same freshness rules.
    pub async fn relay_information(
        &self,
        relay: &str,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationRequestError> {
        let relay = RelayUrl::parse(relay).map_err(|_| {
            RelayInformationRequestError::Engine(EngineError::InvalidRelayUrl {
                url: relay.to_string(),
            })
        })?;
        let handle = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            guard.as_ref().map(|inner| inner.handle.clone()).ok_or(
                RelayInformationRequestError::Engine(EngineError::EngineClosed),
            )?
        };
        handle
            .relay_information_async(relay, policy.into_engine())
            .await
            .map(RelayInformationSnapshot::from_engine)
            .map_err(|error| {
                RelayInformationRequestError::Acquisition(RelayInformationError::from_engine(error))
            })
    }

    #[cfg(test)]
    fn relay_information_retention_census(
        &self,
    ) -> crate::relay_information_service::RelayInformationRetentionCensus {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard
            .as_ref()
            .map(|inner| crate::runtime::relay_information_retention_census(&inner.handle))
            .expect("test census requires an open engine")
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
    use std::collections::BTreeSet;
    use std::future::Future;
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use super::*;
    use crate::publish_queue::{NotSentReason, SigningState, WriteFact, WriteOutcome};
    use crate::{Row, RowDelta, RowSignature};
    use nostr::Keys;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn private_key_bytes(keys: &Keys) -> [u8; 32] {
        keys.secret_key().to_secret_bytes()
    }

    fn receive_added_row(subscription: &Subscription, event_id: EventId) -> crate::Row {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "canonical row {event_id} did not arrive before the deadline"
            );
            let frame = subscription
                .recv_timeout(remaining)
                .expect("the canonical-row observation stays open");
            if let Some(row) = frame.deltas.into_iter().find_map(|delta| match delta {
                crate::RowDelta::Added(row) if row.id() == event_id => Some(row),
                _ => None,
            }) {
                return row;
            }
        }
    }

    #[test]
    fn whole_session_round_trip_is_canonical_and_restores_public_only_accounts() {
        let engine = Engine::new(EngineConfig::default()).expect("engine builds");
        let local = Keys::generate();
        let public_only = Keys::generate().public_key();
        engine
            .add_private_key_account(&private_key_bytes(&local), false)
            .expect("local account");
        engine
            .add_public_key_account(public_only, true)
            .expect("public-only account");
        let first = engine.export_session().expect("export");
        let first_bytes = first.as_bytes().to_vec();
        engine.shutdown();

        let restored = Engine::new_with_session(EngineConfig::default(), first)
            .expect("whole session restores");
        let snapshot = restored.session().expect("snapshot");
        assert_eq!(snapshot.current_pubkey, Some(public_only));
        assert_eq!(snapshot.accounts.len(), 2);
        assert!(snapshot.accounts.iter().any(|account| {
            account.public_key == public_only
                && account.provider.is_none()
                && account.signing == crate::SigningAvailability::Unsupported
        }));
        assert!(snapshot.accounts.iter().any(|account| {
            account.public_key == local.public_key()
                && account.provider == Some(crate::SessionProvider::LocalKey)
                && account.signing == crate::SigningAvailability::Available
        }));
        assert_eq!(
            restored.export_session().unwrap().as_bytes(),
            first_bytes.as_slice(),
            "canonical export is deterministic across restart"
        );
        restored.shutdown();
    }

    #[test]
    fn malformed_restore_creates_no_partially_visible_engine() {
        let malformed = crate::SessionPayload::from_bytes(b"not-a-session".to_vec());
        assert!(matches!(
            Engine::new_with_session(EngineConfig::default(), malformed),
            Err(crate::SessionRestoreError::MalformedPayload)
        ));
    }

    #[test]
    fn restored_session_is_installed_before_parked_write_recovery() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("session-before-recovery.redb");
        let keys = Keys::generate();
        let public_key = keys.public_key();
        let config = || EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        };

        let receipt_id = {
            let engine = Engine::new(config()).expect("persistent engine");
            engine
                .add_public_key_account(public_key, true)
                .expect("public-only current account");
            let receipt = engine
                .publish(WriteIntent {
                    payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                        kind: Kind::TextNote,
                        tags: Vec::new().into_iter().collect(),
                        content: "parked before restart".to_string(),
                        created_at: Some(Timestamp::from(55)),
                    }),
                    routing: nmp_grammar::WriteRouting::Explicit(vec![RelayUrl::parse(
                        "wss://session-recovery.example",
                    )
                    .unwrap()]),
                    identity: Identity::Active,
                    correlation: None,
                })
                .expect("accepted parked write");
            let parked = engine
                .publish_queue_for_event(receipt.event_id, None, 1)
                .unwrap();
            assert!(
                matches!(
                    parked[0].signing,
                    SigningState::AwaitingSigner { pubkey } if pubkey == public_key
                ),
                "expected parked obligation, got {:?}",
                parked[0].signing
            );
            engine.shutdown();
            receipt.id
        };

        let payload = {
            let engine = Engine::new(EngineConfig::default()).expect("payload engine");
            engine
                .add_private_key_account(&private_key_bytes(&keys), true)
                .expect("persistable local provider");
            let payload = engine.export_session().expect("session payload");
            engine.shutdown();
            payload
        };

        let restarted = Engine::new_with_session(config(), payload).expect("restored engine");
        let session = restarted.session().expect("restored metadata");
        assert_eq!(session.current_pubkey, Some(public_key));
        assert_eq!(session.accounts.len(), 1);
        assert_eq!(
            session.accounts[0].signing,
            crate::SigningAvailability::Available
        );
        let entry = restarted
            .publish_queue(None, 10)
            .expect("recovered queue")
            .into_iter()
            .find(|entry| entry.receipt_id == receipt_id)
            .expect("same accepted obligation");
        assert!(
            matches!(entry.signing, SigningState::Signed { .. }),
            "boot recovery must see the restored provider on its first turn: {:?}",
            entry.signing
        );
        restarted.shutdown();
    }

    #[test]
    fn remove_current_account_clears_current_in_same_runtime_turn() {
        let engine = Engine::new(EngineConfig::default()).expect("engine builds");
        let key = Keys::generate().public_key();
        let account = engine
            .add_public_key_account(key, true)
            .expect("account added and selected");
        assert!(engine.remove_account(&account).expect("remove"));
        let snapshot = engine.session().expect("snapshot");
        assert!(snapshot.accounts.is_empty());
        assert_eq!(snapshot.current_pubkey, None);
        engine.shutdown();
    }

    #[test]
    fn session_mutations_update_one_account_and_clear_the_whole_value() {
        let engine = Engine::new(EngineConfig::default()).expect("engine builds");
        let keys = Keys::generate();
        let public_key = keys.public_key();
        let public_only = engine
            .add_public_key_account(public_key, false)
            .expect("public-only account");
        assert_eq!(public_only.provider, None);
        assert_eq!(public_only.signing, crate::SigningAvailability::Unsupported);

        let enriched = engine
            .add_private_key_account(&private_key_bytes(&keys), true)
            .expect("same account gains local provider");
        assert_eq!(enriched.public_key, public_key);
        assert_eq!(enriched.provider, Some(crate::SessionProvider::LocalKey));
        let snapshot = engine.session().unwrap();
        assert_eq!(snapshot.accounts, vec![enriched]);
        assert_eq!(snapshot.current_pubkey, Some(public_key));

        engine.clear_session().expect("clear whole session");
        assert_eq!(
            engine.session().unwrap(),
            crate::SessionSnapshot {
                accounts: vec![],
                current_pubkey: None
            }
        );
        assert_eq!(
            engine.make_current_account(public_key),
            Err(crate::SessionMutationError::AccountNotFound { public_key })
        );
        engine.shutdown();
    }

    #[test]
    fn removing_or_clearing_session_never_retargets_or_discards_accepted_writes() {
        for clear in [false, true] {
            let engine = Engine::new(EngineConfig::default()).expect("engine builds");
            let public_key = Keys::generate().public_key();
            let account = engine
                .add_public_key_account(public_key, true)
                .expect("public-only current account");
            let query = || {
                LiveQuery::from_filter(nmp_grammar::Filter {
                    kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
                    authors: Some(nmp_grammar::Binding::Literal(BTreeSet::from([
                        public_key.to_hex()
                    ]))),
                    ..nmp_grammar::Filter::default()
                })
            };
            let before_observation = engine
                .observe(query(), None)
                .expect("author-and-kind-scoped observation opens");
            let receipt = engine
                .publish(WriteIntent {
                    payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                        kind: Kind::TextNote,
                        tags: Vec::new().into_iter().collect(),
                        content: "accepted before session mutation".to_string(),
                        created_at: Some(Timestamp::from(44)),
                    }),
                    routing: nmp_grammar::WriteRouting::Explicit(vec![RelayUrl::parse(
                        "wss://accepted.example",
                    )
                    .unwrap()]),
                    identity: Identity::Active,
                    correlation: None,
                })
                .expect("write accepted while signer is absent");
            let receipt_id = receipt.id;
            let frozen_event_id = receipt.event_id;
            drop(receipt.statuses);
            let row_before = receive_added_row(&before_observation, frozen_event_id);
            assert_eq!(row_before.id(), frozen_event_id);
            assert_eq!(row_before.pubkey(), public_key);
            assert_eq!(row_before.kind(), Kind::TextNote);
            assert_eq!(row_before.content(), "accepted before session mutation");
            assert_eq!(row_before.signature(), RowSignature::Pending);
            assert_eq!(row_before.signed_event(), None);
            drop(before_observation);
            let before = engine
                .publish_queue_for_event(frozen_event_id, None, 1)
                .unwrap();
            assert_eq!(before.len(), 1);
            assert_eq!(before[0].pubkey, public_key);
            assert_eq!(
                before[0].signing,
                SigningState::AwaitingSigner { pubkey: public_key }
            );

            if clear {
                engine.clear_session().expect("clear session");
            } else {
                assert!(engine.remove_account(&account).expect("remove account"));
            }

            assert!(engine.session().unwrap().accounts.is_empty());
            assert_eq!(engine.session().unwrap().current_pubkey, None);
            let after_observation = engine
                .observe(query(), None)
                .expect("fresh author-and-kind-scoped observation opens");
            let row_after = receive_added_row(&after_observation, frozen_event_id);
            assert_eq!(row_after.id(), frozen_event_id);
            assert_eq!(row_after.pubkey(), public_key);
            assert_eq!(row_after.kind(), Kind::TextNote);
            assert_eq!(row_after.content(), "accepted before session mutation");
            assert_eq!(row_after.signature(), RowSignature::Pending);
            assert_eq!(row_after.signed_event(), None);
            assert_eq!(
                row_after, row_before,
                "session mutation must preserve the exact canonical row"
            );
            drop(after_observation);
            let ReceiptReattachment::Attached { id, .. } = engine
                .reattach_receipt(receipt_id)
                .expect("reattach receipt")
            else {
                panic!("accepted receipt must remain reattachable after session mutation")
            };
            assert_eq!(id, receipt_id, "reattachment must retain receipt identity");
            let after = engine
                .publish_queue_for_event(frozen_event_id, None, 1)
                .unwrap();
            assert_eq!(after.len(), 1, "accepted receipt remains retained");
            assert_eq!(after[0].receipt_id, receipt_id);
            assert_eq!(after[0].event_id, frozen_event_id);
            assert_eq!(after[0].pubkey, public_key, "frozen author is unchanged");
            assert_eq!(
                after[0].signing,
                SigningState::AwaitingSigner { pubkey: public_key },
                "accepted write remains parked on its frozen author"
            );
            engine.shutdown();
        }
    }

    fn engine_with_store_and_lane_faults<S>(
        store: S,
        faults: crate::lane_fault_store::LaneFaults,
    ) -> Engine
    where
        S: nmp_store::EventStore + Send + 'static,
    {
        let store = crate::lane_fault_store::FaultyLaneStore::new(store, faults);
        let (engine_thread, handle) = EngineThread::spawn(store, 4, PoolConfig::default())
            .expect("fault-injecting engine construction");
        Engine {
            inner: Mutex::new(Some(Inner {
                handle,
                engine_thread,
            })),
        }
    }

    fn engine_with_lane_faults(faults: crate::lane_fault_store::LaneFaults) -> Engine {
        engine_with_store_and_lane_faults(MemoryStore::new(), faults)
    }

    #[test]
    fn persistent_engine_recovers_latched_store_and_resolves_ambiguous_acceptance_once() {
        use std::time::Duration;

        use crate::lane_fault_store::LaneFaults;
        use nmp_grammar::{Identity, WritePayload, WriteRouting};
        use nostr::EventBuilder;

        let faults = LaneFaults::default();
        faults.fail_reopen_attempts(2);
        let reopen_events = faults.fail_accept_after_commit_once();
        let persistent_fixture = tempfile::tempdir().expect("persistent store fixture");
        let store = RedbStore::open(persistent_fixture.path().join("ambiguous-acceptance.redb"))
            .expect("persistent fault-injection store must open");
        let engine = engine_with_store_and_lane_faults(store, faults.clone());

        let author = Keys::generate();
        engine
            .select_test_account(Some(author.public_key()))
            .expect("set facade-owned identity");
        let subscription = engine
            .observe(
                LiveQuery::from_filter(nmp_grammar::Filter {
                    kinds: Some(std::collections::BTreeSet::from([1])),
                    ..nmp_grammar::Filter::default()
                }),
                None,
            )
            .expect("open a query handle before the storage generation fails");
        let opening = subscription
            .recv_timeout(Duration::from_secs(5))
            .expect("a new observation receives its opening frame");
        assert!(opening.deltas.iter().all(|delta| delta.row().is_none()));
        let relay = RelayUrl::parse("wss://recovery.example").unwrap();
        let event = EventBuilder::text_note("ambiguous acceptance")
            .sign_with_keys(&author)
            .unwrap();
        let intent = || WriteIntent {
            payload: WritePayload::Signed(event.clone()),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: Some(
                nmp_grammar::CorrelationToken::try_from("recovery-correlation").unwrap(),
            ),
        };

        let first = engine.publish(intent());
        assert!(
            matches!(&first, Err(EngineError::PublishRefused { reason }) if reason.contains("injected acceptance committed before I/O failure")),
            "the uncertain boundary must not report acceptance: {}",
            first
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default()
        );

        assert!(
            !reopen_events.recv_timeout(Duration::from_secs(5)).unwrap(),
            "the first bounded reopen attempt remains unavailable"
        );
        assert!(
            !reopen_events.recv_timeout(Duration::from_secs(5)).unwrap(),
            "the second bounded reopen attempt remains unavailable"
        );
        assert!(
            reopen_events.recv_timeout(Duration::from_secs(5)).unwrap(),
            "the supervisor reconstructs without replacing the Engine"
        );
        let recovered_frame = subscription
            .recv_timeout(Duration::from_secs(5))
            .expect("the pre-failure query handle receives reconstructed rows");
        let recovered_pending = recovered_frame
            .deltas
            .iter()
            .filter_map(RowDelta::row)
            .find(|row| row.id() == event.id)
            .expect("the existing query handle receives the committed boundary row");
        assert_eq!(recovered_pending.signature(), RowSignature::Pending);
        assert!(
            recovered_pending.signed_event().is_none(),
            "the ambiguous boundary committed the frozen body, not signature promotion"
        );

        assert_eq!(
            engine.test_current_public_key().unwrap(),
            Some(author.public_key()),
            "facade identity survives the internal store generation change"
        );
        let divergent = EventBuilder::text_note("different signed body")
            .sign_with_keys(&author)
            .unwrap();
        let invalid_signature = divergent.sig;
        let divergent_retry = engine
            .publish(WriteIntent {
                payload: WritePayload::Signed(divergent),
                routing: WriteRouting::Explicit(vec![relay.clone()]),
                identity: Identity::Active,
                correlation: Some(
                    nmp_grammar::CorrelationToken::try_from("recovery-correlation").unwrap(),
                ),
            })
            .expect("a divergent retry only reattaches the retained receipt");

        let mut invalid = event.clone();
        invalid.sig = invalid_signature;
        let invalid_retry = engine
            .publish(WriteIntent {
                payload: WritePayload::Signed(invalid),
                routing: WriteRouting::Explicit(vec![relay.clone()]),
                identity: Identity::Active,
                correlation: Some(
                    nmp_grammar::CorrelationToken::try_from("recovery-correlation").unwrap(),
                ),
            })
            .expect("an invalid retry only reattaches the retained receipt");
        assert_eq!(invalid_retry.id, divergent_retry.id);
        assert!(
            subscription
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "divergent and invalid retries must not promote or replace the pending row"
        );

        let exact_retry = engine
            .publish(intent())
            .expect("the exact signed retry reuses and promotes the recovered receipt");
        assert_eq!(exact_retry.id, divergent_retry.id);
        let promoted_frame = subscription
            .recv_timeout(Duration::from_secs(5))
            .expect("the exact signed retry promotes the recovered row");
        assert!(
            promoted_frame
                .deltas
                .iter()
                .filter_map(RowDelta::row)
                .filter_map(Row::signed_event)
                .any(|promoted| promoted.id == event.id && promoted.sig == event.sig),
            "the same recovered row is promoted with the exact supplied signature"
        );
        let repeated = engine
            .publish(intent())
            .expect("repeating the exact correlation remains idempotent");
        assert_eq!(exact_retry.id, repeated.id);
        assert_eq!(exact_retry.event_id, repeated.event_id);
        assert_eq!(
            engine.publish_queue(None, u8::MAX).unwrap().len(),
            1,
            "ambiguous acceptance and every same-correlation retry retain exactly one receipt"
        );

        let later_event = EventBuilder::text_note("accepted after reconstruction")
            .sign_with_keys(&author)
            .unwrap();
        engine
            .publish(WriteIntent {
                payload: WritePayload::Signed(later_event),
                routing: WriteRouting::Explicit(vec![relay.clone()]),
                identity: Identity::Active,
                correlation: Some(
                    nmp_grammar::CorrelationToken::try_from("post-recovery-correlation").unwrap(),
                ),
            })
            .expect("later independent work is accepted by the reconstructed Engine");

        // Exercise the other honest I/O boundary in the same public Engine:
        // this transaction is absent rather than committed-but-errored.
        faults.fail_reopen_attempts(1);
        let absent_reopen_events = faults.fail_accept_before_commit_once();
        let absent_event = EventBuilder::text_note("absent boundary acceptance")
            .sign_with_keys(&author)
            .unwrap();
        let absent_intent = || WriteIntent {
            payload: WritePayload::Signed(absent_event.clone()),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: Some(
                nmp_grammar::CorrelationToken::try_from("absent-recovery-correlation").unwrap(),
            ),
        };
        let absent_first = engine.publish(absent_intent());
        assert!(
            matches!(&absent_first, Err(EngineError::PublishRefused { reason }) if reason.contains("failed before commit")),
            "an absent I/O boundary must not report acceptance: {}",
            absent_first
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default()
        );
        assert!(!absent_reopen_events
            .recv_timeout(Duration::from_secs(5))
            .unwrap());
        assert!(absent_reopen_events
            .recv_timeout(Duration::from_secs(5))
            .unwrap());
        let absent_recovered = engine
            .publish(absent_intent())
            .expect("an absent transaction can be accepted once after reconstruction");
        assert_eq!(absent_recovered.event_id, absent_event.id);
        assert_eq!(engine.publish_queue(None, u8::MAX).unwrap().len(), 3);

        engine.shutdown();
    }

    #[test]
    fn persistent_engine_does_not_reconstruct_for_an_invariant_fault() {
        use crate::lane_fault_store::LaneFaults;
        use nmp_grammar::{Identity, WritePayload, WriteRouting};
        use nostr::EventBuilder;

        let faults = LaneFaults::default();
        faults.fail_accept_with_invariant_once();
        let engine = engine_with_lane_faults(faults.clone());
        let author = Keys::generate();
        engine
            .select_test_account(Some(author.public_key()))
            .expect("set facade-owned identity");
        let relay = RelayUrl::parse("wss://invariant.example").unwrap();
        let intent = |content: &str| WriteIntent {
            payload: WritePayload::Signed(
                EventBuilder::text_note(content)
                    .sign_with_keys(&author)
                    .unwrap(),
            ),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        };

        let refused = engine.publish(intent("invariant refusal"));
        assert!(
            matches!(&refused, Err(EngineError::PublishRefused { reason }) if reason.contains("non-reopenable acceptance invariant"))
        );
        engine
            .publish(intent("ordinary next write"))
            .expect("a non-reopenable refusal does not replace the healthy store handle");
        assert_eq!(faults.reopen_attempt_count(), 0);
        assert_eq!(engine.publish_queue(None, u8::MAX).unwrap().len(), 1);

        engine.shutdown();
    }

    fn signer_public_key(public_key: PublicKey) -> nmp_signer::SignerPublicKey {
        nmp_signer::SignerPublicKey::new(public_key.to_bytes())
    }

    fn signer_unsigned_to_nostr(unsigned: nmp_signer::SignerUnsignedEvent) -> nostr::UnsignedEvent {
        let (public_key, created_at, kind, tags, content) = unsigned.into_parts();
        nostr::UnsignedEvent::new(
            PublicKey::from_slice(public_key.as_bytes()).unwrap(),
            Timestamp::from(created_at),
            Kind::from(kind),
            tags.into_iter()
                .map(nostr::Tag::parse)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            content,
        )
    }

    fn nostr_signed_to_signer(event: nostr::Event) -> nmp_signer::SignerSignedEvent {
        nmp_signer::SignerSignedEvent::new(
            event.id.to_bytes(),
            signer_public_key(event.pubkey),
            event.created_at.as_secs(),
            event.kind.as_u16(),
            event
                .tags
                .to_vec()
                .into_iter()
                .map(nostr::Tag::to_vec)
                .collect(),
            event.content,
            event.sig.serialize(),
        )
    }

    struct ThreadWake(std::thread::Thread);

    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    fn evidence_attribute<'a>(
        evidence: &'a crate::ObservationEvidence,
        key: &str,
    ) -> Option<&'a str> {
        evidence
            .attributes
            .iter()
            .find_map(|(candidate, value)| (candidate == key).then_some(value.as_str()))
    }

    #[test]
    fn loopback_relay_reaches_the_facade_transport_pool_without_opt_in() {
        use std::collections::BTreeSet;
        use std::time::{Duration, Instant};

        use nostr::filter::MatchEventOptions;
        use nostr::{ClientMessage, EventBuilder, JsonUtil, RelayMessage};
        use tungstenite::Message;

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind the intentional local relay");
        let relay_address = listener.local_addr().expect("read local relay address");
        let relay =
            RelayUrl::parse(&format!("ws://{relay_address}")).expect("parse local relay URL");
        let author = Keys::generate();
        let event = EventBuilder::text_note("facade local relay proof")
            .sign_with_keys(&author)
            .expect("sign relay fixture");
        let expected_id = event.id;

        let relay_thread = std::thread::spawn({
            let event = event.clone();
            move || {
                let (stream, _) = listener.accept().expect("accept facade connection");
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("bound relay read");
                let mut socket = tungstenite::accept(stream).expect("accept WebSocket");
                while let Ok(message) = socket.read() {
                    let Message::Text(text) = message else {
                        continue;
                    };
                    let Ok(ClientMessage::Req {
                        subscription_id,
                        filters,
                    }) = ClientMessage::from_json(text.as_str())
                    else {
                        continue;
                    };
                    if !filters.into_iter().any(|filter| {
                        filter
                            .into_owned()
                            .match_event(&event, MatchEventOptions::new())
                    }) {
                        continue;
                    }
                    socket
                        .send(Message::text(
                            RelayMessage::event(subscription_id.clone().into_owned(), event)
                                .as_json(),
                        ))
                        .expect("send matching event");
                    socket
                        .send(Message::text(
                            RelayMessage::eose(subscription_id.into_owned()).as_json(),
                        ))
                        .expect("send EOSE");
                    socket.flush().expect("flush relay frames");
                    while socket.read().is_ok() {}
                    return;
                }
                panic!("facade connection ended before a REQ reached the local relay");
            }
        });

        let engine = Engine::new(EngineConfig {
            app_relays: vec![relay.to_string()],
            ..EngineConfig::default()
        })
        .expect("loopback relay must build without opt-in");
        let query = LiveQuery::single(
            crate::Demand::new(
                crate::Filter {
                    kinds: Some(BTreeSet::from([1])),
                    authors: Some(crate::Binding::Literal(BTreeSet::from([author
                        .public_key()
                        .to_hex()]))),
                    ..crate::Filter::default()
                },
                crate::SourceAuthority::Pinned(BTreeSet::from([relay])),
                crate::AccessContext::Public,
            )
            .expect("build pinned local-relay demand"),
        );
        let subscription = engine
            .observe(query, None)
            .expect("observe through supported facade");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut found = false;
        let mut execution = Vec::new();
        while (!found
            || !execution.iter().any(|fact: &crate::ObservationEvidence| {
                fact.kind == "request_settled"
                    && evidence_attribute(fact, "terminal") == Some("eose")
            }))
            && Instant::now() < deadline
        {
            if let Ok(frame) = subscription.recv_timeout(Duration::from_millis(250)) {
                found = frame
                    .deltas
                    .iter()
                    .filter_map(|delta| delta.row().and_then(|row| row.signed_event()))
                    .any(|received| received.id == expected_id)
                    || found;
                execution.extend(frame.execution);
            }
        }

        subscription.cancel();
        engine.shutdown();
        if !found {
            // Unblock `accept` when the regression under test prevents the
            // engine from dialing at all, so failure stays bounded.
            let _ = std::net::TcpStream::connect(relay_address);
        }
        let relay_result = relay_thread.join();
        assert!(found, "loopback relay never reached the facade query");
        assert!(execution.iter().any(|fact| {
            fact.kind == "concrete_filter"
                && fact.path.as_deref() == Some("$")
                && fact.revision == Some(1)
        }));
        let requests: BTreeSet<_> = execution
            .iter()
            .filter(|fact| fact.kind == "relay_request" && fact.path.as_deref() == Some("$"))
            .map(|fact| {
                (
                    fact.revision.expect("request filter revision"),
                    evidence_attribute(fact, "transport_generation")
                        .expect("request transport generation")
                        .parse::<u64>()
                        .expect("numeric transport generation"),
                    evidence_attribute(fact, "request_revision")
                        .expect("request revision")
                        .parse::<u64>()
                        .expect("numeric request revision"),
                )
            })
            .collect();
        assert!(
            !requests.is_empty(),
            "facade frame must expose an actual REQ handoff"
        );
        assert!(
            execution.iter().any(|fact| {
                fact.kind == "request_settled"
                    && fact.path.as_deref() == Some("$")
                    && evidence_attribute(fact, "terminal") == Some("eose")
                    && requests.contains(&(
                        fact.revision.expect("EOSE filter revision"),
                        evidence_attribute(fact, "transport_generation")
                            .expect("EOSE transport generation")
                            .parse::<u64>()
                            .expect("numeric transport generation"),
                        evidence_attribute(fact, "request_revision")
                            .expect("EOSE request revision")
                            .parse::<u64>()
                            .expect("numeric request revision"),
                    ))
            }),
            "EOSE must identify the exact accepted REQ: {execution:#?}"
        );
        relay_result.expect("join local relay");
    }

    #[test]
    fn persistent_store_reset_is_destructive_and_idempotent() {
        let fixture = tempfile::tempdir().expect("temporary directory");
        let path = fixture.path().join("nmp.redb");
        let config = EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        };

        let engine = Engine::new(config.clone()).expect("persistent engine must build");
        assert!(
            path.exists(),
            "opening the persistent engine creates its store"
        );
        let before = std::fs::read(&path).expect("live store bytes must be readable");
        let alias = fixture.path().join(".").join("nmp.redb");
        let refusal = Engine::reset_persistent_store(&alias)
            .expect_err("a canonical alias of a live store must refuse reset");
        assert_eq!(
            refusal,
            EngineError::StoreStillOpen {
                path: path
                    .canonicalize()
                    .expect("live store path must canonicalize")
                    .to_string_lossy()
                    .into_owned(),
            }
        );
        assert_eq!(
            std::fs::read(&path).expect("refused reset must leave the store readable"),
            before,
            "refused reset must not touch the live store file"
        );
        let hard_link = fixture.path().join("nmp-hard-link.redb");
        std::fs::hard_link(&path, &hard_link).expect("hard-link alias must be created");
        let hard_link_refusal = Engine::reset_persistent_store(&hard_link)
            .expect_err("a hard-link alias of a live store must refuse reset");
        assert_eq!(
            hard_link_refusal,
            EngineError::StoreStillOpen {
                path: hard_link
                    .canonicalize()
                    .expect("hard-link path must canonicalize")
                    .to_string_lossy()
                    .into_owned(),
            }
        );
        assert_eq!(
            std::fs::read(&path).expect("hard-link refusal must preserve the original name"),
            before
        );
        assert_eq!(
            std::fs::read(&hard_link).expect("hard-link refusal must preserve the alias"),
            before
        );
        let second_open = Engine::new(config.clone())
            .err()
            .expect("a second persistent engine owner must be refused");
        assert_eq!(
            second_open,
            EngineError::StoreAlreadyOpen {
                path: path
                    .canonicalize()
                    .expect("live store path must canonicalize")
                    .to_string_lossy()
                    .into_owned(),
            }
        );

        engine.shutdown();

        let after_shutdown =
            std::fs::read(&path).expect("shutdown store bytes must remain readable");
        assert_eq!(
            std::fs::read(&hard_link).expect("hard-link alias must match the store after shutdown"),
            after_shutdown
        );
        assert!(matches!(
            Engine::reset_persistent_store(&hard_link),
            Err(EngineError::StoreResetFailed { reason })
                if reason.contains("2 hard links")
        ));
        assert_eq!(
            std::fs::read(&path).expect("multi-link refusal must preserve the original name"),
            after_shutdown
        );
        assert_eq!(
            std::fs::read(&hard_link).expect("multi-link refusal must preserve the alias"),
            after_shutdown
        );
        std::fs::remove_file(&hard_link).expect("restore the single-link reset precondition");
        Engine::reset_persistent_store(&path).expect("a closed store must reset");
        assert!(
            !path.exists(),
            "reset must remove the complete canonical store"
        );
        Engine::reset_persistent_store(&path).expect("a missing store is already reset");

        let reopened = Engine::new(config).expect("reset path must open as a fresh store");
        drop(reopened);
        Engine::reset_persistent_store(&path)
            .expect("dropping an engine must release its store ownership");
    }

    #[test]
    fn failed_persistent_store_open_releases_reset_guard() {
        let fixture = tempfile::tempdir().expect("temporary directory");
        let path = fixture.path().join("corrupt.redb");
        std::fs::write(&path, b"not a redb database").expect("corrupt fixture must write");
        let error = Engine::new(EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .err()
        .expect("corrupt store must fail construction");
        assert!(matches!(error, EngineError::StoreOpenFailed { .. }));

        Engine::reset_persistent_store(&path)
            .expect("failed construction must release its store ownership");
        assert!(!path.exists(), "reset must remove the failed-open store");
    }

    /// #920: the two open refusals an app must never confuse. A store from a
    /// superseded epoch is recoverable and the recovery is to discard the
    /// file; damaged current-epoch bytes are not, and discarding them
    /// destroys the only copy of accepted-but-unpublished writes.
    ///
    /// The epoch fixture is the shape a real store hit: a marker written at
    /// the address a superseded epoch owned (`schema_meta_v6`/`version`),
    /// which this build cannot read, so `found` is `None` — "not this
    /// epoch", not "no data". That fixture is exactly what made the refusal
    /// text ("predates the schema marker") read as indistinguishable from an
    /// unreadable file, and it is why the branch has to be a type.
    #[test]
    fn superseded_epoch_and_damaged_bytes_are_different_typed_open_refusals() {
        use redb::{Database, TableDefinition};

        let fixture = tempfile::tempdir().expect("temporary directory");

        let superseded = fixture.path().join("superseded-epoch.redb");
        {
            let database = Database::create(&superseded).expect("epoch fixture must create");
            let write = database.begin_write().expect("epoch fixture must begin");
            {
                let mut marker = write
                    .open_table(TableDefinition::<&str, u64>::new("schema_meta_v6"))
                    .expect("epoch fixture must open its retired marker table");
                marker.insert("version", 10u64).expect("marker must insert");
            }
            write.commit().expect("epoch fixture must commit");
        }
        let error = Engine::new(EngineConfig {
            store_path: Some(superseded.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .err()
        .expect("a superseded-epoch store must refuse construction");
        let expected = match &error {
            EngineError::StoreUnsupportedSchema {
                path,
                expected,
                found,
            } => {
                assert!(
                    path.ends_with("superseded-epoch.redb"),
                    "the refusal must name the store an app would discard: {path}"
                );
                assert_eq!(
                    *found, None,
                    "a marker this build cannot read is absent, not a different number"
                );
                *expected
            }
            other => panic!(
                "a superseded epoch must not collapse into a generic open failure: {other:?}"
            ),
        };
        assert!(expected > 0, "the build's own epoch must be reported");

        // The operator contract (#1017) has to survive the promotion to this
        // boundary, because this is where an app's operator reads it.
        let rendered = error.to_string();
        for required in [
            "discard and recreate this store to continue",
            "NMP can reacquire the relay-backed read cache",
            "accepted but unpublished writes",
            "permanently lost",
        ] {
            assert!(
                rendered.contains(required),
                "the reachable refusal must state {required:?}: {rendered}"
            );
        }

        // The variant tells an app the discard is correct; it is worth
        // nothing if the refused open still owns the file. This is the exact
        // sequence a consumer runs after branching on it.
        Engine::reset_persistent_store(&superseded)
            .expect("the epoch refusal must release its store ownership");
        assert!(
            !superseded.exists(),
            "the discard an app is told to perform must actually be performable"
        );
        Engine::new(EngineConfig {
            store_path: Some(superseded.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .expect("a recreated store must open as the current epoch")
        .shutdown();

        let damaged = fixture.path().join("damaged.redb");
        std::fs::write(&damaged, b"not a redb database").expect("damaged fixture must write");
        let refusal = Engine::new(EngineConfig {
            store_path: Some(damaged.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .err()
        .expect("damaged bytes must refuse construction");
        assert!(
            matches!(refusal, EngineError::StoreOpenFailed { .. }),
            "damaged bytes must never be reported as a discardable epoch: {refusal:?}"
        );
        assert!(
            !refusal.to_string().contains("discard and recreate"),
            "no open refusal but the epoch one may tell an operator to discard: {refusal}"
        );
    }

    #[test]
    fn facade_cancellation_is_typed_idempotent_and_reattachable() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        engine
            .select_test_account(Some(keys.public_key()))
            .expect("engine open");
        let receipt = engine
            .publish(WriteIntent {
                payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::TextNote,
                    tags: (Vec::new()).into_iter().collect(),
                    content: ("cancel through facade").into(),
                    created_at: Some(Timestamp::from(10)),
                }),
                routing: nmp_grammar::WriteRouting::Auto,
                identity: Identity::Active,
                correlation: None,
            })
            .expect("accept write");
        // `publish` returning `Ok` IS acceptance -- there is no
        // acceptance fact to wait for on the stream.

        assert_eq!(engine.cancel(receipt.id), Ok(CancelWriteOutcome::Cancelled));
        let mut saw_cancelled = false;
        while let Ok(status) = receipt
            .statuses
            .recv_timeout(std::time::Duration::from_secs(1))
        {
            if status == WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled)) {
                saw_cancelled = true;
                break;
            }
        }
        assert!(saw_cancelled);
        assert_eq!(engine.cancel(receipt.id), Ok(CancelWriteOutcome::Cancelled));

        let ReceiptReattachment::Attached {
            statuses: replay, ..
        } = engine.reattach_receipt(receipt.id).unwrap()
        else {
            panic!("cancelled receipt must remain reattachable")
        };
        assert_eq!(
            replay.recv().unwrap(),
            WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled))
        );
        assert!(matches!(
            engine.cancel(ReceiptId(u64::MAX)),
            Err(CancelWriteError::UnknownReceipt { .. })
        ));

        engine.shutdown();
        assert_eq!(
            engine.cancel(receipt.id),
            Err(CancelWriteError::EngineClosed)
        );
    }

    #[test]
    fn dropping_a_receipt_observer_does_not_cancel_the_write() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        engine
            .select_test_account(Some(keys.public_key()))
            .expect("engine open");
        let receipt = engine
            .publish(WriteIntent {
                payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::TextNote,
                    tags: (Vec::new()).into_iter().collect(),
                    content: ("observer lifetime is not write ownership").into(),
                    created_at: Some(Timestamp::from(11)),
                }),
                routing: nmp_grammar::WriteRouting::Auto,
                identity: Identity::Active,
                correlation: None,
            })
            .expect("accept write");
        let receipt_id = receipt.id;
        drop(receipt.statuses);

        let ReceiptReattachment::Attached { .. } = engine.reattach_receipt(receipt_id).unwrap()
        else {
            panic!("dropping the observer must not remove the receipt")
        };
        assert_eq!(engine.cancel(receipt_id), Ok(CancelWriteOutcome::Cancelled));
        engine.shutdown();
    }

    #[cfg(feature = "unstable-mechanism")]
    #[test]
    fn from_parts_cannot_bypass_guard_and_spawn_failure_releases_store() {
        let fixture = tempfile::tempdir().expect("temporary directory");
        let path = fixture.path().join("from-parts.redb");
        let store = RedbStore::open(&path).expect("store must open");
        let engine = Engine::from_parts(store, 10, PoolConfig::default())
            .expect("from_parts engine must build");
        assert!(matches!(
            Engine::reset_persistent_store(&path),
            Err(EngineError::StoreStillOpen { .. })
        ));
        engine.shutdown();
        Engine::reset_persistent_store(&path)
            .expect("from_parts shutdown must release store ownership");

        let store = RedbStore::open(&path).expect("store must reopen");
        let failure = Engine::from_parts(
            store,
            usize::MAX,
            PoolConfig {
                max_relays: usize::MAX,
                ..PoolConfig::default()
            },
        )
        .err()
        .expect("unrepresentable relay envelope must refuse construction");
        assert!(matches!(failure, EngineError::EngineStartFailed { .. }));
        Engine::reset_persistent_store(&path)
            .expect("post-open spawn failure must release RedbStore ownership");
    }

    #[test]
    fn sign_event_returns_exact_verified_event_without_store_or_publish_queue_residue() {
        use nmp_store::EventStore;

        let fixture = tempfile::tempdir().expect("temporary directory");
        let path = fixture.path().join("sign-only.redb");
        let engine = Engine::new(EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .expect("engine must build");
        let secret = format!("{:064x}", 7u8);
        let author = engine
            .install_test_local_provider(&secret)
            .expect("account must register")
            .public_key();
        engine
            .select_test_account(Some(author))
            .expect("account must activate");
        let request = SignEventRequest {
            created_at: nostr::Timestamp::from(1_723_456_789),
            kind: nostr::Kind::Custom(27_272),
            tags: vec![nostr::Tag::parse(vec!["t".to_string(), "sign-only".to_string()]).unwrap()],
            content: "exact body".to_string(),
        };

        let signed = engine
            .sign_event(request.clone())
            .expect("sign-only operation must start")
            .recv()
            .expect("current account's local signing provider must complete");
        assert_eq!(signed.pubkey, author);
        assert_eq!(signed.created_at, request.created_at);
        assert_eq!(signed.kind, request.kind);
        assert_eq!(
            signed.tags.iter().cloned().collect::<Vec<_>>(),
            request.tags
        );
        assert_eq!(signed.content, request.content);
        signed.verify().expect("returned signature must verify");
        engine.shutdown();

        let store = nmp_store::RedbStore::open(&path).expect("store must reopen");
        assert!(
            store
                .query(&nostr::Filter::new())
                .expect("canonical query must succeed")
                .is_empty(),
            "sign-only must not create a canonical row"
        );
        assert!(
            store
                .recover_publish_queue()
                .expect("recover delivery")
                .is_empty(),
            "sign-only must not create an intent, receipt, or delivery lane"
        );
    }

    #[test]
    fn sign_event_rejects_missing_current_account_or_provider_before_invocation() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let active = nostr::Keys::generate().public_key();
        let request = SignEventRequest {
            created_at: nostr::Timestamp::from(1),
            kind: nostr::Kind::TextNote,
            tags: Vec::new(),
            content: "body".to_string(),
        };
        match engine.sign_event(request.clone()) {
            Err(error) => assert_eq!(error, SignEventError::NoCurrentSigningProvider),
            Ok(_) => panic!("a missing current account must refuse before acceptance"),
        }
        engine.select_test_account(Some(active)).unwrap();
        match engine.sign_event(request) {
            Err(error) => assert_eq!(error, SignEventError::NoCurrentSigningProvider),
            Ok(_) => panic!("an unavailable signing provider must refuse before acceptance"),
        }
        engine.shutdown();
    }

    struct MismatchedSigner {
        reported: PublicKey,
        actual: Keys,
        calls: Arc<AtomicUsize>,
    }

    impl nmp_signer::SigningCapability for MismatchedSigner {
        fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
            Some(signer_public_key(self.reported))
        }

        fn sign(
            &self,
            unsigned: nmp_signer::SignerUnsignedEvent,
        ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let unsigned = signer_unsigned_to_nostr(unsigned);
            let substituted = nostr::UnsignedEvent::new(
                self.actual.public_key(),
                unsigned.created_at,
                unsigned.kind,
                unsigned.tags,
                unsigned.content,
            );
            nmp_signer::SignerOp::ok(nostr_signed_to_signer(
                substituted.sign_with_keys(&self.actual).unwrap(),
            ))
        }
    }

    #[test]
    fn sign_event_rejects_mismatched_signer_output() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let reported = nostr::Keys::generate();
        let calls = Arc::new(AtomicUsize::new(0));
        engine
            .install_test_signing_capability(MismatchedSigner {
                reported: reported.public_key(),
                actual: nostr::Keys::generate(),
                calls: Arc::clone(&calls),
            })
            .expect("signer must register");
        engine
            .select_test_account(Some(reported.public_key()))
            .unwrap();
        let request = SignEventRequest {
            created_at: nostr::Timestamp::from(2),
            kind: nostr::Kind::TextNote,
            tags: Vec::new(),
            content: "frozen".to_string(),
        };
        assert!(matches!(
            engine.sign_event(request).unwrap().recv(),
            Err(SignEventError::InvalidSignerOutput { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    struct PendingSigner {
        public_key: PublicKey,
        cancellations: Arc<AtomicUsize>,
    }

    struct NoHookPendingSigner {
        public_key: PublicKey,
        operation: Mutex<Option<nmp_signer::SignerOp<nmp_signer::SignerSignedEvent>>>,
    }

    impl nmp_signer::SigningCapability for NoHookPendingSigner {
        fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
            Some(signer_public_key(self.public_key))
        }

        fn sign(
            &self,
            _unsigned: nmp_signer::SignerUnsignedEvent,
        ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
            self.operation
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
                .expect("fixture signs once")
        }
    }

    struct HookCompletesSigner {
        keys: Keys,
        cancellations: Arc<AtomicUsize>,
    }

    impl nmp_signer::SigningCapability for HookCompletesSigner {
        fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
            Some(signer_public_key(self.keys.public_key()))
        }

        fn sign(
            &self,
            unsigned: nmp_signer::SignerUnsignedEvent,
        ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
            let signed = nostr_signed_to_signer(
                signer_unsigned_to_nostr(unsigned)
                    .sign_with_keys(&self.keys)
                    .unwrap(),
            );
            let completion: Arc<
                Mutex<Option<nmp_signer::PendingSignerSender<nmp_signer::SignerSignedEvent>>>,
            > = Arc::new(Mutex::new(None));
            let completion_for_cancel = Arc::clone(&completion);
            let cancellations = Arc::clone(&self.cancellations);
            let (sender, operation) =
                nmp_signer::SignerOp::pending_channel_with_cancel(move || {
                    cancellations.fetch_add(1, Ordering::SeqCst);
                    if let Some(sender) = completion_for_cancel
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .take()
                    {
                        let _ = sender.resolve(Ok(signed));
                    }
                });
            *completion
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(sender);
            operation
        }
    }

    struct CountingSigner {
        keys: Keys,
        calls: Arc<AtomicUsize>,
    }

    impl nmp_signer::SigningCapability for CountingSigner {
        fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
            Some(signer_public_key(self.keys.public_key()))
        }

        fn sign(
            &self,
            unsigned: nmp_signer::SignerUnsignedEvent,
        ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            nmp_signer::SignerOp::ok(nostr_signed_to_signer(
                signer_unsigned_to_nostr(unsigned)
                    .sign_with_keys(&self.keys)
                    .unwrap(),
            ))
        }
    }

    #[test]
    fn sign_event_admits_then_invokes_the_signer_exactly_once() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let calls = Arc::new(AtomicUsize::new(0));
        engine
            .install_test_signing_capability(CountingSigner {
                keys: keys.clone(),
                calls: Arc::clone(&calls),
            })
            .unwrap();
        engine.select_test_account(Some(keys.public_key())).unwrap();

        let signed = engine
            .sign_event(SignEventRequest {
                created_at: Timestamp::from(5),
                kind: Kind::TextNote,
                tags: Vec::new(),
                content: "one slot".to_string(),
            })
            .expect("cap=1 must admit the operation")
            .recv()
            .expect("local signer must complete");
        assert_eq!(signed.pubkey, keys.public_key());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    impl nmp_signer::SigningCapability for PendingSigner {
        fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
            Some(signer_public_key(self.public_key))
        }

        fn sign(
            &self,
            _unsigned: nmp_signer::SignerUnsignedEvent,
        ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
            let producer: Arc<
                Mutex<Option<nmp_signer::PendingSignerSender<nmp_signer::SignerSignedEvent>>>,
            > = Arc::new(Mutex::new(None));
            let producer_for_cancel = Arc::clone(&producer);
            let cancellations = Arc::clone(&self.cancellations);
            let (sender, operation) =
                nmp_signer::SignerOp::pending_channel_with_cancel(move || {
                    cancellations.fetch_add(1, Ordering::SeqCst);
                    producer_for_cancel
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .take();
                });
            *producer.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(sender);
            operation
        }
    }

    #[test]
    fn cancelling_a_write_cancels_its_pending_signer() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let cancellations = Arc::new(AtomicUsize::new(0));
        engine
            .install_test_signing_capability(PendingSigner {
                public_key: keys.public_key(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.select_test_account(Some(keys.public_key())).unwrap();

        let publish = |content: &str| {
            engine
                .publish(WriteIntent {
                    payload: nmp_grammar::WritePayload::Event(nmp_grammar::EventBuilder {
                        kind: Kind::TextNote,
                        tags: (Vec::new()).into_iter().collect(),
                        content: content.to_string(),
                        created_at: Some(Timestamp::from(10)),
                    }),
                    routing: nmp_grammar::WriteRouting::Auto,
                    identity: Identity::Active,
                    correlation: None,
                })
                .expect("write must be accepted")
        };

        // #680 removed the native-task census, so the write's pending signer
        // cancellation is observed directly through the `cancellations` counter
        // (bounded poll) rather than the admitted-slot census. The real semantic
        // preserved: cancelling a write cancels its pending signer, and a second
        // write can be published and cancelled the same way.
        let wait_for_cancellations = |target: usize| {
            for _ in 0..500 {
                if cancellations.load(Ordering::SeqCst) >= target {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!(
                "expected {target} signer cancellations, saw {}",
                cancellations.load(Ordering::SeqCst)
            );
        };

        let first = publish("cancel cancels the pending signer");
        assert_eq!(engine.cancel(first.id), Ok(CancelWriteOutcome::Cancelled));
        wait_for_cancellations(1);

        let second = publish("a second write cancels the same way");
        assert_eq!(engine.cancel(second.id), Ok(CancelWriteOutcome::Cancelled));
        wait_for_cancellations(2);
        engine.shutdown();
    }

    #[test]
    fn superseding_a_replaceable_write_cancels_its_pending_signer() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let cancellations = Arc::new(AtomicUsize::new(0));
        engine
            .install_test_signing_capability(PendingSigner {
                public_key: keys.public_key(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.select_test_account(Some(keys.public_key())).unwrap();

        let publish = |created_at| {
            engine
                .publish(WriteIntent {
                    payload: nmp_grammar::WritePayload::Event(
                        nmp_grammar::EventBuilder::new(Kind::Metadata)
                            .content(format!("metadata at {created_at}"))
                            .created_at(Timestamp::from(created_at)),
                    ),
                    routing: nmp_grammar::WriteRouting::Auto,
                    identity: Identity::Active,
                    correlation: None,
                })
                .expect("write must be accepted")
        };

        let wait_for_cancellations = |target: usize| {
            for _ in 0..500 {
                if cancellations.load(Ordering::SeqCst) >= target {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!(
                "expected {target} signer cancellations, saw {}",
                cancellations.load(Ordering::SeqCst)
            );
        };

        let first = publish(1);
        let second = publish(2);
        assert_eq!(
            first.statuses.recv().unwrap(),
            WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Superseded))
        );
        wait_for_cancellations(1);

        assert_eq!(engine.cancel(second.id), Ok(CancelWriteOutcome::Cancelled));
        wait_for_cancellations(2);
        engine.shutdown();
    }

    #[test]
    fn sign_event_cancellation_is_session_scoped() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        let cancellations = Arc::new(AtomicUsize::new(0));
        engine
            .install_test_signing_capability(PendingSigner {
                public_key: keys.public_key(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.select_test_account(Some(keys.public_key())).unwrap();
        let request = SignEventRequest {
            created_at: nostr::Timestamp::from(3),
            kind: nostr::Kind::TextNote,
            tags: Vec::new(),
            content: "pending".to_string(),
        };

        let operation = engine.sign_event(request).expect("sign event is admitted");
        operation.cancel_handle().cancel();
        // The cancel hook runs inside `recv_or_cancel` before the operation
        // resolves, so `cancellations == 1` is deterministic once `recv()`
        // observes `Cancelled` (no removed native-task idle barrier needed).
        assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    #[test]
    fn shutdown_cancels_and_joins_an_accepted_sign_event() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let cancellations = Arc::new(AtomicUsize::new(0));
        engine
            .install_test_signing_capability(PendingSigner {
                public_key: keys.public_key(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.select_test_account(Some(keys.public_key())).unwrap();
        let operation = engine
            .sign_event(SignEventRequest {
                created_at: Timestamp::from(6),
                kind: Kind::TextNote,
                tags: Vec::new(),
                content: "shutdown".to_string(),
            })
            .expect("operation must be accepted");

        engine.shutdown();
        assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sign_event_cancellation_without_adapter_hook_drops_retained_producer_and_joins() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let (producer, operation) = nmp_signer::SignerOp::pending_channel();
        engine
            .install_test_signing_capability(NoHookPendingSigner {
                public_key: keys.public_key(),
                operation: Mutex::new(Some(operation)),
            })
            .unwrap();
        engine.select_test_account(Some(keys.public_key())).unwrap();
        let operation = engine
            .sign_event(SignEventRequest {
                created_at: Timestamp::from(7),
                kind: Kind::TextNote,
                tags: Vec::new(),
                content: "no cancellation hook".to_string(),
            })
            .expect("operation must be accepted");

        operation.cancel_handle().cancel();
        // `recv_or_cancel` sets `receiver = None` before the completion resolves
        // the operation, so once `recv()` observes `Cancelled` the worker
        // receiver is already dropped — deterministic without the removed
        // native-task idle barrier (#680).
        assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
        assert!(
            matches!(
                producer.resolve(Err(nmp_signer::SignerError::Unavailable)),
                Err(nmp_signer::PendingSignerResolveError::ReceiverDropped(_))
            ),
            "the worker receiver must be dropped even while the producer is retained"
        );
        engine.shutdown();
    }

    #[test]
    fn sign_event_shutdown_without_adapter_hook_drops_retained_producer_and_joins() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let (producer, operation) = nmp_signer::SignerOp::pending_channel();
        engine
            .install_test_signing_capability(NoHookPendingSigner {
                public_key: keys.public_key(),
                operation: Mutex::new(Some(operation)),
            })
            .unwrap();
        engine.select_test_account(Some(keys.public_key())).unwrap();
        let operation = engine
            .sign_event(SignEventRequest {
                created_at: Timestamp::from(8),
                kind: Kind::TextNote,
                tags: Vec::new(),
                content: "shutdown without hook".to_string(),
            })
            .expect("operation must be accepted");

        engine.shutdown();
        assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
        assert!(
            matches!(
                producer.resolve(Err(nmp_signer::SignerError::Unavailable)),
                Err(nmp_signer::PendingSignerResolveError::ReceiverDropped(_))
            ),
            "shutdown must drop the worker receiver while the producer is retained"
        );
    }

    #[test]
    fn sign_event_cancellation_claim_beats_hook_that_simultaneously_completes() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let cancellations = Arc::new(AtomicUsize::new(0));
        engine
            .install_test_signing_capability(HookCompletesSigner {
                keys: keys.clone(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.select_test_account(Some(keys.public_key())).unwrap();
        let operation = engine
            .sign_event(SignEventRequest {
                created_at: Timestamp::from(9),
                kind: Kind::TextNote,
                tags: Vec::new(),
                content: "cancel wins".to_string(),
            })
            .expect("operation must be accepted");

        operation.cancel_handle().cancel();
        // `recv_or_cancel` fires the cancel hook before the completion resolves
        // the operation, so once `recv()` observes `Cancelled` the hook has run
        // exactly once — no native-task idle barrier is needed (removed in #680).
        assert_eq!(operation.recv(), Err(SignEventError::Cancelled));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    // #680 deleted `sign_event_capacity_refusal_happens_before_signer_invocation`:
    // it asserted the removed global native-task capacity refusal
    // (`SignEventError::ExecutorSaturated` + `max_native_tasks`). Sign-event
    // admission no longer surfaces a configurable capacity ceiling.
    use nmp_grammar::{Identity, WritePayload, WriteRouting};
    use nostr::ToBech32;

    /// `EngineConfig::default()` (no `store_path`) must select the
    /// in-memory store and construct cleanly with no network at all -- no
    /// operator app/fallback relay configured.
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

    /// The test provider seam must accept both hex and bech32 `nsec` secret keys and
    /// return the same public key either way.
    #[test]
    fn test_provider_seam_accepts_legacy_fixture_encodings() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();

        let via_hex = engine
            .install_test_local_provider(&keys.secret_key().to_secret_hex())
            .expect("hex secret key must parse");
        assert_eq!(via_hex.public_key(), keys.public_key());

        let via_nsec = engine
            .install_test_local_provider(
                &keys
                    .secret_key()
                    .to_bech32()
                    .expect("secret key must encode as bech32"),
            )
            .expect("bech32 nsec must parse");
        assert_eq!(via_nsec.public_key(), keys.public_key());

        engine.shutdown();
    }

    /// A malformed secret key is a typed error, not a panic.
    #[test]
    fn test_provider_seam_rejects_malformed_fixture_key() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        assert_eq!(
            engine.install_test_local_provider("not-a-key"),
            Err(crate::SessionMutationError::InvalidSecretKey)
        );
        engine.shutdown();
    }

    /// One public key is one stable session account. Reinstalling its provider
    /// updates that account rather than minting a second identity category.
    #[test]
    fn same_key_provider_reinstall_updates_one_session_account() {
        let engine = Engine::new(EngineConfig {
            max_auth_capabilities: 1,
            ..EngineConfig::default()
        })
        .expect("engine must build");
        let keys = Keys::generate();
        let first = engine
            .install_test_local_provider(&keys.secret_key().to_secret_hex())
            .expect("first account must register");
        let replacement = engine
            .install_test_local_provider(&keys.secret_key().to_secret_hex())
            .expect("same-key replacement must not consume another slot");

        assert_eq!(first.public_key(), replacement.public_key());
        assert_eq!(first, replacement, "identity is the decoded public key");
        assert_eq!(engine.session().unwrap().accounts.len(), 1);
        assert!(engine.remove_account(&first).unwrap());
        assert!(
            !engine.remove_account(&replacement).unwrap(),
            "whole-account removal is identity-idempotent"
        );
        engine.shutdown();
    }

    struct AllowAuthPolicy;

    impl crate::AuthPolicy for AllowAuthPolicy {
        fn evaluate(&self, _request: crate::AuthPolicyRequest) -> crate::AuthPolicyOp {
            crate::AuthPolicyOp::allow()
        }
    }

    /// The same exact-instance discipline for AUTH-policy registrations.
    #[test]
    fn auth_policy_registration_is_exact_instance_repeatable_and_stale_safe() {
        let engine = Engine::new(EngineConfig {
            max_auth_capabilities: 1,
            ..EngineConfig::default()
        })
        .expect("engine must build");
        let public_key = Keys::generate().public_key();
        let first = engine
            .add_auth_policy(public_key, AllowAuthPolicy)
            .expect("first policy must register");
        let replacement = engine
            .add_auth_policy(public_key, AllowAuthPolicy)
            .expect("same-key replacement must not consume another slot");

        assert_eq!(first.expected_public_key(), public_key);
        assert_ne!(first, replacement);
        assert!(
            !engine.remove_auth_policy(&first).unwrap(),
            "a stale policy registration must no-op instead of detaching its replacement"
        );
        assert!(engine.remove_auth_policy(&replacement).unwrap());
        assert!(!engine.remove_auth_policy(&replacement).unwrap());
        engine.shutdown();
    }

    /// Zero capabilities intentionally admits none, with the typed error.
    #[test]
    fn zero_auth_capabilities_admits_none_with_typed_error() {
        let engine = Engine::new(EngineConfig {
            max_auth_capabilities: 0,
            ..EngineConfig::default()
        })
        .expect("zero-capability engine must still build");
        assert_eq!(
            engine
                .install_test_local_provider(&Keys::generate().secret_key().to_secret_hex())
                .err(),
            Some(crate::SessionMutationError::CapabilityRegistryFull { limit: 0 })
        );
        assert_eq!(
            engine
                .add_auth_policy(Keys::generate().public_key(), AllowAuthPolicy)
                .err(),
            Some(EngineError::AuthCapabilityRegistryFull { limit: 0 })
        );
        engine.shutdown();
    }

    /// Accounts and AUTH policies share ONE finite capability ceiling;
    /// removing a registration releases its shared slot.
    #[test]
    fn signer_and_policy_share_one_finite_capability_ceiling() {
        let engine = Engine::new(EngineConfig {
            max_auth_capabilities: 1,
            ..EngineConfig::default()
        })
        .expect("engine must build");
        let keys = Keys::generate();
        let account = engine
            .install_test_local_provider(&keys.secret_key().to_secret_hex())
            .expect("account consumes the one shared slot");
        assert_eq!(
            engine
                .add_auth_policy(keys.public_key(), AllowAuthPolicy)
                .err(),
            Some(EngineError::AuthCapabilityRegistryFull { limit: 1 })
        );
        assert!(engine.remove_account(&account).unwrap());
        engine
            .add_auth_policy(keys.public_key(), AllowAuthPolicy)
            .expect("removing the account releases the shared slot");
        engine.shutdown();
    }

    /// The account/policy lifecycle verbs fail closed after shutdown like
    /// every other verb.
    #[test]
    fn account_and_policy_lifecycle_fail_closed_after_shutdown() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let account = engine
            .install_test_local_provider(&keys.secret_key().to_secret_hex())
            .expect("account must register");
        let policy = engine
            .add_auth_policy(keys.public_key(), AllowAuthPolicy)
            .expect("policy must register");
        engine.shutdown();

        assert_eq!(
            engine.remove_account(&account).err(),
            Some(crate::SessionMutationError::EngineClosed)
        );
        assert_eq!(
            engine
                .add_auth_policy(keys.public_key(), AllowAuthPolicy)
                .err(),
            Some(EngineError::EngineClosed)
        );
        assert_eq!(
            engine.remove_auth_policy(&policy).err(),
            Some(EngineError::EngineClosed)
        );
    }

    #[test]
    fn sign_event_uses_the_current_account_without_publishing() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let pubkey = engine
            .install_test_local_provider(&keys.secret_key().to_secret_hex())
            .expect("account must register")
            .public_key();
        engine
            .select_test_account(Some(pubkey))
            .expect("account must activate");

        let signed = engine
            .sign_event(SignEventRequest {
                created_at: Timestamp::from(1_750_000_000),
                kind: Kind::Custom(27_235),
                tags: vec![Tag::parse(["client", "nip07-test"]).expect("valid tag")],
                content: "sign without publish".to_string(),
            })
            .expect("current account's local signing provider must start")
            .recv()
            .expect("current account's local signing provider must sign");

        assert_eq!(signed.pubkey, pubkey);
        assert_eq!(signed.created_at, Timestamp::from(1_750_000_000));
        assert_eq!(signed.kind, Kind::Custom(27_235));
        assert_eq!(signed.content, "sign without publish");
        assert!(signed.verify().is_ok());
        engine.shutdown();
    }

    #[test]
    fn sign_event_without_a_current_account_fails_closed() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let result = engine.sign_event(SignEventRequest {
            created_at: Timestamp::from(1_750_000_000),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "unsigned".to_string(),
        });
        match result {
            Err(error) => assert_eq!(error, SignEventError::NoCurrentSigningProvider),
            Ok(_) => panic!("a missing current account must fail closed"),
        }
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

        let refused = engine.publish(WriteIntent {
            payload: WritePayload::Signed(event),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        });
        assert!(
            matches!(
                refused.as_ref().err(),
                Some(EngineError::PublishRefused { .. })
            ),
            "a forged Signed payload must refuse the call itself, taking nothing \
             into custody -- got {:?}",
            refused.as_ref().err()
        );

        engine.shutdown();
    }

    /// #47 falsifier (a) through the facade: with account A current and B
    /// merely registered, never activated, a
    /// builder carrying `Identity::Explicit(B)` reaches
    /// `WriteFact::Signed` bearing the exact id of the frozen B-authored
    /// body -- which commits cryptographically to author and content --
    /// and the session still answers A afterward: naming B
    /// consented to ONE write, it never re-rooted the engine.
    #[test]
    fn an_explicit_identity_publishes_as_a_secondary_without_moving_the_current_account() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys_a = Keys::generate();
        let keys_b = Keys::generate();
        let pk_a = engine
            .install_test_local_provider(&keys_a.secret_key().to_secret_hex())
            .expect("account A must register")
            .public_key();
        let pk_b = engine
            .install_test_local_provider(&keys_b.secret_key().to_secret_hex())
            .expect("account B must register")
            .public_key();
        engine
            .select_test_account(Some(pk_a))
            .expect("account A must activate");

        let draft = nostr::UnsignedEvent::new(
            pk_b,
            Timestamp::from(1_750_000_047),
            Kind::Custom(9999),
            Vec::new(),
            "one write as b, engine still rooted on a",
        );
        let expected = draft
            .clone()
            .sign_with_keys(&keys_b)
            .expect("derive the frozen body's id");
        let rx = engine
            .publish(WriteIntent {
                payload: WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: draft.kind,
                    tags: draft.tags.iter().cloned().collect(),
                    content: draft.content.clone(),
                    created_at: Some(draft.created_at),
                }),
                routing: WriteRouting::Auto,
                identity: Identity::Explicit(pk_b),
                correlation: None,
            })
            .expect("engine is open")
            .statuses;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut signed_as_b = false;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(WriteFact::Signing(SigningState::Signed { event_id: id })) => {
                    assert_eq!(
                        id, expected.id,
                        "Signed must carry the frozen B-authored body's exact id"
                    );
                    signed_as_b = true;
                    break;
                }
                Ok(WriteFact::Signing(SigningState::Refused { reason })) => {
                    panic!("override publish must not be refused by the signer: {reason}")
                }
                Ok(WriteFact::Outcome(outcome)) => {
                    panic!("override publish must not terminate pre-routing: {outcome:?}")
                }
                Ok(_) => {}
                Err(crate::runtime::FifoRecvTimeoutError::Timeout) => {}
                Err(crate::runtime::FifoRecvTimeoutError::Closed) => break,
                Err(crate::runtime::FifoRecvTimeoutError::Lagged) => {
                    panic!("short identity-override receipt unexpectedly lagged")
                }
            }
        }
        assert!(signed_as_b, "override publish must reach Signed as B");
        assert_eq!(
            engine.test_current_public_key().expect("engine is open"),
            Some(pk_a),
            "the per-write override must never move the current account"
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
            engine.observe(probe_query(), None).err(),
            Some(EngineError::EngineClosed)
        );
        assert_eq!(
            engine.observe_diagnostics().err(),
            Some(EngineError::EngineClosed)
        );
        assert_eq!(
            engine.observe(probe_query(), Some(window_probe())).err(),
            Some(EngineError::EngineClosed)
        );
        assert_eq!(
            engine.select_test_account(None).err(),
            Some(EngineError::EngineClosed)
        );
        assert_eq!(
            engine.install_test_local_provider(&Keys::generate().secret_key().to_secret_hex()),
            Err(crate::SessionMutationError::EngineClosed)
        );
        let publish_result = engine.publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: nostr::Kind::Custom(9999),
                tags: (Vec::new()).into_iter().collect(),
                content: ("unreachable").into(),
                created_at: Some(nostr::Timestamp::now()),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        });
        assert_eq!(publish_result.err(), Some(EngineError::EngineClosed));
    }

    /// A second, concurrent `shutdown` racing the first must still only
    /// ever see the gate flip exactly once -- both calls are safe, and
    /// after both return the engine is closed exactly as if only one had
    /// been called.
    #[test]
    fn concurrent_shutdown_calls_are_race_free() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).expect("engine must build"));
        let other = Arc::clone(&engine);
        let joined = std::thread::spawn(move || other.shutdown());
        engine.shutdown();
        joined.join().expect("concurrent shutdown must not panic");

        assert_eq!(
            engine.select_test_account(None).err(),
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

        let subscription = engine.observe(probe_query(), None).expect("engine is open");
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
        let subscription = engine.observe(probe_query(), None).expect("engine is open");

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
            let terminal = loop {
                match subscription.recv() {
                    Ok(frame)
                        if frame
                            .execution
                            .iter()
                            .any(|evidence| evidence.kind == "withdrawn") =>
                    {
                        break true;
                    }
                    Ok(_) => continue,
                    Err(_) => break false,
                }
            };
            let disconnected = subscription.recv().is_err();
            let _ = result_tx.send((terminal, disconnected));
        });

        cancel.cancel();

        let (terminal, disconnected) = result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect(
                "cancel() from a separate handle must unblock the drain thread's recv() \
                 within a bounded wait, not hang",
            );
        assert!(
            terminal,
            "the unblocked recv() must expose the observation's terminal Withdrawn fact"
        );
        assert!(
            disconnected,
            "the receive after Withdrawn must observe a deterministic disconnect"
        );

        engine.shutdown();
    }

    #[test]
    fn history_cancel_handle_unblocks_idle_recv_within_a_bound() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let subscription = engine
            .observe(probe_query(), Some(window_probe()))
            .expect("engine is open");
        subscription
            .recv()
            .expect("a fresh windowed subscription delivers its current state");
        let cancel = subscription.cancel_handle();

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = result_tx.send(subscription.recv().is_err());
        });
        cancel.cancel();
        assert!(result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("history cancellation must wake the blocked receiver"));
        engine.shutdown();
    }

    #[test]
    fn shutdown_wakes_a_live_history_receiver_within_a_bound() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let subscription = engine
            .observe(probe_query(), Some(window_probe()))
            .expect("engine is open");
        subscription
            .recv()
            .expect("a fresh windowed subscription delivers its current state");

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = result_tx.send(subscription.recv().is_err());
        });
        engine.shutdown();
        assert!(result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("shutdown must wake the blocked history receiver"));
    }

    #[test]
    fn history_advance_and_blocking_recv_have_safe_split_ownership() {
        use nmp_store::EventStore;

        let fixture = tempfile::tempdir().expect("temporary directory");
        let path = fixture.path().join("history-advance.redb");
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-facade.example").unwrap();
        {
            let mut store = RedbStore::open(&path).expect("history store must open");
            for index in 0..3 {
                let event = UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(100),
                    Kind::Custom(7_777),
                    Vec::new(),
                    format!("history-{index}"),
                )
                .sign_with_keys(&keys)
                .unwrap();
                store
                    .insert(
                        event,
                        nmp_store::RelayObserved::new(relay.clone(), Timestamp::from(200)),
                    )
                    .unwrap();
            }
        }

        let engine = Engine::new(EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .expect("engine must build");
        let query = LiveQuery::from_filter(nmp_grammar::Filter {
            kinds: Some(std::collections::BTreeSet::from([7_777])),
            authors: Some(nmp_grammar::Binding::Literal(
                std::collections::BTreeSet::from([keys.public_key().to_hex()]),
            )),
            ..nmp_grammar::Filter::default()
        });
        let window = Window::Expandable {
            initial: std::num::NonZeroUsize::new(1).unwrap(),
            max: std::num::NonZeroUsize::new(3).unwrap(),
        };
        let subscription = engine
            .observe(query, Some(window))
            .expect("window must open");
        subscription.recv().expect("initial frame must arrive");
        let window_handle = subscription
            .window_handle()
            .expect("a windowed observation exposes a window handle");

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (batch_tx, batch_rx) = std::sync::mpsc::channel();
        let drain = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            loop {
                let frame = subscription.recv();
                let returned = matches!(
                    frame
                        .as_ref()
                        .ok()
                        .and_then(|frame| frame.window.as_ref())
                        .map(|window| window.load),
                    Some(crate::core::WindowLoad::Returned { .. })
                );
                if returned || frame.is_err() {
                    batch_tx.send(frame).unwrap();
                    break;
                }
            }
        });
        ready_rx.recv().unwrap();
        window_handle
            .request_rows(2)
            .expect("separate capability must grow the window while recv owns delivery");
        let frame = batch_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("growth must unblock the independently-owned receiver")
            .expect("window channel stays open");
        let contents = frame.window.expect("windowed frames carry window contents");
        assert_eq!(
            contents.load,
            crate::core::WindowLoad::Returned { added: 1 }
        );
        assert_eq!(contents.rows.len(), 2);
        drain.join().unwrap();

        // The drain's subscription has already dropped and cancelled the
        // shared session. A retained window-handle clone converges on that
        // same idempotent guard rather than issuing a second withdrawal.
        window_handle.cancel();
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

        let subscription = engine.observe(probe_query(), None).expect("engine is open");
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

    #[test]
    fn live_nip11_cannot_outlive_real_engine_shutdown_with_retained_owners() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::Barrier::new(2));
        let server_accepted = Arc::clone(&accepted);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "HTTP request ended before its headers");
                received.extend_from_slice(&buffer[..count]);
            }
            server_accepted.wait();
            let mut sink = Vec::new();
            let _ = stream.read_to_end(&mut sink);
        });

        // Issue #519: the resolved-IP admission check now refuses a loopback
        // dial by default, so this test's own `127.0.0.1` NIP-11 mock server
        // needs the same operator opt-in a real local relay would use.
        let engine = Arc::new(
            Engine::new(EngineConfig {
                ..EngineConfig::default()
            })
            .expect("engine must build"),
        );
        let retained_engine = Arc::clone(&engine);
        let subscription = engine.observe(probe_query(), None).expect("engine is open");
        subscription
            .recv()
            .expect("a fresh subscription delivers its initial frame");
        let cancel = subscription.cancel_handle();
        let relay = format!("ws://{address}");
        let acquisition = std::thread::spawn(move || {
            block_on(
                retained_engine.relay_information(&relay, RelayInformationCachePolicy::Refresh),
            )
        });
        accepted.wait();

        let shutdown_engine = Arc::clone(&engine);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            shutdown_engine.shutdown();
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("live cancellable DNS/HTTP must not hold EngineThread shutdown");
        assert!(matches!(
            acquisition.join().unwrap(),
            Err(RelayInformationRequestError::Acquisition(
                RelayInformationError::ServiceClosed
            ))
        ));
        // #680 removed the native-task census surface; the real semantic here
        // is that shutdown drained the live acquisition (ServiceClosed above)
        // without blocking, and the subscription reaches disconnect. The
        // observation-evidence stream may still have its final pre-shutdown
        // batch queued after the initial row frame; consuming that bounded
        // batch before disconnect is delivery, not an outliving producer.
        let mut queued_after_shutdown = 0;
        loop {
            match subscription.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(_) => {
                    queued_after_shutdown += 1;
                    assert_eq!(
                        queued_after_shutdown, 1,
                        "the one-slot mailbox cannot retain multiple frames after shutdown"
                    );
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("subscription producer outlived engine shutdown")
                }
            }
        }

        // These retained owners remain safe after exact-zero teardown.
        cancel.cancel();
        drop(subscription);
        drop(engine);
        server.join().unwrap();
    }

    #[test]
    fn sixty_four_owned_facade_values_do_not_become_engine_retention() {
        const BODY_BYTES: usize = 256 * 1024;
        const CALLS: usize = 64;

        let prefix = r#"{"description":""#;
        let suffix = r#""}"#;
        let body = format!(
            "{prefix}{}{suffix}",
            "x".repeat(BODY_BYTES - prefix.len() - suffix.len())
        );
        assert_eq!(body.len(), BODY_BYTES);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "HTTP request ended before its headers");
                received.extend_from_slice(&buffer[..count]);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/nostr+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        // Issue #519: opt the mock server's loopback host in — see the
        // identical note in `live_nip11_cannot_outlive_real_engine_shutdown_with_retained_owners`.
        let engine = Engine::new(EngineConfig {
            ..EngineConfig::default()
        })
        .expect("engine must build");
        let relay = format!("ws://{address}");
        let mut caller_owned = Vec::with_capacity(CALLS);
        caller_owned.push(
            block_on(engine.relay_information(&relay, RelayInformationCachePolicy::Refresh))
                .unwrap(),
        );
        server.join().unwrap();
        for _ in 1..CALLS {
            caller_owned.push(
                block_on(engine.relay_information(&relay, RelayInformationCachePolicy::UseCache))
                    .unwrap(),
            );
        }
        assert!(caller_owned
            .iter()
            .all(|snapshot| snapshot.raw_json.len() == BODY_BYTES));

        let while_callers_retain = engine.relay_information_retention_census();
        assert_eq!(while_callers_retain.cached_entries, 1);
        assert_eq!(while_callers_retain.cached_payloads, 1);
        assert_eq!(while_callers_retain.cached_raw_body_bytes, BODY_BYTES);
        assert_eq!(while_callers_retain.active_flights, 0);
        assert_eq!(while_callers_retain.subscribed_callers, 0);

        // The 64 ordinary facade values above intentionally own 64 public
        // copies. Dropping them cannot change the engine census because those
        // copies transferred to the caller at the supported value boundary.
        drop(caller_owned);
        assert_eq!(
            engine.relay_information_retention_census(),
            while_callers_retain
        );
        engine.shutdown();
    }

    fn probe_query() -> LiveQuery {
        LiveQuery::from_filter(nmp_grammar::Filter {
            // An arbitrary caller-owned kind, not any NIP-01 core schema --
            // see this module's other fixtures for why.
            kinds: Some(std::collections::BTreeSet::from([9999u16])),
            ..nmp_grammar::Filter::default()
        })
    }

    fn window_probe() -> Window {
        Window::Expandable {
            initial: std::num::NonZeroUsize::new(1).unwrap(),
            max: std::num::NonZeroUsize::new(2).unwrap(),
        }
    }

    /// An unbounded observation has no window: `request_rows` is a typed
    /// `Unwindowed` refusal and `window_handle()` is `None`. The growth
    /// capability's very existence is derived from the window policy.
    #[test]
    fn unwindowed_observation_has_no_growth_capability() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let subscription = engine.observe(probe_query(), None).expect("engine is open");
        subscription
            .recv()
            .expect("a fresh subscribe delivers one batch");
        assert!(subscription.window_handle().is_none());
        assert_eq!(
            subscription.request_rows(10),
            Err(crate::RequestRowsError::Unwindowed)
        );
        engine.shutdown();
    }

    /// `initial > max` and a selection that already carries a NIP-01 `limit`
    /// are typed `EngineError`s caught at `observe`, before the engine is
    /// touched.
    #[test]
    fn windowed_observe_rejects_bad_bounds_and_competing_limit() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        assert_eq!(
            engine
                .observe(
                    probe_query(),
                    Some(Window::Expandable {
                        initial: std::num::NonZeroUsize::new(5).unwrap(),
                        max: std::num::NonZeroUsize::new(2).unwrap(),
                    })
                )
                .err(),
            Some(EngineError::WindowInitialExceedsMax { initial: 5, max: 2 })
        );
        let mut branch = probe_query().branches()[0].clone();
        branch.selection.limit = Some(3);
        let limited = LiveQuery::single(branch);
        assert_eq!(
            engine.observe(limited, Some(window_probe())).err(),
            Some(EngineError::WindowSelectionHasLimit)
        );
        engine.shutdown();
    }
}
