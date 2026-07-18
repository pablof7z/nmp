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

use nmp_engine::runtime::FifoReceiver;
use std::sync::{Arc, Mutex};

use nmp_engine::core::ReceiptId;
use nmp_engine::outbox::WriteStatus;
use nmp_engine::runtime::{
    EngineThread, Handle, HistoryHandle, HistoryReceiver, QueryHandle, ReceiptReattachment,
    ReceiptStream, RowsReceiver, RuntimeConfig, SignEventError, SignEventOperation,
    SignerRegistration,
};
use nmp_grammar::WriteIntent;
use nmp_resolver::LiveQuery;
use nmp_store::{MemoryStore, RedbStore, RedbStoreResetError};
use nmp_transport::PoolConfig;
use nostr::RelayUrl;
use nostr::{EventId, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

use crate::auth::{AuthPolicy, EngineAuthPolicyAdapter};
use crate::config::{build_admission_policy, build_directory, EngineConfig};
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
    active_pubkey: Option<PublicKey>,
}

/// The one supported Rust product surface (canonical-facade-52-plan.md §1).
/// Owns the `EngineThread` + `Handle` pair; every mechanism crate
/// (`nmp-store`/`nmp-router`/`nmp-transport`/`nmp-resolver`) is reached only
/// through here. See this module's doc for the serialized lifecycle gate
/// `inner` implements.
pub struct Engine {
    inner: Mutex<Option<Inner>>,
    native_tasks: nmp_executor::Executor,
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
    AlreadyAbandoned {
        receipt_id: ReceiptId,
    },
    PersistenceFailed {
        receipt_id: ReceiptId,
        reason: String,
    },
    EngineClosed,
}

fn cancel_write_outcome_from_engine(
    outcome: nmp_engine::outbox::CancelWriteOutcome,
) -> CancelWriteOutcome {
    match outcome {
        nmp_engine::outbox::CancelWriteOutcome::Cancelled => CancelWriteOutcome::Cancelled,
    }
}

fn cancel_write_error_from_engine(error: nmp_engine::outbox::CancelWriteError) -> CancelWriteError {
    match error {
        nmp_engine::outbox::CancelWriteError::UnknownReceipt { receipt_id } => {
            CancelWriteError::UnknownReceipt { receipt_id }
        }
        nmp_engine::outbox::CancelWriteError::AlreadySigned {
            receipt_id,
            event_id,
        } => CancelWriteError::AlreadySigned {
            receipt_id,
            event_id,
        },
        nmp_engine::outbox::CancelWriteError::AlreadyCompensated { receipt_id } => {
            CancelWriteError::AlreadyCompensated { receipt_id }
        }
        nmp_engine::outbox::CancelWriteError::AlreadyAbandoned { receipt_id } => {
            CancelWriteError::AlreadyAbandoned { receipt_id }
        }
        nmp_engine::outbox::CancelWriteError::PersistenceFailed { receipt_id, reason } => {
            CancelWriteError::PersistenceFailed { receipt_id, reason }
        }
        nmp_engine::outbox::CancelWriteError::EngineClosed => CancelWriteError::EngineClosed,
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
            Self::AlreadyAbandoned { receipt_id } => {
                write!(f, "receipt {} was abandoned after restart", receipt_id.0)
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

/// Opaque ownership proof for one exact local-account installation (#8's
/// ratified account model, closing #495). Returned by
/// [`Engine::add_account`]; the ONLY value that may later detach that exact
/// installation via [`Engine::remove_account`]. Equality is capability-
/// INSTANCE identity, not key equality: registering the same key again
/// mints a NEW registration and invalidates this one, so a stale clone can
/// never detach its replacement.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountRegistration {
    inner: SignerRegistration,
}

impl AccountRegistration {
    /// The registered account's public key.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.inner.public_key()
    }
}

impl std::fmt::Debug for AccountRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountRegistration")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// Opaque ownership proof for one exact AUTH-policy installation (#8).
/// Same exact-instance discipline as [`AccountRegistration`]: replacement
/// invalidates it, and a stale clone cannot detach the replacement.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthPolicyRegistration {
    inner: nmp_engine::runtime::AuthPolicyRegistration,
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

/// One event body to sign with the active account without publishing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignEventRequest {
    pub created_at: Timestamp,
    pub kind: Kind,
    pub tags: Vec<Tag>,
    pub content: String,
}

/// Executor-owned cancellation fallback for a blocking task whose producer
/// is the engine runtime itself. It contains only a raw shutdown sender, not
/// an `Arc<Engine>`, so task registration cannot create an ownership cycle.
#[doc(hidden)]
#[derive(Clone)]
pub struct NativeTaskCancel {
    action: Arc<dyn Fn() + Send + Sync>,
}

impl NativeTaskCancel {
    pub fn cancel(&self) {
        (self.action)();
    }
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
    /// Destructively remove one closed persistent engine store.
    ///
    /// This clears NMP's canonical events, pending writes, receipts,
    /// coverage/evidence, and all other state held in that store. It does not
    /// touch any separately configured platform signer-provider checkpoint.
    /// A live in-process engine using the same canonical path is refused with
    /// [`EngineError::StoreStillOpen`] without touching the file. Call
    /// [`Engine::shutdown`] (or drop the engine) first. A missing path is
    /// already reset and succeeds. Cross-process exclusion is not provided.
    pub fn reset_persistent_store(path: impl AsRef<std::path::Path>) -> Result<(), EngineError> {
        match RedbStore::reset(path) {
            Ok(()) => Ok(()),
            Err(RedbStoreResetError::StoreStillOpen { path }) => Err(EngineError::StoreStillOpen {
                path: path.to_string_lossy().into_owned(),
            }),
            Err(RedbStoreResetError::ResetFailed { reason }) => {
                Err(EngineError::StoreResetFailed { reason })
            }
        }
    }

    /// The ONE construction call: config -> store/directory selection,
    /// router cap, everything `nmp-ffi::facade::build_directory` and
    /// `nmp-demo`'s hand-rolled assembly used to duplicate independently.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        let directory = build_directory(&config)?;
        let admission = build_admission_policy(&config);
        // #20: one effective ceiling is threaded to both the whole-demand
        // compiler and transport. EngineThread normalizes legacy zero to the
        // finite default and resolves any mechanism-level mismatch downward.
        let pool_config = PoolConfig {
            max_relays: config.max_relays,
            ..PoolConfig::default()
        };

        let runtime_config = RuntimeConfig {
            max_auth_capabilities: config.max_auth_capabilities,
        };
        let (engine_thread, handle) = match &config.store_path {
            Some(path) => {
                let store = RedbStore::open(path).map_err(|e| EngineError::StoreOpenFailed {
                    reason: e.to_string(),
                })?;
                EngineThread::spawn_with_runtime_config(
                    store,
                    directory,
                    config.max_relays,
                    pool_config,
                    admission,
                    runtime_config,
                )
                .map_err(EngineError::from_thread_error)?
            }
            None => {
                let store = MemoryStore::new();
                EngineThread::spawn_with_runtime_config(
                    store,
                    directory,
                    config.max_relays,
                    pool_config,
                    admission,
                    runtime_config,
                )
                .map_err(EngineError::from_thread_error)?
            }
        };

        let native_tasks = engine_thread.native_tasks();
        Ok(Self {
            inner: Mutex::new(Some(Inner {
                handle,
                engine_thread,
                active_pubkey: None,
            })),
            native_tasks,
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
    ) -> Result<Self, EngineError>
    where
        S: nmp_store::EventStore + Send + 'static,
        D: nmp_router::RelayDirectory + Send + 'static,
    {
        let (engine_thread, handle) =
            EngineThread::spawn(store, directory, cap, pool_config, admission)
                .map_err(EngineError::from_thread_error)?;
        let native_tasks = engine_thread.native_tasks();
        Ok(Self {
            inner: Mutex::new(Some(Inner {
                handle,
                engine_thread,
                active_pubkey: None,
            })),
            native_tasks,
        })
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

    /// Reserve a slot on the engine's fixed internal blocking-adapter pool
    /// before accepting the transient blocking work it will own (the NIP-02
    /// follow observer/action worker). This is intentionally hidden mechanism
    /// used by protocol adapters, not an app scheduling API — the pool has no
    /// app-visible capacity, census, or idle barrier, and observations never
    /// touch it (they are pure-waker async since #680). A residual refusal at
    /// the fixed ceiling surfaces as [`EngineError::ThreadUnavailable`], never
    /// a global native-task ceiling.
    #[doc(hidden)]
    pub fn reserve_native_task(
        &self,
        component: impl Into<String>,
    ) -> Result<nmp_executor::Reservation, EngineError> {
        let component = component.into();
        self.with_handle(|_| self.native_tasks.reserve(component))?
            .map_err(|error| EngineError::ThreadUnavailable {
                reason: error.to_string(),
                component: error.component,
            })
    }

    #[doc(hidden)]
    pub fn native_task_cancel(&self) -> Result<NativeTaskCancel, EngineError> {
        self.with_handle(|handle| {
            let handle = handle.clone();
            NativeTaskCancel {
                action: Arc::new(move || handle.shutdown()),
            }
        })
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
                .map_err(EngineError::from_thread_error),
            Some(Window::Expandable { initial, max }) => {
                if initial > max {
                    return Err(EngineError::WindowInitialExceedsMax {
                        initial: initial.get(),
                        max: max.get(),
                    });
                }
                if query.0.selection.limit.is_some() {
                    return Err(EngineError::WindowSelectionHasLimit);
                }
                let history_query =
                    nmp_engine::core::HistoryQuery::new(query, initial.get(), max.get());
                self.with_handle(|handle| {
                    handle
                        .subscribe_history(history_query)
                        .map(|(history_handle, batches)| {
                            windowed(handle.clone(), history_handle, batches)
                        })
                })?
                .map_err(EngineError::from_thread_error)
            }
        }
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
    ///
    /// Identity (#47): with `identity_override: None` an unsigned draft
    /// signs as the CURRENT active account and fails closed pre-acceptance
    /// when the draft's author is anyone else (or when nobody is active).
    /// `identity_override: Some(pk)` is explicit per-write consent to
    /// publish as `pk` — a registered/secondary identity — without touching
    /// the active account: `pk` must equal the draft's author (the engine
    /// never restamps; a mismatch is `WriteStatus::Failed` with no
    /// `Accepted`), it works even while logged out, and acceptance pins
    /// `pk` so later [`Self::set_active_account`] calls cannot retarget the
    /// write. An override whose key has no registered signing capability
    /// parks durably as `WriteStatus::AwaitingCapability` until that exact
    /// key's signer attaches.
    pub fn publish(&self, intent: WriteIntent) -> Result<FifoReceiver<WriteStatus>, EngineError> {
        self.with_handle(|handle| handle.publish(intent))?
            .map_err(EngineError::from_publish_error)
    }

    /// Enqueue a write while retaining the stable store-issued receipt id
    /// needed for process-later reattachment. Pre-acceptance correlation-id
    /// exhaustion returns a typed error without creating a receipt.
    /// Identity resolution follows [`Self::publish`]'s contract exactly:
    /// active-account default, or an explicit `identity_override` that must
    /// equal the draft's author and is pinned at acceptance (#47).
    pub fn publish_tracked(&self, intent: WriteIntent) -> Result<ReceiptStream, EngineError> {
        self.with_handle(|handle| handle.publish_tracked(intent))?
            .map_err(EngineError::from_publish_error)
    }

    /// Reattach to durable receipt facts after a restart. Missing ids and
    /// retained obligations with unreadable evidence are distinct outcomes.
    pub fn reattach_receipt(&self, id: ReceiptId) -> Result<ReceiptReattachment, EngineError> {
        self.with_handle(|handle| handle.reattach_receipt(id))
    }

    /// Explicitly cancel one accepted unsigned write by its stable receipt
    /// id. [`CancelWriteOutcome::Cancelled`] means the durable
    /// [`WriteStatus::Cancelled`] fact committed; signed or otherwise terminal
    /// receipts return a precise typed refusal.
    pub fn cancel(&self, id: ReceiptId) -> Result<CancelWriteOutcome, CancelWriteError> {
        self.with_handle(|handle| handle.cancel_write(id))
            .map_err(|_| CancelWriteError::EngineClosed)?
            .map(cancel_write_outcome_from_engine)
            .map_err(cancel_write_error_from_engine)
    }

    /// Register an account from its secret key (hex or bech32 `nsec`). Does
    /// NOT make the account active -- call [`Self::set_active_account`] for
    /// that. Returns the [`AccountRegistration`] -- the only value that may
    /// later detach this exact installation via [`Self::remove_account`]
    /// (#8's ratified account model; there is deliberately no pubkey-only
    /// removal).
    ///
    /// Registering the same key again replaces the capability, mints a NEW
    /// registration, and invalidates the prior one. Registration is bounded
    /// by [`EngineConfig::max_auth_capabilities`](crate::EngineConfig::max_auth_capabilities)
    /// (shared with [`Self::add_auth_policy`]); a full registry or an
    /// exhausted instance namespace fails closed with a typed error and
    /// registers nothing.
    ///
    /// This builds a `LocalKeySigner` internally, whose `public_key()`
    /// always reports `Some` -- there is no reachable "signer has no
    /// public key" state on this path (unlike an arbitrary third-party
    /// `SigningCapability`, which the `unstable-mechanism`-gated
    /// `add_signer` covers instead).
    pub fn add_account(&self, secret_key: &str) -> Result<AccountRegistration, EngineError> {
        let keys = Keys::parse(secret_key).map_err(|_| EngineError::InvalidSecretKey)?;
        let registration = self.with_handle(|handle| {
            handle
                .add_signer(nmp_signer::LocalKeySigner::new(keys))
                .map_err(EngineError::from_add_signer_error)
        })??;
        Ok(AccountRegistration {
            inner: registration,
        })
    }

    /// Detach one exact local-account installation without changing active
    /// identity or any accepted write's frozen author. Returns `Ok(false)`
    /// for a stale registration -- one already removed, or one superseded by
    /// a newer [`Self::add_account`] for the same key -- which is a no-op
    /// that can never detach the replacement.
    pub fn remove_account(&self, registration: &AccountRegistration) -> Result<bool, EngineError> {
        self.with_handle(|handle| handle.remove_signer(registration.inner.clone()))
    }

    /// Register an arbitrary signing capability (e.g. a NIP-46/bunker
    /// remote signer) -- the lower-level verb [`Self::add_account`] sits on
    /// top of for the common local-key case. Same "does not activate it"
    /// caveat as `add_account`.
    ///
    /// The promotion boundary verifies signature, id, author, timestamp,
    /// kind, tags, and content against the frozen accepted template before
    /// any relay publication. Capabilities without a stable public key are
    /// rejected rather than stored unreachably.
    pub fn add_signer<Sig>(&self, signer: Sig) -> Result<SignerRegistration, EngineError>
    where
        Sig: nmp_signer::SigningCapability + Send + 'static,
    {
        self.with_handle(|handle| {
            handle
                .add_signer(signer)
                .map_err(EngineError::from_add_signer_error)
        })?
    }

    /// Detach one exact signer installation without changing active identity
    /// or any accepted write's frozen author. A stale registration cannot
    /// detach a newer signer installed for the same public key.
    pub fn remove_signer(&self, registration: SignerRegistration) -> Result<bool, EngineError> {
        self.with_handle(|handle| handle.remove_signer(registration))
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

    /// Sign one immutable unsigned event through the currently active
    /// account's registered capability and return the exact signed event.
    ///
    /// This is intentionally orthogonal to [`Self::publish`]: it creates no
    /// write intent, pending row, receipt, outbox lane, relay plan, or
    /// publication. The active author is frozen while the same lifecycle /
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
            let pubkey = inner.active_pubkey.ok_or(SignEventError::NoActiveSigner)?;
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
    ) -> Result<nmp_engine::runtime::SignEventCancel, SignEventError> {
        let (handle, pubkey) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let inner = guard.as_ref().ok_or(SignEventError::EngineClosed)?;
            let pubkey = inner.active_pubkey.ok_or(SignEventError::NoActiveSigner)?;
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

    /// Re-root every reactive query AND the active signing capability
    /// together onto `pubkey` (`None` -> logged-out / read-only). `pubkey`
    /// need not have been registered via [`Self::add_account`] -- read-only
    /// browsing of an account this app holds no key for is legal. Publishes
    /// attempted in that keyless-active state resolve truthfully, never a
    /// panic: an unsigned draft AUTHORED BY the keyless active pubkey is
    /// accepted and parks durably as `WriteStatus::AwaitingCapability`
    /// until a matching signing capability attaches, while a draft authored
    /// by a DIFFERENT pubkey (and carrying no `identity_override`, #47)
    /// fails closed pre-acceptance as `WriteStatus::Failed`.
    pub fn set_active_account(&self, pubkey: Option<PublicKey>) -> Result<(), EngineError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match &mut *guard {
            Some(inner) => {
                inner.handle.set_active_account(pubkey);
                inner.active_pubkey = pubkey;
                Ok(())
            }
            None => Err(EngineError::EngineClosed),
        }
    }

    /// The account currently rooting reactive identity and unsigned writes.
    /// This is facade-owned identity state, not a cache projection. It is
    /// updated under the same lifecycle mutex as [`Self::set_active_account`]
    /// so protocol actions can pin the author they are editing and detect an
    /// account switch before acceptance.
    pub fn active_account(&self) -> Result<Option<PublicKey>, EngineError> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match &*guard {
            Some(inner) => Ok(inner.active_pubkey),
            None => Err(EngineError::EngineClosed),
        }
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
    ) -> nmp_engine::relay_information::RelayInformationRetentionCensus {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard
            .as_ref()
            .map(|inner| nmp_engine::runtime::relay_information_retention_census(&inner.handle))
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
            active_pubkey: _,
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
    use std::future::Future;
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[test]
    fn allowed_local_relay_host_reaches_the_facade_transport_pool() {
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
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..EngineConfig::default()
        })
        .expect("local relay opt-in must build");
        let query = LiveQuery(
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
        while !found && Instant::now() < deadline {
            if let Ok(frame) = subscription.recv_timeout(Duration::from_millis(250)) {
                found = frame
                    .deltas
                    .iter()
                    .filter_map(|delta| delta.event())
                    .any(|received| received.id == expected_id);
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
        assert!(found, "allowed local relay never reached the facade query");
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

        engine.shutdown();

        Engine::reset_persistent_store(&path).expect("a closed store must reset");
        assert!(
            !path.exists(),
            "reset must remove the complete canonical store"
        );
        Engine::reset_persistent_store(&path).expect("a missing store is already reset");

        let reopened = Engine::new(config).expect("reset path must open as a fresh store");
        drop(reopened);
        Engine::reset_persistent_store(&path)
            .expect("dropping an engine must release its store registration");
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
            .expect("failed construction must release its path registration");
        assert!(!path.exists(), "reset must remove the failed-open store");
    }

    #[test]
    fn facade_cancellation_is_typed_idempotent_and_reattachable() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        engine
            .set_active_account(Some(keys.public_key()))
            .expect("engine open");
        let receipt = engine
            .publish_tracked(WriteIntent {
                payload: nmp_grammar::WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(10),
                    Kind::TextNote,
                    Vec::new(),
                    "cancel through facade",
                )),
                durability: nmp_grammar::Durability::Durable,
                routing: nmp_grammar::WriteRouting::AuthorOutbox,
                identity_override: None,
            })
            .expect("accept write");
        assert_eq!(
            receipt
                .statuses
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            WriteStatus::Accepted
        );

        assert_eq!(engine.cancel(receipt.id), Ok(CancelWriteOutcome::Cancelled));
        let mut saw_cancelled = false;
        while let Ok(status) = receipt
            .statuses
            .recv_timeout(std::time::Duration::from_secs(1))
        {
            if status == WriteStatus::Cancelled {
                saw_cancelled = true;
                break;
            }
        }
        assert!(saw_cancelled);
        assert_eq!(engine.cancel(receipt.id), Ok(CancelWriteOutcome::Cancelled));

        let ReceiptReattachment::Attached(replay) = engine.reattach_receipt(receipt.id).unwrap()
        else {
            panic!("cancelled receipt must remain reattachable")
        };
        assert_eq!(replay.recv().unwrap(), WriteStatus::Cancelled);
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
            .set_active_account(Some(keys.public_key()))
            .expect("engine open");
        let receipt = engine
            .publish_tracked(WriteIntent {
                payload: nmp_grammar::WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(11),
                    Kind::TextNote,
                    Vec::new(),
                    "observer lifetime is not write ownership",
                )),
                durability: nmp_grammar::Durability::Durable,
                routing: nmp_grammar::WriteRouting::AuthorOutbox,
                identity_override: None,
            })
            .expect("accept write");
        let receipt_id = receipt.id;
        assert_eq!(receipt.statuses.recv().unwrap(), WriteStatus::Accepted);
        drop(receipt.statuses);

        let ReceiptReattachment::Attached(replay) = engine.reattach_receipt(receipt_id).unwrap()
        else {
            panic!("dropping the observer must not remove the receipt")
        };
        assert_eq!(replay.recv().unwrap(), WriteStatus::Accepted);
        assert_eq!(engine.cancel(receipt_id), Ok(CancelWriteOutcome::Cancelled));
        engine.shutdown();
    }

    #[cfg(feature = "unstable-mechanism")]
    #[test]
    fn from_parts_cannot_bypass_guard_and_spawn_failure_releases_store() {
        let fixture = tempfile::tempdir().expect("temporary directory");
        let path = fixture.path().join("from-parts.redb");
        let store = RedbStore::open(&path).expect("store must open");
        let engine = Engine::from_parts(
            store,
            nmp_router::FixtureDirectory::new(),
            10,
            PoolConfig::default(),
            nmp_engine::core::RelayAdmissionPolicy::default(),
        )
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
            nmp_router::FixtureDirectory::new(),
            usize::MAX,
            PoolConfig {
                max_relays: usize::MAX,
                ..PoolConfig::default()
            },
            nmp_engine::core::RelayAdmissionPolicy::default(),
        )
        .err()
        .expect("unrepresentable relay envelope must refuse construction");
        assert!(matches!(failure, EngineError::ThreadUnavailable { .. }));
        Engine::reset_persistent_store(&path)
            .expect("post-open spawn failure must release RedbStore ownership");
    }

    #[test]
    fn sign_event_returns_exact_verified_event_without_store_or_outbox_residue() {
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
            .add_account(&secret)
            .expect("account must register")
            .public_key();
        engine
            .set_active_account(Some(author))
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
            .expect("active local signer must complete");
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
            store.recover_outbox().is_empty(),
            "sign-only must not create an intent, receipt, or outbox lane"
        );
    }

    #[test]
    fn sign_event_rejects_missing_active_account_or_signer_before_invocation() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let active = nostr::Keys::generate().public_key();
        let request = SignEventRequest {
            created_at: nostr::Timestamp::from(1),
            kind: nostr::Kind::TextNote,
            tags: Vec::new(),
            content: "body".to_string(),
        };
        match engine.sign_event(request.clone()) {
            Err(error) => assert_eq!(error, SignEventError::NoActiveSigner),
            Ok(_) => panic!("a missing active account must refuse before acceptance"),
        }
        engine.set_active_account(Some(active)).unwrap();
        match engine.sign_event(request) {
            Err(error) => assert_eq!(error, SignEventError::NoActiveSigner),
            Ok(_) => panic!("a missing signer must refuse before acceptance"),
        }
        engine.shutdown();
    }

    struct MismatchedSigner {
        reported: PublicKey,
        actual: Keys,
        calls: Arc<AtomicUsize>,
    }

    impl nmp_signer::SigningCapability for MismatchedSigner {
        fn public_key(&self) -> Option<PublicKey> {
            Some(self.reported)
        }

        fn sign(&self, unsigned: nostr::UnsignedEvent) -> nmp_signer::SignerOp<nostr::Event> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let substituted = nostr::UnsignedEvent::new(
                self.actual.public_key(),
                unsigned.created_at,
                unsigned.kind,
                unsigned.tags,
                unsigned.content,
            );
            nmp_signer::SignerOp::ok(substituted.sign_with_keys(&self.actual).unwrap())
        }
    }

    #[test]
    fn sign_event_rejects_mismatched_signer_output() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let reported = nostr::Keys::generate();
        let calls = Arc::new(AtomicUsize::new(0));
        engine
            .add_signer(MismatchedSigner {
                reported: reported.public_key(),
                actual: nostr::Keys::generate(),
                calls: Arc::clone(&calls),
            })
            .expect("signer must register");
        engine
            .set_active_account(Some(reported.public_key()))
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
        operation: Mutex<Option<nmp_signer::SignerOp<nostr::Event>>>,
    }

    impl nmp_signer::SigningCapability for NoHookPendingSigner {
        fn public_key(&self) -> Option<PublicKey> {
            Some(self.public_key)
        }

        fn sign(&self, _unsigned: nostr::UnsignedEvent) -> nmp_signer::SignerOp<nostr::Event> {
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
        fn public_key(&self) -> Option<PublicKey> {
            Some(self.keys.public_key())
        }

        fn sign(&self, unsigned: nostr::UnsignedEvent) -> nmp_signer::SignerOp<nostr::Event> {
            let signed = unsigned.sign_with_keys(&self.keys).unwrap();
            let completion: Arc<Mutex<Option<nmp_signer::PendingSignerSender<nostr::Event>>>> =
                Arc::new(Mutex::new(None));
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
        fn public_key(&self) -> Option<PublicKey> {
            Some(self.keys.public_key())
        }

        fn sign(&self, unsigned: nostr::UnsignedEvent) -> nmp_signer::SignerOp<nostr::Event> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            nmp_signer::SignerOp::ok(unsigned.sign_with_keys(&self.keys).unwrap())
        }
    }

    #[test]
    fn sign_event_admits_then_invokes_the_signer_exactly_once() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let calls = Arc::new(AtomicUsize::new(0));
        engine
            .add_signer(CountingSigner {
                keys: keys.clone(),
                calls: Arc::clone(&calls),
            })
            .unwrap();
        engine.set_active_account(Some(keys.public_key())).unwrap();

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
        fn public_key(&self) -> Option<PublicKey> {
            Some(self.public_key)
        }

        fn sign(&self, _unsigned: nostr::UnsignedEvent) -> nmp_signer::SignerOp<nostr::Event> {
            let producer: Arc<Mutex<Option<nmp_signer::PendingSignerSender<nostr::Event>>>> =
                Arc::new(Mutex::new(None));
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
            .add_signer(PendingSigner {
                public_key: keys.public_key(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.set_active_account(Some(keys.public_key())).unwrap();

        let publish = |content: &str| {
            engine
                .publish_tracked(WriteIntent {
                    payload: nmp_grammar::WritePayload::Unsigned(UnsignedEvent::new(
                        keys.public_key(),
                        Timestamp::from(10),
                        Kind::TextNote,
                        Vec::new(),
                        content,
                    )),
                    durability: nmp_grammar::Durability::Durable,
                    routing: nmp_grammar::WriteRouting::AuthorOutbox,
                    identity_override: None,
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
        assert_eq!(first.statuses.recv().unwrap(), WriteStatus::Accepted);
        assert_eq!(engine.cancel(first.id), Ok(CancelWriteOutcome::Cancelled));
        wait_for_cancellations(1);

        let second = publish("a second write cancels the same way");
        assert_eq!(second.statuses.recv().unwrap(), WriteStatus::Accepted);
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
            .add_signer(PendingSigner {
                public_key: keys.public_key(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.set_active_account(Some(keys.public_key())).unwrap();
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
            .add_signer(PendingSigner {
                public_key: keys.public_key(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.set_active_account(Some(keys.public_key())).unwrap();
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
            .add_signer(NoHookPendingSigner {
                public_key: keys.public_key(),
                operation: Mutex::new(Some(operation)),
            })
            .unwrap();
        engine.set_active_account(Some(keys.public_key())).unwrap();
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
            .add_signer(NoHookPendingSigner {
                public_key: keys.public_key(),
                operation: Mutex::new(Some(operation)),
            })
            .unwrap();
        engine.set_active_account(Some(keys.public_key())).unwrap();
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
            .add_signer(HookCompletesSigner {
                keys: keys.clone(),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.set_active_account(Some(keys.public_key())).unwrap();
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
        assert_eq!(via_hex.public_key(), keys.public_key());

        let via_nsec = engine
            .add_account(
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
    fn add_account_rejects_malformed_secret_key() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        assert_eq!(
            engine.add_account("not-a-key"),
            Err(EngineError::InvalidSecretKey)
        );
        engine.shutdown();
    }

    /// #8's ratified account model plus the #529 falsifier folded here: a
    /// same-key replacement invalidates the prior exact instance, a stale
    /// registration's removal is a `false` no-op that can never detach the
    /// replacement, and removal is idempotent per exact instance.
    #[test]
    fn account_registration_is_exact_instance_repeatable_and_stale_safe() {
        let engine = Engine::new(EngineConfig {
            max_auth_capabilities: 1,
            ..EngineConfig::default()
        })
        .expect("engine must build");
        let keys = Keys::generate();
        let first = engine
            .add_account(&keys.secret_key().to_secret_hex())
            .expect("first account must register");
        let replacement = engine
            .add_account(&keys.secret_key().to_secret_hex())
            .expect("same-key replacement must not consume another slot");

        assert_eq!(first.public_key(), replacement.public_key());
        assert_ne!(
            first, replacement,
            "a replacement must be a NEW exact instance, never equal to the stale one"
        );
        assert_eq!(
            first.clone(),
            first,
            "clones of one registration share its exact instance identity"
        );
        assert!(
            !engine.remove_account(&first).unwrap(),
            "a stale registration must no-op instead of detaching its replacement"
        );
        assert!(engine.remove_account(&replacement).unwrap());
        assert!(
            !engine.remove_account(&replacement).unwrap(),
            "removal is exact-instance idempotent"
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
                .add_account(&Keys::generate().secret_key().to_secret_hex())
                .err(),
            Some(EngineError::AuthCapabilityRegistryFull { limit: 0 })
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
            .add_account(&keys.secret_key().to_secret_hex())
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
            .add_account(&keys.secret_key().to_secret_hex())
            .expect("account must register");
        let policy = engine
            .add_auth_policy(keys.public_key(), AllowAuthPolicy)
            .expect("policy must register");
        engine.shutdown();

        assert_eq!(
            engine.remove_account(&account).err(),
            Some(EngineError::EngineClosed)
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
    fn sign_event_uses_the_active_account_without_publishing() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys = Keys::generate();
        let pubkey = engine
            .add_account(&keys.secret_key().to_secret_hex())
            .expect("account must register")
            .public_key();
        engine
            .set_active_account(Some(pubkey))
            .expect("account must activate");

        let signed = engine
            .sign_event(SignEventRequest {
                created_at: Timestamp::from(1_750_000_000),
                kind: Kind::Custom(27_235),
                tags: vec![Tag::parse(["client", "nip07-test"]).expect("valid tag")],
                content: "sign without publish".to_string(),
            })
            .expect("active local signer must start")
            .recv()
            .expect("active local signer must sign");

        assert_eq!(signed.pubkey, pubkey);
        assert_eq!(signed.created_at, Timestamp::from(1_750_000_000));
        assert_eq!(signed.kind, Kind::Custom(27_235));
        assert_eq!(signed.content, "sign without publish");
        assert!(signed.verify().is_ok());
        engine.shutdown();
    }

    #[test]
    fn sign_event_without_an_active_account_fails_closed() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let result = engine.sign_event(SignEventRequest {
            created_at: Timestamp::from(1_750_000_000),
            kind: Kind::TextNote,
            tags: Vec::new(),
            content: "unsigned".to_string(),
        });
        match result {
            Err(error) => assert_eq!(error, SignEventError::NoActiveSigner),
            Ok(_) => panic!("a missing active account must fail closed"),
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

        let rx = engine
            .publish(WriteIntent {
                payload: WritePayload::Signed(event),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
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

    /// #47 falsifier (a) through the facade: with account A active and B
    /// merely registered ([`Engine::add_account`], never activated), a
    /// B-authored draft carrying `identity_override: Some(B)` reaches
    /// `WriteStatus::Signed` bearing the exact id of the frozen B-authored
    /// body -- which commits cryptographically to author and content --
    /// and [`Engine::active_account`] still answers A afterward: the
    /// override consented to ONE write, it never re-rooted the engine.
    #[test]
    fn identity_override_publishes_as_secondary_without_moving_active_account() {
        let engine = Engine::new(EngineConfig::default()).expect("engine must build");
        let keys_a = Keys::generate();
        let keys_b = Keys::generate();
        let pk_a = engine
            .add_account(&keys_a.secret_key().to_secret_hex())
            .expect("account A must register")
            .public_key();
        let pk_b = engine
            .add_account(&keys_b.secret_key().to_secret_hex())
            .expect("account B must register")
            .public_key();
        engine
            .set_active_account(Some(pk_a))
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
                payload: WritePayload::Unsigned(draft),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: Some(pk_b),
            })
            .expect("engine is open");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut signed_as_b = false;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(WriteStatus::Signed(id)) => {
                    assert_eq!(
                        id, expected.id,
                        "Signed must carry the frozen B-authored body's exact id"
                    );
                    signed_as_b = true;
                    break;
                }
                Ok(WriteStatus::Failed(reason)) => {
                    panic!("override publish must not fail pre-routing: {reason}")
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(signed_as_b, "override publish must reach Signed as B");
        assert_eq!(
            engine.active_account().expect("engine is open"),
            Some(pk_a),
            "the per-write override must never move the active account"
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
            identity_override: None,
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
                    Some(nmp_engine::core::WindowLoad::Returned { .. })
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
            nmp_engine::core::WindowLoad::Returned { added: 1 }
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
                allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
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
        // without blocking, and the subscription is closed.
        assert!(subscription.recv().is_err());

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
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
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
        assert_eq!(while_callers_retain.admitted_waiters, 0);

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
        let mut limited = probe_query();
        limited.0.selection.limit = Some(3);
        assert_eq!(
            engine.observe(limited, Some(window_probe())).err(),
            Some(EngineError::WindowSelectionHasLimit)
        );
        engine.shutdown();
    }
}
