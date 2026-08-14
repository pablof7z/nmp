//! [`Engine`] -- the one supported construction call plus the two nouns
//! (canonical-facade-52-plan.md §1). Owns config -> store/routing-fact
//! selection and the router cap `nmp-ffi` used to duplicate by hand.
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

mod observation;

use std::sync::Mutex;

use crate::core::ReceiptId;
use crate::publish_queue::{
    PublishQueueEntry, PublishQueueReadError, ReceiptResult, ReceiptResultError,
    RemoveQueueEntryError,
};
#[cfg(any(test, feature = "test-instrumentation"))]
use crate::runtime::SignerRegistration;
use crate::runtime::{
    EngineThread, Handle, ReceiptReattachment, ReceiptReplayCursor, ReceiptStream, RuntimeConfig,
    SignEventError, SignEventOperation,
};
#[cfg(test)]
use crate::subscription::{Subscription, Window};
#[cfg(test)]
use nmp_grammar::LiveQuery;
use nmp_grammar::WriteIntent;
use nmp_signer::SigningCapability;
use nmp_store::{RedbStore, RedbStoreOpenError, RedbStoreResetError};
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
use crate::subscription::{AsyncDiagnosticsSubscription, DiagnosticsSubscription};

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
    /// router cap, everything `nmp-ffi`'s hand-rolled assembly used to
    /// duplicate independently.
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
                    | RedbStoreOpenError::TemporaryDirectoryFailed { .. }
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
                let store =
                    RedbStore::temporary().map_err(|error| EngineError::StoreOpenFailed {
                        reason: error.to_string(),
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
    pub fn from_parts(
        store: RedbStore,
        cap: usize,
        pool_config: PoolConfig,
    ) -> Result<Self, EngineError> {
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
    pub fn from_parts_with_fixture_routing_facts(
        store: RedbStore,
        facts: nmp_router::FixtureRoutingFacts,
        cap: usize,
        pool_config: PoolConfig,
    ) -> Result<Self, EngineError> {
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
    pub fn from_parts_with_fixture_routing_facts_and_nip65_sources(
        store: RedbStore,
        facts: nmp_router::FixtureRoutingFacts,
        nip65_sources: Vec<RelayUrl>,
        cap: usize,
        pool_config: PoolConfig,
    ) -> Result<Self, EngineError> {
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
            .relay_information_async(relay, policy)
            .await
            .map_err(RelayInformationRequestError::Acquisition)
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
mod tests;
