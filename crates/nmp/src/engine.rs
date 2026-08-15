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

#[cfg(test)]
use crate::core::ReceiptId;
#[cfg(test)]
use crate::runtime::ReceiptReattachment;
#[cfg(any(test, feature = "test-instrumentation"))]
use crate::runtime::{AddSignerError, SignerRegistration};
use crate::runtime::{EngineThread, Handle, RuntimeConfig, SignEventError, SignEventOperation};
#[cfg(test)]
use crate::subscription::{Subscription, Window};
#[cfg(test)]
use nmp_grammar::LiveQuery;
#[cfg(test)]
use nmp_grammar::WriteIntent;
use nmp_store::{RedbStore, RedbStoreOpenError, RedbStoreResetError};
use nmp_transport::PoolConfig;
use nostr::secp256k1::rand::{rngs::OsRng, RngCore};
#[cfg(test)]
use nostr::EventId;
#[cfg(any(test, all(feature = "unstable-mechanism", feature = "nip65")))]
use nostr::RelayUrl;
use nostr::{Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

use crate::auth::{AuthPolicy, EngineAuthPolicyAdapter};
#[cfg(feature = "nip65")]
use crate::config::build_nip65_sources;
use crate::config::{build_routing_facts, EngineConfig};
use crate::error::EngineError;
#[cfg(test)]
use crate::relay_information::{RelayInformationCachePolicy, RelayInformationError};
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

    #[cfg(any(test, feature = "test-instrumentation"))]
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

#[cfg(test)]
mod tests;
