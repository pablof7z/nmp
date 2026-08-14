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
//! `nmp-ffi` is now only config/type mirroring plus native object handles over
//! `nmp`'s async pull APIs. Long-lived observations expose `next()` and
//! `cancel()` directly; no drain-thread/callback-observer bridge remains.
//!
//! Neutral routing facts are assembled privately by `nmp::Engine`; this
//! facade forwards operator configuration and never exposes a mutable
//! directory.

use std::sync::Arc;

use crate::auth::{FfiAuthPolicyAdapter, FfiAuthPolicyCallback, FfiAuthPolicyRegistration};
use crate::convert::{
    parse_pubkey, relay_information_error_kind, sign_event_request_from_ffi,
    sign_event_start_error, FfiError,
};
#[cfg(test)]
use crate::convert::{FfiRequestRowsError, FfiRowPullError};
#[cfg(feature = "nip02")]
use crate::nip02::{NmpFollowActionStream, NmpFollowStream};
use crate::session::{
    FfiPrivateKey, FfiPublicKey, FfiSessionAccount, FfiSessionPayload, FfiSessionSnapshot,
};
#[cfg(test)]
use crate::types::{
    FfiCancelWriteError, FfiCancelWriteOutcome, FfiReceiptReattachment, FfiWriteIntent,
};
use crate::types::{
    FfiRelayInformation, FfiRelayInformationCachePolicy, FfiRelayInformationDocument,
    FfiRelayInformationFreshness, FfiRelayInformationLimitations, FfiSignEventRequest,
};

/// Start a follow/unfollow action and expose its status stream (#680/#704). A
/// valid target starts an async action task on the shared runtime; an
/// unparseable target yields a one-shot stream carrying a single
/// `Failed(InvalidTarget)` fact. The status FIFO is delivered pull-based over
/// [`NmpFollowActionStream`], so no worker or drain thread is held by the
/// action.
#[cfg(feature = "nip02")]
fn start_following_action(
    engine: Arc<nmp::Engine>,
    target: String,
    change: nmp_nip02::FollowChange,
) -> Arc<NmpFollowActionStream> {
    let action = match parse_pubkey(&target) {
        Ok(target) => nmp_nip02::set_following(engine, target, change),
        Err(_) => nmp_nip02::FollowAction::one_shot_failure(
            nmp_nip02::FollowActionFailure::InvalidTarget { got: target },
        ),
    };
    NmpFollowActionStream::new(action.into_async())
}

/// Construction config for [`NmpEngine::new`]. Build-time feature selection
/// controls which fields exist; runtime relay values remain app-owned inputs.
#[derive(uniffi::Record, Clone, Debug)]
pub struct NmpEngineConfig {
    /// `None` -> an engine-owned temporary Redb store (nothing survives the
    /// engine's lifetime). `Some(path)` -> a persistent `RedbStore` opened at that path (the same file
    /// reopened across restarts is what preserves source-scoped evidence for
    /// a cold, offline read -- ledger #7).
    pub store_path: Option<String>,
    /// Operator app relay set (`Lane::OperatorApp`). Default empty.
    pub app_relays: Vec<String>,
    /// Operator fallback relay set (`Lane::OperatorFallback`). Default empty.
    pub fallback_relays: Vec<String>,
    /// Optional runtime assembly for outbox routing. This field exists only
    /// in a native build that selected the `outbox routing` capability.
    /// `None` constructs an explicit-routing-only engine; `Some` must name at
    /// least one app-owned indexer relay or construction is refused.
    #[cfg(feature = "nip65")]
    #[uniffi(default = None)]
    pub outbox_routing: Option<FfiOutboxRoutingConfig>,
    /// The one whole-engine relay ceiling. It bounds the complete compiled
    /// demand and simultaneous physical transport workers with the same
    /// effective value. Access contexts never share a socket; competing read
    /// and write contexts for the same admitted relay time-share its slot and
    /// the read is restored afterward, so apps do not multiply this value per
    /// context (#598). Legacy zero is normalized to the finite default, never
    /// uncapped.
    ///
    /// The `default =` literal below MUST stay equal to
    /// [`DEFAULT_MAX_RELAYS`] (uniffi record defaults accept only a literal,
    /// never a const path) — the const is the single Rust-side knob; the
    /// literal is its foreign-binding mirror.
    #[uniffi(default = 10)]
    pub max_relays: u32,
    /// Maximum live signer and AUTH-policy registrations. Zero deliberately
    /// admits none rather than selecting the default.
    #[uniffi(default = 64)]
    pub max_auth_capabilities: u32,
}

/// App-owned runtime inputs for outbox routing. These values do not
/// participate in the native artifact's feature or cache identity.
#[cfg(feature = "nip65")]
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiOutboxRoutingConfig {
    /// Relays queried for kind:10002 relay lists. NMP supplies no defaults.
    pub indexers: Vec<String>,
}

/// The default relay-count ceiling for a freshly-constructed engine config
/// (#20). Update BOTH this const AND the `#[uniffi(default = N)]` literal
/// on [`NmpEngineConfig::max_relays`] above — they must match.
pub const DEFAULT_MAX_RELAYS: u32 = 10;
pub const DEFAULT_MAX_AUTH_CAPABILITIES: u32 = 64;

// A DERIVED `Default` would zero `max_auth_capabilities` — and zero
// deliberately admits NO capability registrations — so the Rust-side
// default is written out by hand to mirror every `#[uniffi(default = …)]`
// literal above exactly.
impl Default for NmpEngineConfig {
    fn default() -> Self {
        Self {
            store_path: None,
            app_relays: Vec::new(),
            fallback_relays: Vec::new(),
            #[cfg(feature = "nip65")]
            outbox_routing: None,
            max_relays: DEFAULT_MAX_RELAYS,
            max_auth_capabilities: DEFAULT_MAX_AUTH_CAPABILITIES,
        }
    }
}

/// Destructively reset a closed persistent NMP store. This removes all
/// canonical engine state at `store_path`. A live engine in this OR
/// ANY OTHER process using the same canonical path is refused with
/// `FfiError::StoreStillOpen` without touching the file. Shut down or drop
/// that engine first. The operation is idempotent when the store does not
/// exist.
#[uniffi::export]
pub fn reset_persistent_store(store_path: String) -> Result<(), FfiError> {
    nmp::Engine::reset_persistent_store(store_path)?;
    Ok(())
}

// Keep the native-facing literal pinned to the canonical finite default.
const _: () = assert!(DEFAULT_MAX_RELAYS == 10);
const _: () = assert!(DEFAULT_MAX_AUTH_CAPABILITIES == 64);

impl From<NmpEngineConfig> for nmp::EngineConfig {
    fn from(config: NmpEngineConfig) -> Self {
        nmp::EngineConfig {
            store_path: config.store_path,
            #[cfg(feature = "nip65")]
            indexer_relays: config
                .outbox_routing
                .map(|outbox_routing| outbox_routing.indexers)
                .unwrap_or_default(),
            #[cfg(not(feature = "nip65"))]
            indexer_relays: Vec::new(),
            app_relays: config.app_relays,
            fallback_relays: config.fallback_relays,
            max_publish_attempts: nmp::DEFAULT_MAX_PUBLISH_ATTEMPTS,
            max_relays: config.max_relays as usize,
            max_auth_capabilities: config.max_auth_capabilities as usize,
        }
    }
}

/// The UniFFI-exported engine object. `new` is the ONE construction call the
/// M4 kill test (plan §7) requires -- everything past construction is a
/// method call on this object, never a second container the app must adopt.
/// Wraps a single [`nmp::Engine`] -- the one supported Rust product API
/// -- rather than independently assembling `nmp-store`/`nmp-router`/
/// `nmp-transport`/`nmp-resolver` mechanism types (#52).
#[derive(uniffi::Object)]
pub struct NmpEngine {
    pub(crate) engine: Arc<nmp::Engine>,
    #[cfg(feature = "nip65")]
    automatic_routing: AutomaticRoutingAssembly,
}

#[cfg(feature = "nip65")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutomaticRoutingAssembly {
    Unavailable,
    Nip65,
}

#[cfg(feature = "nip65")]
impl AutomaticRoutingAssembly {
    fn from_config(config: &NmpEngineConfig) -> Result<Self, FfiError> {
        match &config.outbox_routing {
            None => Ok(Self::Unavailable),
            Some(outbox_routing) if outbox_routing.indexers.is_empty() => {
                Err(FfiError::OutboxRoutingIndexersEmpty)
            }
            Some(_) => Ok(Self::Nip65),
        }
    }
}

#[uniffi::export]
impl NmpEngine {
    /// Explicit one-shot NIP-11 acquisition. Fresh reads are cache hits;
    /// concurrent misses/refreshes share one bounded engine-owned flight.
    pub async fn relay_information(
        &self,
        relay: String,
        policy: FfiRelayInformationCachePolicy,
    ) -> Result<FfiRelayInformation, FfiError> {
        let policy = match policy {
            FfiRelayInformationCachePolicy::UseCache => nmp::RelayInformationCachePolicy::UseCache,
            FfiRelayInformationCachePolicy::Refresh => nmp::RelayInformationCachePolicy::Refresh,
        };
        let value = self.engine.relay_information(&relay, policy).await?;
        let document = value.document();
        Ok(FfiRelayInformation {
            relay: value.relay().to_string(),
            document: FfiRelayInformationDocument {
                name: document.name.clone(),
                description: document.description.clone(),
                banner: document.banner.clone(),
                icon: document.icon.clone(),
                pubkey: document.pubkey.clone(),
                self_pubkey: document.self_pubkey.clone(),
                contact: document.contact.clone(),
                supported_nips: document.supported_nips.clone(),
                software: document.software.clone(),
                version: document.version.clone(),
                terms_of_service: document.terms_of_service.clone(),
                limitation: FfiRelayInformationLimitations {
                    max_message_length: document.limitation.max_message_length,
                    max_subscriptions: document.limitation.max_subscriptions,
                    max_filters: document.limitation.max_filters,
                    max_limit: document.limitation.max_limit,
                    max_subid_length: document.limitation.max_subid_length,
                    max_event_tags: document.limitation.max_event_tags,
                    max_content_length: document.limitation.max_content_length,
                    min_pow_difficulty: document.limitation.min_pow_difficulty,
                    auth_required: document.limitation.auth_required,
                    payment_required: document.limitation.payment_required,
                    created_at_lower_limit: document.limitation.created_at_lower_limit,
                    created_at_upper_limit: document.limitation.created_at_upper_limit,
                },
                structured: document.structured.clone().into_iter().collect(),
            },
            raw_json: value.raw_json().to_owned(),
            document_revision: value.document_revision().to_owned(),
            fetched_at: value.fetched_at(),
            fresh_until: value.fresh_until(),
            freshness: match value.freshness() {
                nmp::RelayInformationFreshness::Fresh => FfiRelayInformationFreshness::Fresh,
                nmp::RelayInformationFreshness::Stale => FfiRelayInformationFreshness::Stale,
            },
            etag: value.etag().map(str::to_owned),
            last_modified: value.last_modified().map(str::to_owned),
            cache_control: value.cache_control().map(str::to_owned),
            expires: value.expires().map(str::to_owned),
            last_error: value
                .last_error()
                .cloned()
                .map(relay_information_error_kind),
        })
    }

    #[uniffi::constructor]
    pub fn new(
        config: NmpEngineConfig,
        session_payload: Option<Arc<FfiSessionPayload>>,
    ) -> Result<Arc<Self>, FfiError> {
        #[cfg(feature = "nip65")]
        let automatic_routing = AutomaticRoutingAssembly::from_config(&config)?;
        let engine = Arc::new(match session_payload {
            Some(payload) => nmp::Engine::new_with_session(config.into(), payload.payload())
                .map_err(FfiError::from)?,
            None => nmp::Engine::new(config.into())?,
        });
        Ok(Arc::new(Self {
            engine,
            #[cfg(feature = "nip65")]
            automatic_routing,
        }))
    }

    pub fn session(&self) -> Result<FfiSessionSnapshot, FfiError> {
        Ok(FfiSessionSnapshot::from(self.engine.session()?))
    }

    pub fn export_session(&self) -> Result<Arc<FfiSessionPayload>, FfiError> {
        Ok(FfiSessionPayload::from_payload(
            self.engine.export_session()?,
        ))
    }

    pub fn add_private_key_account(
        &self,
        private_key: Arc<FfiPrivateKey>,
        make_current: bool,
    ) -> Result<Arc<FfiSessionAccount>, FfiError> {
        // The FFI key and the engine's local provider are each canonical
        // zeroizing owners. The unavoidable duplicate lasts only until the
        // caller releases this transport key; neither owner exposes bytes.
        self.engine
            .add_private_key_account(private_key.secret_bytes(), make_current)
            .map(FfiSessionAccount::from_account)
            .map_err(FfiError::from)
    }

    pub fn add_public_key_account(
        &self,
        public_key: Arc<FfiPublicKey>,
        make_current: bool,
    ) -> Result<Arc<FfiSessionAccount>, FfiError> {
        self.engine
            .add_public_key_account(public_key.inner, make_current)
            .map(FfiSessionAccount::from_account)
            .map_err(FfiError::from)
    }

    pub fn make_current_account(&self, account: Arc<FfiSessionAccount>) -> Result<(), FfiError> {
        self.engine
            .make_current_account(account.inner.public_key)
            .map_err(FfiError::from)
    }

    pub fn remove_account(&self, account: Arc<FfiSessionAccount>) -> Result<bool, FfiError> {
        self.engine
            .remove_account(&account.inner)
            .map_err(FfiError::from)
    }

    pub fn clear_session(&self) -> Result<(), FfiError> {
        self.engine.clear_session().map_err(FfiError::from)
    }

    /// Install a native-owned authorization policy for one exact account.
    /// The callback may resolve inline or retain the supplied completion.
    pub fn add_auth_policy(
        &self,
        expected_public_key: String,
        callback: Box<dyn FfiAuthPolicyCallback>,
    ) -> Result<Arc<FfiAuthPolicyRegistration>, FfiError> {
        let expected_public_key = parse_pubkey(&expected_public_key)?;
        let registration = self
            .engine
            .add_auth_policy(expected_public_key, FfiAuthPolicyAdapter::new(callback))?;
        Ok(Arc::new(FfiAuthPolicyRegistration {
            inner: registration,
        }))
    }

    /// Remove only the policy installation proven by `registration`.
    pub fn remove_auth_policy(
        &self,
        registration: Arc<FfiAuthPolicyRegistration>,
    ) -> Result<bool, FfiError> {
        Ok(self.engine.remove_auth_policy(&registration.inner)?)
    }

    /// Sign one exact event through the current account without accepting a
    /// write, persisting a row/receipt, planning relays, or publishing. The
    /// returned [`NmpSignEventHandle`] delivers the outcome once through its
    /// `async fn signed()`; [`NmpSignEventHandle::cancel`] cancels only this
    /// signer operation.
    pub fn sign_event(
        &self,
        event: FfiSignEventRequest,
    ) -> Result<Arc<NmpSignEventHandle>, FfiError> {
        let request = sign_event_request_from_ffi(event)?;
        // One-shot result channel: the engine-admitted completion fires exactly
        // once (success / failure / cancellation), sending the result and then
        // dropping the sender so `signed()`'s awaited FIFO ends after it.
        let (sender, receiver) = nmp::fifo_channel::<Result<nmp::Event, nmp::SignEventError>>();
        let cancel = self
            .engine
            .sign_event_with_completion(request, move |result| {
                sender.send(result);
            })
            .map_err(sign_event_start_error)?;
        Ok(Arc::new(NmpSignEventHandle {
            cancel,
            result: receiver.into_async(),
        }))
    }

    /// Open a live diagnostics stream (#680) -- "the acceptance test rendered
    /// on screen, permanently." Delivery is pull-based: await
    /// [`NmpDiagnosticsStream::next`], which parks a waker on the engine's
    /// latest-state diagnostics mailbox — no dedicated drain thread. The
    /// returned stream's `Drop` withdraws the observer; call
    /// [`NmpDiagnosticsStream::cancel`] for an explicit early teardown. The
    /// first `next()` yields the CURRENT snapshot immediately, then a fresh one
    /// on every recompile/EOSE-driven coverage change. `None` is the terminal
    /// signal (cancel / engine shutdown).
    pub fn observe_diagnostics(&self) -> Result<Arc<NmpDiagnosticsStream>, FfiError> {
        let subscription = self.engine.observe_diagnostics_async()?;
        Ok(Arc::new(NmpDiagnosticsStream {
            inner: subscription,
        }))
    }

    /// Stop the engine. Idempotent: a second call is a no-op (`nmp::Engine`'s
    /// own serialized lifecycle gate, see that type's doc).
    pub fn shutdown(&self) {
        self.engine.shutdown();
    }
}

#[cfg(feature = "nip02")]
#[uniffi::export]
impl NmpEngine {
    /// Observe the current account's relationship to `target` through the
    /// NMP-owned NIP-02 resource (#680). Awaiting [`NmpFollowStream::next`]
    /// costs no NMP-owned OS thread: the relationship snapshot is folded inline
    /// over the engine's waker-driven async row mailbox. Contact-list
    /// semantics and acquisition state stay in Rust and arrive as complete
    /// self-contained snapshots.
    pub fn observe_following(&self, target: String) -> Result<Arc<NmpFollowStream>, FfiError> {
        let target = parse_pubkey(&target)?;
        let observation = nmp_nip02::observe_following_async(self.engine.clone(), target)?;
        Ok(NmpFollowStream::new(observation))
    }

    /// Ask NMP to follow `target`. This is the complete NIP-02 action.
    pub fn follow(&self, target: String) -> Result<Arc<NmpFollowActionStream>, FfiError> {
        if self.automatic_routing == AutomaticRoutingAssembly::Unavailable {
            return Err(FfiError::AutomaticRoutingUnavailable);
        }
        Ok(start_following_action(
            self.engine.clone(),
            target,
            nmp_nip02::FollowChange::Follow,
        ))
    }

    /// The inverse of [`Self::follow`], with the same acquisition,
    /// compare-and-swap, signer, routing, and receipt guarantees.
    pub fn unfollow(&self, target: String) -> Result<Arc<NmpFollowActionStream>, FfiError> {
        if self.automatic_routing == AutomaticRoutingAssembly::Unavailable {
            return Err(FfiError::AutomaticRoutingUnavailable);
        }
        Ok(start_following_action(
            self.engine.clone(),
            target,
            nmp_nip02::FollowChange::Unfollow,
        ))
    }
}

mod diagnostics_stream;
pub use diagnostics_stream::NmpDiagnosticsStream;

mod receipt_stream;
pub use receipt_stream::NmpReceiptStream;

mod row_stream;
pub use row_stream::{NmpRowPull, NmpRowStream};

mod sign_event_handle;
pub use sign_event_handle::NmpSignEventHandle;

#[cfg(test)]
mod tests;
