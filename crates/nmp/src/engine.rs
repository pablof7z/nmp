//! [`Engine`] -- the one supported construction call plus the two nouns
//! (#52 §1). Owns config -> store/routing-fact
//! selection and the router cap `nmp-ffi` used to duplicate by hand.
//!
//! No `Signed`-payload verify lives here: that guarantee moved to
//! `nmp_engine::core::EngineCore::on_publish`'s acceptance boundary (Unit
//! A0, #56) precisely so it holds for every entry point -- this facade,
//! `nmp-ffi`, and any `from_parts`/raw-`EngineThread` caller alike -- not
//! only the one that happens to verify locally. See [`crate::error`]'s doc.
//!
//! ## The serialized lifecycle gate
//!
//! `inner` holds `Some(Inner)` while the engine is open, `None` once
//! [`Engine::shutdown`] has run. Ordinary verbs take the SAME mutex, check
//! that state, and run their short `Handle` call while still holding the
//! lock. Publication is the deliberate exception: it clones the open handle
//! under the lock, then releases the lock while pre-custody capability code
//! may be preparing a complete event. Shutdown can therefore take and join
//! the engine while that preparation remains blocked. The raw publication
//! handle maps a closed command or reply channel to
//! [`EngineError::EngineClosed`], so the race is typed rather than a panic or
//! a fabricated receipt. `Engine`'s `Drop` calls `shutdown` too, so a
//! dropped-without-`shutdown` `Engine` still tears down `EngineThread`
//! cleanly rather than detaching it.

mod observation;
mod publication;
mod relay_information;
mod session;

pub use publication::{CancelWriteError, CancelWriteOutcome};
pub use relay_information::RelayInformationRequestError;

use std::sync::Mutex;

#[cfg(feature = "test-instrumentation")]
use nmp_runtime::{AddSignerError, SignerRegistration};
use nmp_runtime::{EngineThread, Handle, RuntimeConfig, SignEventError, SignEventOperation};
use nmp_store::{RedbStore, RedbStoreOpenError, RedbStoreResetError};
use nmp_transport::PoolConfig;
use nostr::{Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

use crate::auth::{AuthPolicy, EngineAuthPolicyAdapter};
use crate::config::{build_routing_fact_relays, EngineConfig};

use crate::error::EngineError;
use crate::subscription::{AsyncDiagnosticsSubscription, DiagnosticsSubscription};

/// The open state: the `Handle` verbs are driven through, plus the
/// `EngineThread` `shutdown` eventually joins. Not `Clone` (`EngineThread`
/// isn't), so it lives behind `Engine`'s own mutex rather than a
/// `Mutex<Option<EngineThread>>` alongside a separately-held `Handle`.
struct Inner {
    handle: Handle,
    engine_thread: EngineThread,
}

/// The one supported Rust product surface (#52 §1).
/// Owns the `EngineThread` + `Handle` pair; every mechanism crate
/// (`nmp-store`/`nmp-router`/`nmp-transport`/`nmp-resolver`) is reached only
/// through here. See this module's doc for the serialized lifecycle gate
/// `inner` implements.
pub struct Engine {
    inner: Mutex<Option<Inner>>,
}

/// Opaque ownership proof for one exact AUTH-policy installation (#8).
/// Replacement invalidates it, and a stale clone cannot detach the
/// replacement.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthPolicyRegistration {
    inner: nmp_runtime::AuthPolicyRegistration,
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

impl Engine {
    #[cfg(feature = "test-instrumentation")]
    #[doc(hidden)]
    pub fn install_test_signing_capability<Sig>(
        &self,
        signer: Sig,
    ) -> Result<SignerRegistration, AddSignerError>
    where
        Sig: nmp_signer::SigningCapability + Send + Sync + 'static,
    {
        self.with_handle(|handle| handle.add_signer(signer))
            .unwrap_or(Err(AddSignerError::EngineShuttingDown))
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
    ///
    /// Per #1624, normal Rust construction USED TO include the
    /// feature-selected replaceable-capability built-ins `nmp` itself owned
    /// (NIP-29's group-list capability, compiled in whenever the `nip29`
    /// Cargo feature was enabled). #1707 removed the last of those: `nmp`
    /// does not own any capability's meaning any more, NIP-29 included, so
    /// it cannot auto-register one. Every capability -- NIP-02's
    /// `nmp_nip02::follow_capability()`, NIP-29's
    /// `nmp_nip29::group_list_capability()`, any future one -- is supplied
    /// explicitly at the consumer boundary, in the vec passed to
    /// [`Engine::new_with_capabilities`]: the FFI facade already does this
    /// unconditionally, and a direct-Rust app does the same.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        Self::new_with_capabilities(config, Vec::new())
    }

    /// Construct an engine with the complete compiled replaceable-capability
    /// set available before store recovery. A retained operation whose
    /// program/format is absent from `capabilities` refuses open and leaves
    /// the store unchanged.
    pub fn new_with_capabilities(
        config: EngineConfig,
        capabilities: Vec<crate::ReplaceableMaterializerSpec>,
    ) -> Result<Self, EngineError> {
        Self::new_with_capabilities_and_routing(config, capabilities, None)
    }

    /// Construct an engine that discovers author routes with the algorithm
    /// the application chose.
    ///
    /// `route_provider` is an [`AuthorRouteProvider`] implementation, exactly
    /// like the capability vec beside it: NMP compiles in no algorithm of its
    /// own and installs no default. `nmp_outbox::Nip65Outbox::new(indexers)`
    /// is the NIP-65 outbox model this workspace ships; a different outbox
    /// algorithm is a different crate, and this facade names neither.
    ///
    /// `None` discovers nothing: every author stays `Unknown`, operator lanes
    /// and explicit routes carry everything they carry, and an `Auto` write
    /// whose author is unknown parks on knowledge rather than failing.
    ///
    /// The choice is fixed for this engine's life. There is deliberately no
    /// way to install, replace, or remove a provider on a running engine --
    /// "swap algorithms" is spelled: shut this engine down and construct
    /// another one.
    pub fn new_with_capabilities_and_routing(
        config: EngineConfig,
        capabilities: Vec<crate::ReplaceableMaterializerSpec>,
        route_provider: Option<Box<dyn crate::AuthorRouteProvider>>,
    ) -> Result<Self, EngineError> {
        Self::new_with_initial_session(
            config,
            nmp_runtime::session::RestoredSession::empty(),
            capabilities,
            route_provider,
        )
    }

    pub(crate) fn new_with_initial_session(
        config: EngineConfig,
        initial_session: nmp_runtime::session::RestoredSession,
        capabilities: Vec<crate::ReplaceableMaterializerSpec>,
        route_provider: Option<Box<dyn crate::AuthorRouteProvider>>,
    ) -> Result<Self, EngineError> {
        let (app_relays, fallback_relays) = build_routing_fact_relays(&config)?;
        // #1624: capability identity is (program, format). A second spec for
        // the same pair is a construction error, not a replacement (the
        // replacement lifecycle is gone). Refuse before any engine thread
        // starts or the store is touched.
        {
            let mut seen = std::collections::BTreeSet::new();
            for spec in &capabilities {
                let key = (spec.program(), spec.format());
                if !seen.insert(key) {
                    return Err(EngineError::DuplicateReplaceableCapability {
                        program: spec.program(),
                        format: spec.format(),
                    });
                }
            }
        }
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
            app_relays,
            fallback_relays,
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
                EngineThread::spawn_with_runtime_config_and_session(
                    store,
                    config.max_relays,
                    pool_config,
                    runtime_config,
                    initial_session,
                    capabilities,
                    route_provider,
                )
                .map_err(EngineError::from_start_error)?
            }
            None => {
                let store =
                    RedbStore::temporary().map_err(|error| EngineError::StoreOpenFailed {
                        reason: error.to_string(),
                    })?;
                EngineThread::spawn_with_runtime_config_and_session(
                    store,
                    config.max_relays,
                    pool_config,
                    runtime_config,
                    initial_session,
                    capabilities,
                    route_provider,
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
    ) -> Result<nmp_runtime::SignEventCancel, SignEventError> {
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

    /// Stop the engine. Idempotent: a second call (or a call racing another
    /// thread's call) finds `inner` already `None` and no-ops. No call that
    /// starts after this one completes can reach the raw
    /// `Handle`/`EngineThread`; a publication that cloned its handle earlier
    /// either already entered custody or receives `EngineClosed` while the
    /// shutdown drain joins the engine -- see this module's doc.
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

