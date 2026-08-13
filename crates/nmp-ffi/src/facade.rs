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

use std::sync::{Arc, Mutex};

use crate::auth::{FfiAuthPolicyAdapter, FfiAuthPolicyCallback, FfiAuthPolicyRegistration};
use crate::convert::{
    cancel_write_error_to_ffi, cancel_write_outcome_to_ffi, diagnostics_snapshot_to_ffi,
    filter_from_ffi, frame_to_ffi, live_query_from_ffi, parse_event_id, parse_pubkey,
    publish_queue_entry_to_ffi, publish_queue_error_to_ffi, receipt_result_to_ffi,
    relay_information_error_kind, remove_queue_entry_error_to_ffi, sign_event_failure,
    sign_event_request_from_ffi, sign_event_start_error, signed_event_to_ffi, window_from_ffi,
    write_intent_from_ffi, write_status_to_ffi, FfiError, FfiRequestRowsError, FfiRowPullError,
    WriteStatusRef,
};
#[cfg(feature = "nip02")]
use crate::nip02::{NmpFollowActionStream, NmpFollowStream};
use crate::session::{
    FfiPrivateKey, FfiPublicKey, FfiSessionAccount, FfiSessionPayload, FfiSessionSnapshot,
};
use crate::types::{
    FfiCancelWriteError, FfiCancelWriteOutcome, FfiCorrelationReattachment, FfiDiagnosticsSnapshot,
    FfiFilter, FfiFrame, FfiLiveQuery, FfiPublishQueueEntry, FfiPublishQueueError,
    FfiReceiptReattachment, FfiReceiptResult, FfiRelayInformation, FfiRelayInformationCachePolicy,
    FfiRelayInformationDocument, FfiRelayInformationFreshness, FfiRelayInformationLimitations,
    FfiRemoveQueueEntryError, FfiSignEventFailure, FfiSignEventRequest, FfiSignedEvent, FfiWindow,
    FfiWriteFact, FfiWriteIntent,
};
use nmp::ReceiptReattachment;

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

    /// Open a live subscription (#680). Delivery is pull-based: await
    /// [`NmpRowStream::next`], which parks a waker on the engine-owned mailbox
    /// rather than blocking a dedicated OS thread — opening one costs no native
    /// thread. `None` from `next()` is the terminal signal (cancel / engine
    /// shutdown / producer drop). The returned [`NmpRowStream`]'s `Drop`
    /// withdraws the subscription; call [`NmpRowStream::cancel`] for an
    /// explicit early teardown.
    ///
    /// `window` selects the observation's delivery policy (#485). `None` is
    /// today's unbounded observation: exact deltas are rebased when a slow
    /// consumer skips intermediate reducer emits, and the full set is never
    /// redelivered. `Some(FfiWindow::Expandable { initial, max })` is a
    /// bounded newest-first window: each frame carries the complete
    /// current row set + growth fact in `FfiFrame::window` (deltas stay
    /// empty on the wire) and grows only via
    /// [`NmpRowStream::request_rows`], never above `max`. Zero bounds and
    /// `initial > max` fail closed here with a typed [`FfiError`]; a
    /// windowed selection that already declares a NIP-01 `limit` fails with
    /// [`FfiError::WindowSelectionHasLimit`].
    pub fn observe(
        &self,
        query: FfiFilter,
        window: Option<FfiWindow>,
    ) -> Result<Arc<NmpRowStream>, FfiError> {
        let filter = filter_from_ffi(query)?;
        let window = window_from_ffi(window)?;
        let windowed = window.is_some();
        let subscription = self
            .engine
            .observe_async(nmp::LiveQuery::from_filter(filter), window)?;
        Ok(NmpRowStream::new(subscription, windowed))
    }

    /// Open a live subscription over an explicit [`FfiLiveQuery`] (#1108) --
    /// the constructor an app reaches for once [`Self::observe`]'s bare
    /// [`FfiFilter`] (which always takes `Demand::from_filter`'s static
    /// default, one branch) isn't enough: declaring `Pinned` wire authority,
    /// a non-default `AccessContext`, a non-`Agnostic` `CacheMode`, SEVERAL
    /// independent demand branches, or a bound on their merged row union.
    ///
    /// Branches are observed through this ONE subscription: rows are unioned
    /// by event id with provenance merged, each frame carries one evidence
    /// entry per canonical branch, and one cancellation withdraws every
    /// branch exactly once. Same pull-based/cancel/window shape as `observe`
    /// in every other respect (see that method's doc for the `window`
    /// policy); a window and an `aggregate_result_limit` are two owners of
    /// row membership and fail closed with
    /// [`FfiError::WindowAggregateResultLimit`].
    pub fn observe_query(
        &self,
        query: FfiLiveQuery,
        window: Option<FfiWindow>,
    ) -> Result<Arc<NmpRowStream>, FfiError> {
        let query = live_query_from_ffi(query)?;
        let window = window_from_ffi(window)?;
        let windowed = window.is_some();
        let subscription = self.engine.observe_async(query, window)?;
        Ok(NmpRowStream::new(subscription, windowed))
    }

    /// Enqueue a write (#680). The returned [`NmpReceiptStream`] exposes the
    /// stable receipt id ([`NmpReceiptStream::id`]) and streams every
    /// `WriteFact` this intent ever reaches (ledger #9 -- enqueue is not
    /// converged; the first value is never a terminal for a durable/
    /// at-most-once intent) via `async fn next()`. A caller-supplied `Signed`
    /// payload that fails verification is no longer a synchronous error here
    /// (that guarantee moved to `nmp-engine::core::EngineCore::on_publish`'s
    /// acceptance boundary, Unit A0/#56, so it holds for every entry point) --
    /// it refuses THIS CALL as `FfiError::PublishRefused`, taking nothing
    /// into custody, so no receipt, no stream and no queue entry exist for
    /// it. Exhaustion of the pre-acceptance correlation namespace is the
    /// same shape: a typed `FfiError` and no receipt id.
    pub fn publish(&self, intent: FfiWriteIntent) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let write_intent = write_intent_from_ffi(intent)?;
        #[cfg(feature = "nip65")]
        if matches!(write_intent.routing, nmp::WriteRouting::Auto)
            && self.automatic_routing == AutomaticRoutingAssembly::Unavailable
        {
            return Err(FfiError::AutomaticRoutingUnavailable);
        }
        let receipt = self.engine.publish(write_intent)?;
        Ok(NmpReceiptStream::new(self.engine.clone(), receipt))
    }

    /// Attach to a retained receipt without collapsing corrupt durable
    /// evidence into the same result as an unknown id (#680). The `Attached`
    /// variant carries an [`NmpReceiptStream`] that transparently traverses
    /// durable `WriteFact` facts in finite pages and streams onward,
    /// delivered pull-based via `async fn next()`.
    pub fn reattach_receipt(&self, receipt_id: u64) -> Result<FfiReceiptReattachment, FfiError> {
        let result = self.engine.reattach_receipt(nmp::ReceiptId(receipt_id))?;
        Ok(match result {
            ReceiptReattachment::Attached {
                id,
                statuses,
                next_cursor,
            } => FfiReceiptReattachment::Attached {
                stream: NmpReceiptStream::from_reattachment(
                    self.engine.clone(),
                    id,
                    statuses,
                    next_cursor,
                ),
            },
            ReceiptReattachment::NotFound => FfiReceiptReattachment::NotFound,
            ReceiptReattachment::RetainedButUnreadable => {
                FfiReceiptReattachment::RetainedButUnreadable
            }
        })
    }

    /// #591: recover a receipt after a crash that happened BEFORE the app
    /// could durably persist the receipt id `publish`
    /// returned -- looked up by the caller's own crash-safe correlation
    /// token instead. Otherwise identical to [`Self::reattach_receipt`],
    /// except the caller cannot already know the receipt id (that is
    /// exactly what a token recovers) -- `FfiCorrelationReattachment.
    /// receipt_id` carries it back, `Some` iff `outcome == Attached`.
    pub fn reattach_by_correlation(
        &self,
        correlation: String,
    ) -> Result<FfiCorrelationReattachment, FfiError> {
        let result = self.engine.reattach_by_correlation(correlation)?;
        let receipt_id = match &result {
            ReceiptReattachment::Attached { id, .. } => Some(id.0),
            ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => None,
        };
        let outcome = match result {
            ReceiptReattachment::Attached {
                id,
                statuses,
                next_cursor,
            } => FfiReceiptReattachment::Attached {
                stream: NmpReceiptStream::from_reattachment(
                    self.engine.clone(),
                    id,
                    statuses,
                    next_cursor,
                ),
            },
            ReceiptReattachment::NotFound => FfiReceiptReattachment::NotFound,
            ReceiptReattachment::RetainedButUnreadable => {
                FfiReceiptReattachment::RetainedButUnreadable
            }
        };
        Ok(FfiCorrelationReattachment {
            outcome,
            receipt_id,
        })
    }

    /// Read the app's own publish queue back (#1039).
    ///
    /// INSPECTION, never waiting: this returns what NMP knows right now and
    /// never blocks on settlement.
    pub fn publish_queue(
        &self,
        after_receipt_id: Option<u64>,
        limit: u8,
    ) -> Result<Vec<FfiPublishQueueEntry>, FfiPublishQueueError> {
        self.engine
            .publish_queue(after_receipt_id.map(nmp::ReceiptId), limit)
            .map(|entries| entries.iter().map(publish_queue_entry_to_ffi).collect())
            .map_err(publish_queue_error_to_ffi)
    }

    /// Read one bounded page of currently open obligations for the exact
    /// event id carried by a query row (#903).
    pub fn publish_queue_for_event(
        &self,
        event_id: String,
        after_receipt_id: Option<u64>,
        limit: u8,
    ) -> Result<Vec<FfiPublishQueueEntry>, FfiPublishQueueError> {
        let event_id =
            parse_event_id(&event_id).map_err(|error| FfiPublishQueueError::InvalidEventId {
                reason: error.to_string(),
            })?;
        self.engine
            .publish_queue_for_event(event_id, after_receipt_id.map(nmp::ReceiptId), limit)
            .map(|entries| entries.iter().map(publish_queue_entry_to_ffi).collect())
            .map_err(publish_queue_error_to_ffi)
    }

    /// Forget one queue entry (#1039). How a write parked forever on a
    /// missing signer, or a permanently-failed refused entry, ever ends —
    /// the parked one through `cancel_write` first, which ends the obligation
    /// and compensates the optimistic row, leaving the terminal receipt this
    /// door then forgets. An entry whose obligation is still open is refused.
    pub fn remove_publish_queue_entry(
        &self,
        receipt_id: u64,
    ) -> Result<(), FfiRemoveQueueEntryError> {
        self.engine
            .remove_publish_queue_entry(nmp::ReceiptId(receipt_id))
            .map_err(remove_queue_entry_error_to_ffi)
    }

    /// Explicitly cancel one accepted unsigned write. A successful outcome
    /// means the matching durable terminal fact was delivered to receipt
    /// observers.
    pub fn cancel(&self, receipt_id: u64) -> Result<FfiCancelWriteOutcome, FfiCancelWriteError> {
        self.engine
            .cancel(nmp::ReceiptId(receipt_id))
            .map(cancel_write_outcome_to_ffi)
            .map_err(cancel_write_error_to_ffi)
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

/// The app-facing pull-based handle to a live subscription (returned by
/// [`NmpEngine::observe`], #680/#762). Native SDKs synchronously call
/// [`Self::begin_next`] before awaiting [`NmpRowPull::receive`], then
/// synchronously commit or abort that ticket. The ticket is private transport
/// ownership inside the existing Swift `AsyncSequence` / Kotlin `Flow`; it is
/// not another app-facing observation noun.
///
/// For unbounded delta observations, one frame may be retained in the active
/// ticket until foreign completion acknowledges it. Reducer output produced
/// meanwhile still composes in the existing one-slot engine mailbox. The
/// maximum is therefore one claimed delta plus one composed successor, never
/// one item per cancellation. Windowed frames remain self-contained snapshots
/// and are not retained on abort.
#[derive(uniffi::Object)]
pub struct NmpRowStream {
    shared: Arc<RowStreamShared>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowDeliveryMode {
    Delta,
    Snapshot,
}

struct RowStreamShared {
    inner: nmp::AsyncSubscription,
    mode: RowDeliveryMode,
    lifecycle: Mutex<RowStreamLifecycle>,
}

struct RowStreamLifecycle {
    state: RowStreamState,
}

enum RowStreamState {
    Open(Box<RowStreamOpen>),
    Closed,
}

struct RowStreamOpen {
    active: Option<ActiveRowPull>,
    retained_delta: Option<nmp::Frame>,
}

struct PullIdentity;

struct ActiveRowPull {
    identity: Arc<PullIdentity>,
    phase: RowPullPhase,
}

enum RowPullPhase {
    Fresh,
    FreshDelta(nmp::Frame),
    Awaiting,
    AbortRequested,
    AwaitFinished,
    ReadyDelta(nmp::Frame),
    ReadySnapshot,
    Terminal,
}

enum ReceiveStart {
    Retained(nmp::Frame),
    Await,
}

impl NmpRowStream {
    fn new(inner: nmp::AsyncSubscription, windowed: bool) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(RowStreamShared {
                inner,
                mode: if windowed {
                    RowDeliveryMode::Snapshot
                } else {
                    RowDeliveryMode::Delta
                },
                lifecycle: Mutex::new(RowStreamLifecycle {
                    state: RowStreamState::Open(Box::new(RowStreamOpen {
                        active: None,
                        retained_delta: None,
                    })),
                }),
            }),
        })
    }
}

#[uniffi::export]
impl NmpRowStream {
    /// Claim the stream synchronously, before entering UniFFI's cancellable
    /// async READY/complete split. A second live ticket is refused; it never
    /// observes or replays the first ticket's retained delta.
    pub fn begin_next(&self) -> Result<Arc<NmpRowPull>, FfiRowPullError> {
        self.shared.begin_next()
    }

    /// Withdraw the subscription now, rather than waiting for `Drop` (a Swift
    /// `deinit` can be delayed by ARC in ways an app may want to preempt).
    /// Wakes any parked ticket `receive()` to `None`. Safe to call more than once, and
    /// safe to never call at all.
    pub fn cancel(&self) {
        self.shared.cancel();
    }

    /// Windowed observations only: monotonically raise the window's row
    /// target to at least `at_least`, clamped to the declared `max`.
    /// Idempotent and declarative -- calling with a value at or below the
    /// current target is a no-op; there is no continuation token to thread
    /// back and no generation to go stale (#485 replaced the opaque
    /// continuation entirely). Growth outcomes arrive as
    /// [`crate::types::FfiWindowLoad`] facts in delivered frames -- reaching
    /// the declared `max` is the `AtBound` FACT there, never an error here.
    /// Unbounded observations fail with
    /// [`FfiRequestRowsError::Unwindowed`].
    pub fn request_rows(&self, at_least: u64) -> Result<(), FfiRequestRowsError> {
        // Saturating u64→usize: `at_least` is a declarative lower bound the
        // engine clamps to the window's `max` anyway, so a value beyond the
        // platform's addressable row count is behaviorally identical to
        // usize::MAX (only reachable on sub-64-bit targets).
        let at_least = usize::try_from(at_least).unwrap_or(usize::MAX);
        self.shared
            .inner
            .request_rows(at_least)
            .map_err(FfiRequestRowsError::from)
    }
}

impl Drop for NmpRowStream {
    fn drop(&mut self) {
        self.shared.cancel();
    }
}

/// One private foreign-delivery claim for [`NmpRowStream`] (#762).
///
/// The native wrapper owns this object before it awaits [`Self::receive`].
/// After a non-cancelled return it synchronously calls [`Self::commit`];
/// every other path calls [`Self::abort`]. Dropping a ticket aborts it
/// idempotently.
#[derive(uniffi::Object)]
pub struct NmpRowPull {
    shared: Arc<RowStreamShared>,
    identity: Arc<PullIdentity>,
}

impl std::fmt::Debug for NmpRowPull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NmpRowPull").finish_non_exhaustive()
    }
}

#[uniffi::export]
impl NmpRowPull {
    /// Await this ticket's frame. A ticket is start-once: a second call is a
    /// typed refusal both while the first is pending and after it reached
    /// READY.
    pub async fn receive(&self) -> Result<Option<FfiFrame>, FfiRowPullError> {
        match self.shared.start_receive(&self.identity)? {
            ReceiveStart::Retained(frame) => Ok(Some(frame_to_ffi(frame))),
            ReceiveStart::Await => {
                let guard = ReceiveGuard::new(self.shared.clone(), self.identity.clone());
                let frame = self
                    .shared
                    .inner
                    .next()
                    .await
                    .map_err(|_| FfiRowPullError::ConcurrentNext)?;
                guard.finish(frame).map(|frame| frame.map(frame_to_ffi))
            }
        }
    }

    /// Acknowledge that foreign code obtained the result. For an unbounded
    /// frame this destructively releases the retained delta exactly once.
    pub fn commit(&self) -> Result<(), FfiRowPullError> {
        self.shared.commit(&self.identity)
    }

    /// Roll back a ticket that did not reach foreign completion. A retained
    /// unbounded delta becomes the next ticket's candidate; a windowed
    /// snapshot is self-contained and needs no replay.
    pub fn abort(&self) {
        self.shared.abort(&self.identity);
    }
}

impl Drop for NmpRowPull {
    fn drop(&mut self) {
        self.shared.abort(&self.identity);
    }
}

impl RowStreamShared {
    fn begin_next(self: &Arc<Self>) -> Result<Arc<NmpRowPull>, FfiRowPullError> {
        let identity = Arc::new(PullIdentity);
        let mut lifecycle = self.lifecycle.lock().unwrap();
        match &mut lifecycle.state {
            RowStreamState::Closed => Err(FfiRowPullError::Closed),
            RowStreamState::Open(open) => {
                if open.active.is_some() {
                    return Err(FfiRowPullError::ConcurrentNext);
                }
                let phase = open
                    .retained_delta
                    .take()
                    .map(RowPullPhase::FreshDelta)
                    .unwrap_or(RowPullPhase::Fresh);
                open.active = Some(ActiveRowPull {
                    identity: identity.clone(),
                    phase,
                });
                Ok(Arc::new(NmpRowPull {
                    shared: self.clone(),
                    identity,
                }))
            }
        }
    }

    fn start_receive(&self, identity: &Arc<PullIdentity>) -> Result<ReceiveStart, FfiRowPullError> {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return Err(FfiRowPullError::Closed);
        };
        let Some(pull) = open.active.as_mut() else {
            return Err(FfiRowPullError::Finished);
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            return Err(FfiRowPullError::Finished);
        }
        match std::mem::replace(&mut pull.phase, RowPullPhase::AwaitFinished) {
            RowPullPhase::Fresh => {
                pull.phase = RowPullPhase::Awaiting;
                Ok(ReceiveStart::Await)
            }
            RowPullPhase::FreshDelta(frame) => {
                let returned = frame.clone();
                pull.phase = RowPullPhase::ReadyDelta(frame);
                Ok(ReceiveStart::Retained(returned))
            }
            phase => {
                pull.phase = phase;
                Err(FfiRowPullError::ReceiveAlreadyStarted)
            }
        }
    }

    fn finish_receive(
        &self,
        identity: &Arc<PullIdentity>,
        frame: Option<nmp::Frame>,
    ) -> Result<Option<nmp::Frame>, FfiRowPullError> {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return if frame.is_none() {
                Ok(None)
            } else {
                Err(FfiRowPullError::Closed)
            };
        };
        let Some(mut pull) = open.active.take() else {
            return Err(FfiRowPullError::Finished);
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            open.active = Some(pull);
            return Err(FfiRowPullError::Finished);
        }
        match &pull.phase {
            RowPullPhase::Awaiting => {}
            RowPullPhase::AbortRequested => {
                if self.mode == RowDeliveryMode::Delta {
                    open.retained_delta = frame;
                }
                return Err(FfiRowPullError::Aborted);
            }
            _ => {
                open.active = Some(pull);
                return Err(FfiRowPullError::Finished);
            }
        }

        match frame {
            Some(frame) if self.mode == RowDeliveryMode::Delta => {
                let returned = frame.clone();
                pull.phase = RowPullPhase::ReadyDelta(frame);
                open.active = Some(pull);
                Ok(Some(returned))
            }
            Some(frame) => {
                pull.phase = RowPullPhase::ReadySnapshot;
                open.active = Some(pull);
                Ok(Some(frame))
            }
            None => {
                pull.phase = RowPullPhase::Terminal;
                open.active = Some(pull);
                Ok(None)
            }
        }
    }

    fn commit(&self, identity: &Arc<PullIdentity>) -> Result<(), FfiRowPullError> {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return Err(FfiRowPullError::Closed);
        };
        let Some(pull) = open.active.as_ref() else {
            return Err(FfiRowPullError::Finished);
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            return Err(FfiRowPullError::Finished);
        }
        match pull.phase {
            RowPullPhase::ReadyDelta(_) | RowPullPhase::ReadySnapshot | RowPullPhase::Terminal => {
                open.active = None;
                Ok(())
            }
            RowPullPhase::Fresh
            | RowPullPhase::FreshDelta(_)
            | RowPullPhase::Awaiting
            | RowPullPhase::AbortRequested
            | RowPullPhase::AwaitFinished => Err(FfiRowPullError::NotReady),
        }
    }

    fn abort(&self, identity: &Arc<PullIdentity>) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return;
        };
        let Some(mut pull) = open.active.take() else {
            return;
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            open.active = Some(pull);
            return;
        }
        match pull.phase {
            RowPullPhase::FreshDelta(frame) | RowPullPhase::ReadyDelta(frame) => {
                open.retained_delta = Some(frame);
            }
            RowPullPhase::Awaiting => {
                pull.phase = RowPullPhase::AbortRequested;
                open.active = Some(pull);
            }
            RowPullPhase::Fresh
            | RowPullPhase::AbortRequested
            | RowPullPhase::AwaitFinished
            | RowPullPhase::ReadySnapshot
            | RowPullPhase::Terminal => {}
        }
    }

    fn receive_dropped(&self, identity: &Arc<PullIdentity>) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return;
        };
        let Some(mut pull) = open.active.take() else {
            return;
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            open.active = Some(pull);
            return;
        }
        match pull.phase {
            RowPullPhase::AbortRequested => {}
            RowPullPhase::Awaiting => {
                pull.phase = RowPullPhase::AwaitFinished;
                open.active = Some(pull);
            }
            _ => {
                open.active = Some(pull);
            }
        }
    }

    fn cancel(&self) {
        let should_cancel = {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            if matches!(lifecycle.state, RowStreamState::Closed) {
                false
            } else {
                lifecycle.state = RowStreamState::Closed;
                true
            }
        };
        if should_cancel {
            self.inner.cancel();
        }
    }
}

struct ReceiveGuard {
    pending: Option<(Arc<RowStreamShared>, Arc<PullIdentity>)>,
}

impl ReceiveGuard {
    fn new(shared: Arc<RowStreamShared>, identity: Arc<PullIdentity>) -> Self {
        Self {
            pending: Some((shared, identity)),
        }
    }

    fn finish(mut self, frame: Option<nmp::Frame>) -> Result<Option<nmp::Frame>, FfiRowPullError> {
        let (shared, identity) = self.pending.take().expect("receive guard is armed");
        shared.finish_receive(&identity, frame)
    }
}

impl Drop for ReceiveGuard {
    fn drop(&mut self) {
        if let Some((shared, identity)) = self.pending.take() {
            shared.receive_dropped(&identity);
        }
    }
}

/// The app-facing pull-based handle to a live diagnostics stream (returned by
/// [`NmpEngine::observe_diagnostics`], #680). Same discipline as
/// [`NmpRowStream`] — await [`Self::next`], `Drop`/[`Self::cancel`] withdraw.
#[derive(uniffi::Object)]
pub struct NmpDiagnosticsStream {
    inner: nmp::AsyncDiagnosticsSubscription,
}

#[uniffi::export]
impl NmpDiagnosticsStream {
    /// Await the next [`FfiDiagnosticsSnapshot`] — the current snapshot on the
    /// first call, a fresh one on every coverage change afterward, or `None`
    /// once the stream is withdrawn. [`FfiError::ConcurrentNext`] on an
    /// overlapping call.
    pub async fn next(&self) -> Result<Option<FfiDiagnosticsSnapshot>, FfiError> {
        match self.inner.next().await {
            Ok(Some(snapshot)) => Ok(Some(diagnostics_snapshot_to_ffi(snapshot))),
            Ok(None) => Ok(None),
            Err(_) => Err(FfiError::ConcurrentNext),
        }
    }

    /// Withdraw this diagnostics observer now, rather than waiting for `Drop`.
    /// Safe to call more than once; safe to never call at all.
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

impl Drop for NmpDiagnosticsStream {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}

/// The app-facing pull-based receipt stream (returned by [`NmpEngine::publish`]
/// and the `Attached` reattachment, #680). It
/// exposes the stable store-issued receipt id via [`Self::id`] and delivers
/// ordered `WriteFact` facts via `async fn next()`. Live delivery is a finite
/// FIFO that reports typed lag. Receipt facts are durable: the persisted
/// publish-queue Redb store is the source of truth, so a dropped or lagged stream can
/// be reattached and traverse retained facts through finite pages.
#[derive(uniffi::Object)]
pub struct NmpReceiptStream {
    id: nmp::ReceiptId,
    engine: Option<Arc<nmp::Engine>>,
    delivery: Mutex<ReceiptDelivery>,
    // Concurrency guard only, never lifecycle/ownership state: cancellation
    // lives in `ReceiptDelivery`, and this flag is released by the RAII
    // `ReceiptReadingGuard` on success, error, or future drop (gate 3).
    reading: std::sync::atomic::AtomicBool,
}

enum ReceiptDelivery {
    Active {
        receiver: Arc<nmp::AsyncFifoReceiver<nmp::WriteFact>>,
        next_cursor: Option<nmp::ReceiptReplayCursor>,
    },
    Cancelled,
}

impl NmpReceiptStream {
    pub(crate) fn new(engine: Arc<nmp::Engine>, receipt: nmp::ReceiptStream) -> Arc<Self> {
        Arc::new(Self {
            id: receipt.id,
            engine: Some(engine),
            delivery: Mutex::new(ReceiptDelivery::Active {
                receiver: Arc::new(receipt.statuses.into_async()),
                next_cursor: None,
            }),
            reading: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn from_reattachment(
        engine: Arc<nmp::Engine>,
        id: nmp::ReceiptId,
        statuses: nmp::FifoReceiver<nmp::WriteFact>,
        next_cursor: Option<nmp::ReceiptReplayCursor>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            engine: Some(engine),
            delivery: Mutex::new(ReceiptDelivery::Active {
                receiver: Arc::new(statuses.into_async()),
                next_cursor,
            }),
            reading: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn current_receiver(
        &self,
    ) -> Option<(
        Arc<nmp::AsyncFifoReceiver<nmp::WriteFact>>,
        Option<nmp::ReceiptReplayCursor>,
    )> {
        let delivery = self.delivery.lock().unwrap();
        match &*delivery {
            ReceiptDelivery::Active {
                receiver,
                next_cursor,
            } => Some((receiver.clone(), next_cursor.clone())),
            ReceiptDelivery::Cancelled => None,
        }
    }

    fn install_page(
        &self,
        prior: &Arc<nmp::AsyncFifoReceiver<nmp::WriteFact>>,
        statuses: nmp::FifoReceiver<nmp::WriteFact>,
        next_cursor: Option<nmp::ReceiptReplayCursor>,
    ) -> bool {
        let replacement = Arc::new(statuses.into_async());
        let mut delivery = self.delivery.lock().unwrap();
        match &mut *delivery {
            ReceiptDelivery::Active {
                receiver,
                next_cursor: cursor,
            } if Arc::ptr_eq(receiver, prior) => {
                *receiver = replacement;
                *cursor = next_cursor;
                true
            }
            ReceiptDelivery::Active { .. } | ReceiptDelivery::Cancelled => {
                replacement.close();
                false
            }
        }
    }

    fn replace_page(
        &self,
        statuses: nmp::FifoReceiver<nmp::WriteFact>,
        next_cursor: Option<nmp::ReceiptReplayCursor>,
    ) -> bool {
        let replacement = Arc::new(statuses.into_async());
        let mut delivery = self.delivery.lock().unwrap();
        match &*delivery {
            ReceiptDelivery::Active { .. } => {
                *delivery = ReceiptDelivery::Active {
                    receiver: replacement,
                    next_cursor,
                };
                true
            }
            ReceiptDelivery::Cancelled => {
                replacement.close();
                false
            }
        }
    }

    async fn next_fact(&self) -> Result<Option<nmp::WriteFact>, FfiError> {
        loop {
            let Some((receiver, next_cursor)) = self.current_receiver() else {
                return Ok(None);
            };
            match receiver.next().await {
                Ok(Some(status)) => return Ok(Some(status)),
                Err(nmp::FifoNextError::ConcurrentNext) => return Err(FfiError::ConcurrentNext),
                Err(nmp::FifoNextError::Lagged) => {
                    return Err(FfiError::FactStreamLagged {
                        receipt_id: Some(self.id.0),
                    });
                }
                Ok(None) => {}
            }

            let Some(cursor) = next_cursor else {
                return Ok(None);
            };
            let Some(engine) = &self.engine else {
                return Err(FfiError::FactStreamLagged {
                    receipt_id: Some(self.id.0),
                });
            };
            match engine.reattach_receipt_from(self.id, cursor)? {
                ReceiptReattachment::Attached {
                    id,
                    statuses,
                    next_cursor,
                } if id == self.id => {
                    if !self.install_page(&receiver, statuses, next_cursor) {
                        return Ok(None);
                    }
                }
                ReceiptReattachment::Attached { .. }
                | ReceiptReattachment::NotFound
                | ReceiptReattachment::RetainedButUnreadable => {
                    return Err(FfiError::ReceiptReplayUnavailable {
                        receipt_id: self.id.0,
                    });
                }
            }
        }
    }

    fn restart_replay(&self) -> Result<(), FfiError> {
        let Some(engine) = &self.engine else {
            return Err(FfiError::ReceiptReplayUnavailable {
                receipt_id: self.id.0,
            });
        };
        match engine.reattach_receipt(self.id)? {
            ReceiptReattachment::Attached {
                id,
                statuses,
                next_cursor,
            } if id == self.id => {
                if self.replace_page(statuses, next_cursor) {
                    Ok(())
                } else {
                    Err(FfiError::ReceiptReplayUnavailable {
                        receipt_id: self.id.0,
                    })
                }
            }
            ReceiptReattachment::Attached { .. }
            | ReceiptReattachment::NotFound
            | ReceiptReattachment::RetainedButUnreadable => {
                Err(FfiError::ReceiptReplayUnavailable {
                    receipt_id: self.id.0,
                })
            }
        }
    }
}

#[uniffi::export]
impl NmpReceiptStream {
    /// The stable store-issued receipt id, needed for process-later
    /// reattachment ([`NmpEngine::reattach_receipt`]) and explicit cancellation
    /// ([`NmpEngine::cancel`]).
    pub fn id(&self) -> u64 {
        self.id.0
    }

    /// Await the next `WriteFact`, or `None` once the intent has fully
    /// resolved or the engine has shut down. [`FfiError::ConcurrentNext`] on an
    /// overlapping call.
    pub async fn next(&self) -> Result<Option<FfiWriteFact>, FfiError> {
        use std::sync::atomic::Ordering;

        if self.reading.swap(true, Ordering::AcqRel) {
            return Err(FfiError::ConcurrentNext);
        }
        let _reading = ReceiptReadingGuard(&self.reading);
        Ok(self
            .next_fact()
            .await?
            .map(|status| write_status_to_ffi(WriteStatusRef(&status))))
    }

    /// Await the one terminal publication answer. NMP owns fact reduction and
    /// automatically restarts from durable replay if live delivery lags.
    pub async fn result(&self) -> Result<FfiReceiptResult, FfiError> {
        use std::sync::atomic::Ordering;

        if self.reading.swap(true, Ordering::AcqRel) {
            return Err(FfiError::ConcurrentNext);
        }
        let _reading = ReceiptReadingGuard(&self.reading);
        self.restart_replay()?;
        let mut facts = Vec::new();
        loop {
            match self.next_fact().await {
                Ok(Some(fact)) => {
                    let terminal = matches!(fact, nmp::WriteFact::Outcome(_));
                    facts.push(fact);
                    if terminal {
                        let result = nmp::ReceiptResult::from_facts(facts).map_err(|_| {
                            FfiError::ReceiptClosedWithoutOutcome {
                                receipt_id: self.id.0,
                            }
                        })?;
                        return Ok(receipt_result_to_ffi(result));
                    }
                }
                Ok(None) => {
                    return Err(FfiError::ReceiptClosedWithoutOutcome {
                        receipt_id: self.id.0,
                    });
                }
                Err(FfiError::FactStreamLagged { .. }) => {
                    facts.clear();
                    self.restart_replay()?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Stop delivering live status frames to this stream. The durable receipt
    /// itself is untouched (the write is not cancelled — use
    /// [`NmpEngine::cancel`] for that); a later [`NmpEngine::reattach_receipt`]
    /// traverses the durable history. Safe to call more than once.
    pub fn cancel(&self) {
        let prior = {
            let mut delivery = self.delivery.lock().unwrap();
            match std::mem::replace(&mut *delivery, ReceiptDelivery::Cancelled) {
                ReceiptDelivery::Active { receiver, .. } => Some(receiver),
                ReceiptDelivery::Cancelled => None,
            }
        };
        if let Some(receiver) = prior {
            receiver.close();
        }
    }
}

impl Drop for NmpReceiptStream {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ReceiptReadingGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for ReceiptReadingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Scoped one-shot sign-only handle (#680). It owns no signer registration and
/// cannot affect accepted durable writes. Await [`Self::signed`] once for the
/// verified event (or a typed failure); [`Self::cancel`] cancels only this
/// signer operation.
#[derive(uniffi::Object)]
pub struct NmpSignEventHandle {
    cancel: nmp::SignEventCancel,
    result: nmp::AsyncFifoReceiver<Result<nmp::Event, nmp::SignEventError>>,
}

#[uniffi::export]
impl NmpSignEventHandle {
    /// Await the one-shot outcome: the fully-verified signed event, or a typed
    /// [`FfiSignEventFailure`]. This is one-shot — a second await (sequential or
    /// concurrent) returns [`FfiSignEventFailure::AlreadyConsumed`], because the
    /// single result was already delivered to the first await.
    pub async fn signed(&self) -> Result<FfiSignedEvent, FfiSignEventFailure> {
        match self.result.next().await {
            Ok(Some(Ok(event))) => Ok(signed_event_to_ffi(event)),
            Ok(Some(Err(error))) => Err(sign_event_failure(error)),
            Ok(None) | Err(_) => Err(FfiSignEventFailure::AlreadyConsumed),
        }
    }

    /// Cancel this sign-only operation. Idempotent; safe after completion.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for NmpSignEventHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests;
