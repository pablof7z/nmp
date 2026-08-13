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
        Ok(FfiRelayInformation {
            relay: value.relay.to_string(),
            document: FfiRelayInformationDocument {
                name: value.document.name,
                description: value.document.description,
                banner: value.document.banner,
                icon: value.document.icon,
                pubkey: value.document.pubkey,
                self_pubkey: value.document.self_pubkey,
                contact: value.document.contact,
                supported_nips: value.document.supported_nips,
                software: value.document.software,
                version: value.document.version,
                terms_of_service: value.document.terms_of_service,
                limitation: FfiRelayInformationLimitations {
                    max_message_length: value.document.limitation.max_message_length,
                    max_subscriptions: value.document.limitation.max_subscriptions,
                    max_filters: value.document.limitation.max_filters,
                    max_limit: value.document.limitation.max_limit,
                    max_subid_length: value.document.limitation.max_subid_length,
                    max_event_tags: value.document.limitation.max_event_tags,
                    max_content_length: value.document.limitation.max_content_length,
                    min_pow_difficulty: value.document.limitation.min_pow_difficulty,
                    auth_required: value.document.limitation.auth_required,
                    payment_required: value.document.limitation.payment_required,
                    created_at_lower_limit: value.document.limitation.created_at_lower_limit,
                    created_at_upper_limit: value.document.limitation.created_at_upper_limit,
                },
                structured: value.document.structured.into_iter().collect(),
            },
            raw_json: value.raw_json,
            document_revision: value.document_revision,
            fetched_at: value.fetched_at,
            fresh_until: value.fresh_until,
            freshness: match value.freshness {
                nmp::RelayInformationFreshness::Fresh => FfiRelayInformationFreshness::Fresh,
                nmp::RelayInformationFreshness::Stale => FfiRelayInformationFreshness::Stale,
            },
            etag: value.etag,
            last_modified: value.last_modified,
            cache_control: value.cache_control,
            expires: value.expires,
            last_error: value.last_error.map(relay_information_error_kind),
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
mod tests {
    use super::*;
    use crate::types::{
        FfiAccessContext, FfiBinding, FfiCacheMode, FfiDemand, FfiFilter, FfiFrame, FfiIdentity,
        FfiLiveQuery, FfiNotSentReason, FfiRowDelta, FfiSignEventRequest, FfiSigningState,
        FfiSourceAuthority, FfiWindow, FfiWindowLoad, FfiWriteFact, FfiWriteOutcome,
        FfiWritePayload, FfiWriteRouting,
    };
    use redb::ReadableTable;
    use std::collections::BTreeSet;
    use std::time::Duration;

    #[cfg(any(feature = "nip02", feature = "nip65"))]
    fn ffi_private_key(keys: &nostr::Keys) -> Arc<FfiPrivateKey> {
        FfiPrivateKey::from_bytes(keys.secret_key().to_secret_bytes().to_vec()).unwrap()
    }

    fn ffi_private_key_byte(byte: u8) -> Arc<FfiPrivateKey> {
        let mut bytes = vec![0; 32];
        bytes[31] = byte;
        FfiPrivateKey::from_bytes(bytes).unwrap()
    }

    fn ffi_public_key(public_key: nostr::PublicKey) -> Arc<FfiPublicKey> {
        FfiPublicKey::from_bytes(public_key.to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn receipt_result_recovers_from_live_fifo_lag_without_exposing_replay() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let keys = nostr::Keys::generate();
        let receipt = engine
            .publish(nmp::WriteIntent {
                payload: nmp::WritePayload::Event(
                    nmp::EventBuilder::new(nostr::Kind::TextNote).content("lagged result"),
                ),
                routing: nmp::WriteRouting::Explicit(vec![nostr::RelayUrl::parse(
                    "wss://lagged-result.invalid",
                )
                .unwrap()]),
                identity: nmp::Identity::Explicit(keys.public_key()),
                correlation: None,
            })
            .unwrap();
        let receipt_id = receipt.id;
        engine.cancel(receipt_id).unwrap();
        let stream = NmpReceiptStream::new(engine.clone(), receipt);

        let (sender, lagged) = nmp::fifo_channel();
        for _ in 0..=nmp::FACT_CHANNEL_CAPACITY {
            let _ = sender.send(nmp::WriteFact::Signing(nmp::SigningState::AwaitingSigner {
                pubkey: keys.public_key(),
            }));
        }
        assert!(stream.replace_page(lagged, None));

        let result = stream.result().await.unwrap();
        assert_eq!(
            result.outcome,
            FfiWriteOutcome::NotSent {
                reason: FfiNotSentReason::Cancelled
            }
        );
        assert!(result.relays.is_empty());
        engine.shutdown();
    }

    // #680 replaced push/callback observers with pull-based async stream handles:
    // `observe`/`observe_demand`/`observe_diagnostics`/`publish`/`sign_event` take
    // no observer argument and return `Arc<Nmp*Stream>`/`Arc<NmpSignEventHandle>`
    // whose `async fn next()`/`signed()` drive delivery. `None` from `next()`
    // replaces `on_closed`. The `RowObserver`/`DiagnosticsObserver`/
    // `ReceiptObserver`/`SignEventObserver`/`FollowObserver` traits are deleted,
    // as is the native-task capacity/census vocabulary. Tests below drive the async
    // handles on a real Tokio executor (`#[tokio::test]`, dev-only).

    struct AllowPolicyCallback;

    impl FfiAuthPolicyCallback for AllowPolicyCallback {
        fn evaluate(
            &self,
            _request: crate::auth::FfiAuthPolicyRequest,
            completion: Arc<crate::auth::FfiAuthPolicyCompletion>,
        ) {
            completion
                .resolve(crate::auth::FfiAuthPolicyOutcome::Allow)
                .unwrap();
        }

        fn on_cancelled(&self, _request: crate::auth::FfiAuthPolicyRequest) {}
    }

    /// Await the next row frame within the lifecycle bound. `None` is the
    /// terminal signal (cancel / shutdown / producer drop).
    async fn next_frame(stream: &NmpRowStream) -> Option<FfiFrame> {
        let pull = match stream.begin_next() {
            Ok(pull) => pull,
            Err(FfiRowPullError::Closed) => return None,
            Err(error) => panic!("row stream is available: {error}"),
        };
        let frame = tokio::time::timeout(Duration::from_secs(5), pull.receive())
            .await
            .expect("a frame must arrive within the lifecycle bound")
            .expect("row pull lifecycle is valid");
        pull.commit()
            .expect("foreign completion commits the row pull");
        frame
    }

    /// Await the next receipt status within the lifecycle bound. `None` is the
    /// terminal signal (the intent fully resolved / engine shutdown).
    async fn next_status(stream: &NmpReceiptStream) -> Option<FfiWriteFact> {
        tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("a status must arrive within the lifecycle bound")
            .expect("receipt next() is not a concurrent-misuse")
    }

    #[test]
    fn ffi_config_manual_default_keeps_auth_capacity_finite() {
        let config = NmpEngineConfig::default();
        assert_eq!(config.max_auth_capabilities, 64);
        assert_eq!(nmp::EngineConfig::from(config).max_auth_capabilities, 64);
    }

    #[test]
    fn core_native_config_cannot_assemble_an_author_route_provider() {
        let config = NmpEngineConfig {
            app_relays: vec!["wss://app.example".to_string()],
            fallback_relays: vec!["wss://fallback.example".to_string()],
            ..NmpEngineConfig::default()
        };
        let projected = nmp::EngineConfig::from(config);

        assert!(
            projected.indexer_relays.is_empty(),
            "core native has no discovery-source setting; optional providers own their sources"
        );
        assert_eq!(projected.app_relays, ["wss://app.example"]);
        assert_eq!(projected.fallback_relays, ["wss://fallback.example"]);
    }

    #[test]
    #[cfg(feature = "nip65")]
    fn selected_outbox_routing_refuses_an_empty_runtime_indexer_set() {
        let result = NmpEngine::new(
            NmpEngineConfig {
                outbox_routing: Some(FfiOutboxRoutingConfig {
                    indexers: Vec::new(),
                }),
                ..NmpEngineConfig::default()
            },
            None,
        );
        assert!(matches!(result, Err(FfiError::OutboxRoutingIndexersEmpty)));
    }

    #[test]
    #[cfg(feature = "nip65")]
    fn selected_outbox_routing_projects_only_the_app_owned_indexers() {
        let config = NmpEngineConfig {
            outbox_routing: Some(FfiOutboxRoutingConfig {
                indexers: vec!["wss://indexer.example".to_string()],
            }),
            ..NmpEngineConfig::default()
        };
        let projected = nmp::EngineConfig::from(config.clone());
        assert_eq!(projected.indexer_relays, ["wss://indexer.example"]);

        let engine =
            NmpEngine::new(config, None).expect("a nonempty app-owned indexer set is valid");
        engine.shutdown();
    }

    #[test]
    #[cfg(feature = "nip65")]
    fn providerless_auto_refuses_before_acceptance_and_leaves_no_residue() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None)
            .expect("an explicit-routing-only engine is valid");
        let result = engine.publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: crate::types::FfiEventBuilder {
                    kind: 1,
                    tags: Vec::new(),
                    content: "must not park".to_string(),
                    created_at: Some(10),
                },
            },
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Active,
            correlation: None,
        });
        assert!(matches!(result, Err(FfiError::AutomaticRoutingUnavailable)));
        assert!(engine.publish_queue(None, u8::MAX).unwrap().is_empty());
        engine.shutdown();
    }

    #[test]
    #[cfg(feature = "nip02")]
    fn providerless_follow_refuses_before_the_action_starts_or_leaves_residue() {
        let author = nostr::Keys::generate();
        let engine = NmpEngine::new(NmpEngineConfig::default(), None)
            .expect("an explicit-routing-only engine is valid");
        let _account = engine
            .add_private_key_account(ffi_private_key(&author), true)
            .expect("the native account registers");
        let result = engine.follow(nostr::Keys::generate().public_key().to_hex());
        assert!(matches!(result, Err(FfiError::AutomaticRoutingUnavailable)));
        assert!(engine.publish_queue(None, u8::MAX).unwrap().is_empty());
        engine.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[cfg(feature = "nip65")]
    async fn selected_outbox_routing_discovers_and_publishes_to_the_cold_outbox() {
        use nmp_test_support::relays::{RelayConfig, ScriptedRelay};

        let author = nostr::Keys::generate();
        let indexer = ScriptedRelay::start(&RelayConfig::default()).await;
        let outbox = ScriptedRelay::start(&RelayConfig::default()).await;
        indexer
            .seed_relay_list(&author, &[outbox.url.to_string()], &[], 1_700_000_000)
            .await;

        let engine = NmpEngine::new(
            NmpEngineConfig {
                outbox_routing: Some(FfiOutboxRoutingConfig {
                    indexers: vec![indexer.url.to_string()],
                }),
                ..NmpEngineConfig::default()
            },
            None,
        )
        .expect("the selected native NIP-65 assembly constructs");
        let _account = engine
            .add_private_key_account(ffi_private_key(&author), true)
            .expect("the native account registers");

        let receipt = engine
            .publish(FfiWriteIntent {
                payload: FfiWritePayload::Event {
                    builder: crate::types::FfiEventBuilder {
                        kind: 1,
                        tags: Vec::new(),
                        content: "cold native outbox".to_string(),
                        created_at: Some(1_700_000_001),
                    },
                },
                routing: FfiWriteRouting::Auto,
                identity: FfiIdentity::Active,
                correlation: None,
            })
            .expect("the automatic native write is accepted");

        let published_relay = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match receipt
                    .next()
                    .await
                    .expect("the receipt stream remains valid")
                {
                    Some(FfiWriteFact::Relay {
                        relay,
                        state: crate::types::FfiRelayState::Published,
                    }) => break relay,
                    Some(_) => {}
                    None => panic!("the receipt ended before a relay published"),
                }
            }
        })
        .await
        .expect("cold NIP-65 discovery and delivery complete within the bound");

        assert_eq!(published_relay, outbox.url.to_string());
        assert_eq!(outbox.admitted_events().len(), 1);
        assert!(indexer.admitted_events().is_empty());

        engine.shutdown();
        outbox.shutdown();
        indexer.shutdown();
    }

    #[test]
    fn ffi_account_identity_is_its_public_key() {
        let engine = NmpEngine::new(
            NmpEngineConfig {
                max_auth_capabilities: 1,
                ..NmpEngineConfig::default()
            },
            None,
        )
        .unwrap();
        let first = engine
            .add_private_key_account(ffi_private_key_byte(41), false)
            .unwrap();
        let replacement = engine
            .add_private_key_account(ffi_private_key_byte(41), false)
            .unwrap();

        assert_eq!(first.public_key().bytes(), replacement.public_key().bytes());
        assert!(engine.remove_account(Arc::clone(&first)).unwrap());
        assert!(!engine.remove_account(Arc::clone(&replacement)).unwrap());
        assert!(!engine.remove_account(replacement).unwrap());
    }

    #[test]
    fn ffi_auth_policy_registration_is_explicit_repeatable_and_stale_safe() {
        let engine = NmpEngine::new(
            NmpEngineConfig {
                max_auth_capabilities: 1,
                ..NmpEngineConfig::default()
            },
            None,
        )
        .unwrap();
        let public_key = nostr::Keys::generate().public_key().to_hex();
        let first = engine
            .add_auth_policy(public_key.clone(), Box::new(AllowPolicyCallback))
            .unwrap();
        let replacement = engine
            .add_auth_policy(public_key.clone(), Box::new(AllowPolicyCallback))
            .unwrap();

        assert_eq!(first.expected_public_key(), public_key);
        assert!(!engine.remove_auth_policy(Arc::clone(&first)).unwrap());
        assert!(engine.remove_auth_policy(Arc::clone(&replacement)).unwrap());
        assert!(!engine.remove_auth_policy(replacement).unwrap());
    }

    #[test]
    fn ffi_zero_auth_capacity_returns_typed_registry_refusal() {
        let engine = NmpEngine::new(
            NmpEngineConfig {
                max_auth_capabilities: 0,
                ..NmpEngineConfig::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            engine
                .add_private_key_account(ffi_private_key_byte(42), false)
                .unwrap_err(),
            FfiError::AuthCapabilityRegistryFull { limit: 0 }
        );
    }

    fn ffi_windowed_query(author: String) -> FfiLiveQuery {
        FfiLiveQuery {
            branches: vec![FfiDemand {
                selection: FfiFilter {
                    kinds: Some(vec![7_778]),
                    authors: Some(FfiBinding::Literal {
                        values: vec![author],
                    }),
                    ..FfiFilter::default()
                },
                source: FfiSourceAuthority::AuthorOutboxes,
                access: FfiAccessContext::Public,
                cache: FfiCacheMode::Agnostic,
                freshness: crate::types::FfiFreshness::Live,
            }],
            aggregate_result_limit: None,
        }
    }

    /// Drive `next()` until a windowed frame with the wanted load fact arrives.
    async fn recv_window_load(
        stream: &NmpRowStream,
        wanted: impl Fn(FfiWindowLoad) -> bool,
    ) -> FfiFrame {
        loop {
            let frame = next_frame(stream)
                .await
                .expect("windowed stream must not end before the wanted frame");
            assert!(
                frame.deltas.is_empty(),
                "windowed frames must never ship wire deltas alongside the snapshot"
            );
            let load = frame
                .window
                .as_ref()
                .expect("windowed observation frames must carry window contents")
                .load;
            if wanted(load) {
                return frame;
            }
        }
    }

    /// #485's FFI drain proof, ported to the pull-based handle: bounded delivery
    /// over tie-second rows, explicit declarative growth, AtBound as a delivered
    /// FACT (never a thrown error), and the windowed/unbounded split on the same
    /// handle type.
    #[tokio::test]
    async fn ffi_windowed_observe_delivers_snapshot_frames_grows_and_reports_at_bound() {
        use nmp_store::EventStore;

        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("ffi-window.redb");
        let keys = nostr::Keys::generate();
        let relay = nostr::RelayUrl::parse("wss://ffi-window.example").unwrap();
        {
            let mut store = nmp_store::RedbStore::open(&path).unwrap();
            for index in 0..3 {
                let event = nostr::UnsignedEvent::new(
                    keys.public_key(),
                    nostr::Timestamp::from(100),
                    nostr::Kind::Custom(7_778),
                    Vec::new(),
                    format!("ffi-window-{index}"),
                )
                .sign_with_keys(&keys)
                .unwrap();
                store
                    .insert(
                        event,
                        nmp_store::RelayObserved::new(relay.clone(), nostr::Timestamp::from(200)),
                    )
                    .unwrap();
            }
        }

        let engine = NmpEngine::new(
            NmpEngineConfig {
                store_path: Some(path.to_string_lossy().into_owned()),
                ..NmpEngineConfig::default()
            },
            None,
        )
        .unwrap();
        let handle = engine
            .observe_query(
                ffi_windowed_query(keys.public_key().to_hex()),
                Some(FfiWindow::Expandable { initial: 1, max: 2 }),
            )
            .unwrap();

        let first = recv_window_load(&handle, |load| load == FfiWindowLoad::Idle).await;
        assert_eq!(first.window.unwrap().rows.len(), 1);

        // Declarative growth: no token to thread back, just a row target.
        handle.request_rows(2).unwrap();
        let second =
            recv_window_load(&handle, |load| load == FfiWindowLoad::Returned { added: 1 }).await;
        assert_eq!(second.window.unwrap().rows.len(), 2);

        // Raising the target past `max` clamps and is NEVER an error --
        // being at the bound arrives as the AtBound FACT in a frame.
        handle.request_rows(5).unwrap();
        let bounded =
            recv_window_load(&handle, |load| load == FfiWindowLoad::AtBound { max: 2 }).await;
        assert_eq!(bounded.window.unwrap().rows.len(), 2);

        // An UNBOUNDED handle on the same engine has no window to grow --
        // the same verb fails closed, typed.
        let unbounded = engine
            .observe(
                FfiFilter {
                    kinds: Some(vec![7_778]),
                    ..FfiFilter::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(
            unbounded.request_rows(10).unwrap_err(),
            FfiRequestRowsError::Unwindowed
        );

        drop(handle);
        drop(unbounded);
        engine.shutdown();
    }

    /// Window validation fails closed at the conversion/facade seam, typed,
    /// BEFORE any observation is opened.
    #[test]
    fn ffi_window_validation_is_typed() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");

        let zero = engine
            .observe(
                FfiFilter::default(),
                Some(FfiWindow::Expandable { initial: 0, max: 4 }),
            )
            .map(|_| ())
            .expect_err("a zero window bound must fail closed");
        assert_eq!(zero, FfiError::WindowZeroRows);

        let inverted = engine
            .observe(
                FfiFilter::default(),
                Some(FfiWindow::Expandable { initial: 5, max: 2 }),
            )
            .map(|_| ())
            .expect_err("an inverted window must fail closed");
        assert_eq!(
            inverted,
            FfiError::WindowInitialExceedsMax { initial: 5, max: 2 }
        );

        let limited = engine
            .observe(
                FfiFilter {
                    limit: Some(1),
                    ..FfiFilter::default()
                },
                Some(FfiWindow::Expandable { initial: 1, max: 4 }),
            )
            .map(|_| ())
            .expect_err("a limit-carrying windowed selection must fail closed");
        assert_eq!(limited, FfiError::WindowSelectionHasLimit);

        engine.shutdown();
    }

    // #680 deleted `ffi_window_validation_does_not_strand_a_capacity_one_executor`:
    // it asserted the removed native-task census (`max_native_tasks`,
    // `native_task_census`) around a rejected observe. Observations no longer
    // touch a capacity slot at all, so there is nothing to strand; window
    // validation itself is covered by `ffi_window_validation_is_typed`.

    /// Engine shutdown closes a windowed observation (its `next()` terminates in
    /// `None`), and a post-shutdown growth request fails closed, typed.
    #[tokio::test]
    async fn ffi_shutdown_closes_windowed_observer_and_fails_request_rows_closed() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let handle = engine
            .observe(
                FfiFilter {
                    kinds: Some(vec![7_778]),
                    ..FfiFilter::default()
                },
                Some(FfiWindow::Expandable { initial: 1, max: 4 }),
            )
            .expect("windowed observation must start");

        engine.shutdown();

        // Shutdown drops the producer, so `next()` drains any pending frame and
        // then terminates in `None` — the pull-based replacement for `on_closed`.
        loop {
            if next_frame(&handle).await.is_none() {
                break;
            }
        }
        assert!(
            handle.request_rows(2).is_err(),
            "growth after shutdown must fail closed, never hang or panic"
        );
        drop(handle);
    }

    // #680 deleted `simultaneous_query_demand_follow_and_receipt_drains_charge_five_tasks`:
    // its only purpose was asserting the removed native-task census (five charged
    // tasks via `spawn_native_bridge`/`reserve_native_task`/`native_task_census`).
    // Dense simultaneous composition without refusal is now proven by
    // `tests/async_observation_falsifiers.rs::dense_composition_never_refuses_and_delivers_current_state`.

    // #680 deleted `finite_native_executor_refuses_before_acceptance_and_returns_exact_baseline`:
    // it asserted the removed `FfiError::ExecutorSaturated` capacity refusal for
    // observations, a concept that no longer exists (observations never touch the
    // internal adapter pool).

    #[test]
    fn ffi_persistent_store_reset_is_destructive_and_idempotent() {
        let fixture = tempfile::tempdir().expect("tempdir");
        let path = fixture.path().join("nmp.redb");
        let config = NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        };
        let engine = NmpEngine::new(config.clone(), None).expect("persistent engine must build");
        let before = std::fs::read(&path).expect("live FFI store must be readable");
        let refusal = reset_persistent_store(path.to_string_lossy().into_owned())
            .expect_err("live FFI store must refuse reset");
        assert_eq!(
            refusal,
            FfiError::StoreStillOpen {
                path: path
                    .canonicalize()
                    .expect("live FFI store must canonicalize")
                    .to_string_lossy()
                    .into_owned(),
            }
        );
        assert_eq!(
            std::fs::read(&path).expect("refused FFI reset must leave the store readable"),
            before,
            "refused FFI reset must not touch the store file"
        );
        let second_open = NmpEngine::new(config.clone(), None)
            .err()
            .expect("a second FFI engine owner must be refused");
        assert_eq!(
            second_open,
            FfiError::StoreAlreadyOpen {
                path: path
                    .canonicalize()
                    .expect("live FFI store must canonicalize")
                    .to_string_lossy()
                    .into_owned(),
            }
        );

        engine.shutdown();

        reset_persistent_store(path.to_string_lossy().into_owned())
            .expect("closed FFI store must reset");
        assert!(!path.exists(), "FFI reset must remove the canonical store");
        reset_persistent_store(path.to_string_lossy().into_owned())
            .expect("missing FFI store is already reset");

        let reopened = NmpEngine::new(config, None).expect("reset store must reopen fresh");
        reopened.shutdown();
    }

    /// #920: the schema-epoch refusal survives the FFI boundary as its own
    /// variant. An app on this side decides whether to delete a store; it
    /// cannot make that call from `StoreOpenFailed`'s prose.
    ///
    /// The fixture is a nonempty store whose marker this build cannot read,
    /// so `found` is `None`. The table is one this schema never writes.
    /// Retired table names are not recorded here.
    #[test]
    fn ffi_superseded_epoch_store_is_its_own_refusal_and_damaged_bytes_are_not() {
        use redb::{Database, TableDefinition};

        let fixture = tempfile::tempdir().expect("tempdir");

        let superseded = fixture.path().join("superseded-epoch.redb");
        {
            let database = Database::create(&superseded).expect("epoch fixture must create");
            let write = database.begin_write().expect("epoch fixture must begin");
            write
                .open_table(TableDefinition::<u64, &[u8]>::new(
                    "a-table-this-schema-never-writes",
                ))
                .expect("epoch fixture must open a table this schema never writes");
            write.commit().expect("epoch fixture must commit");
        }
        let refusal = NmpEngine::new(
            NmpEngineConfig {
                store_path: Some(superseded.to_string_lossy().into_owned()),
                ..NmpEngineConfig::default()
            },
            None,
        )
        .err()
        .expect("a superseded-epoch store must refuse FFI construction");
        match &refusal {
            FfiError::StoreUnsupportedSchema {
                path,
                expected,
                found,
            } => {
                assert!(
                    path.ends_with("superseded-epoch.redb"),
                    "the FFI refusal must name the store an app would delete: {path}"
                );
                assert!(*expected > 0, "the build's own epoch must cross the FFI");
                assert_eq!(*found, None, "an unreadable marker is absent, not zero");
            }
            other => panic!("the epoch refusal must not collapse at the FFI: {other:?}"),
        }
        let rendered = refusal.to_string();
        for required in [
            "discard and recreate this store to continue",
            "NMP can reacquire the relay-backed read cache",
            "accepted but unpublished writes",
            "permanently lost",
        ] {
            assert!(
                rendered.contains(required),
                "the FFI refusal must state {required:?}: {rendered}"
            );
        }

        let damaged = fixture.path().join("damaged.redb");
        std::fs::write(&damaged, b"not a redb database").expect("damaged fixture must write");
        let generic = NmpEngine::new(
            NmpEngineConfig {
                store_path: Some(damaged.to_string_lossy().into_owned()),
                ..NmpEngineConfig::default()
            },
            None,
        )
        .err()
        .expect("damaged bytes must refuse FFI construction");
        assert!(
            matches!(generic, FfiError::StoreOpenFailed { .. }),
            "damaged bytes must stay the generic FFI open failure: {generic:?}"
        );
        assert!(
            !generic.to_string().contains("discard and recreate"),
            "only the epoch refusal may tell an app to delete the file: {generic}"
        );
    }

    /// A store whose marker this build CAN read reports the number. Reaching
    /// that end to end would mean writing the current epoch's marker address
    /// from outside the crate that owns it, so the projection is pinned here
    /// instead: both `found` shapes cross intact.
    #[test]
    fn ffi_epoch_refusal_carries_both_found_shapes() {
        for found in [Some(10u64), None] {
            let projected = FfiError::from(nmp::EngineError::StoreUnsupportedSchema {
                path: "/canonical/nmp.redb".to_owned(),
                expected: 13,
                found,
            });
            assert_eq!(
                projected,
                FfiError::StoreUnsupportedSchema {
                    path: "/canonical/nmp.redb".to_owned(),
                    expected: 13,
                    found,
                }
            );
        }
    }

    // #680 deleted `reattachment_mapping_is_exhaustive_and_distinct`: it drove the
    // removed pure `reattachment_to_ffi` enum-mapping helper. The real reattach
    // behavior is exercised end-to-end by
    // `ffi_reattach_replays_real_receipt_facts_through_a_fresh_stream`,
    // `ffi_reattach_of_unknown_id_is_not_found`, and
    // `ffi_reattach_of_corrupt_retained_receipt_is_unreadable`.
    //
    // #680 also deleted the callback-observer sign-event tests
    // (`ffi_sign_event_*` on `SignEventObserver`/`max_native_tasks`/
    // `native_task_census`/`ExecutorSaturated`/`await_native_tasks_idle`);
    // the async `NmpSignEventHandle::signed()` API is exercised by the
    // sign-event handle tests instead.

    fn pending_ffi_request() -> FfiSignEventRequest {
        FfiSignEventRequest {
            created_at: 7,
            kind: 1,
            tags: Vec::new(),
            content: "pending ffi".to_string(),
        }
    }

    #[tokio::test]
    async fn ffi_sign_event_returns_the_exact_verified_event_without_publish_api_use() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let author = engine
            .add_private_key_account(ffi_private_key_byte(17), true)
            .expect("account must register");
        let request = FfiSignEventRequest {
            created_at: 1_723_456_789,
            kind: 27_272,
            tags: vec![vec!["t".to_string(), "ffi-sign-only".to_string()]],
            content: "exact ffi body".to_string(),
        };
        let handle = engine
            .sign_event(request.clone())
            .expect("sign operation must start");

        let signed = handle.signed().await.expect("sign operation must succeed");
        assert_eq!(signed.pubkey, author.inner.public_key.to_hex());
        assert_eq!(signed.created_at, request.created_at);
        assert_eq!(signed.kind, request.kind);
        assert_eq!(signed.tags, request.tags);
        assert_eq!(signed.content, request.content);
        assert_eq!(signed.id.len(), 64);
        assert_eq!(signed.sig.len(), 128);
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_missing_current_signing_provider_is_typed() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .add_public_key_account(ffi_public_key(keys.public_key()), true)
            .unwrap();
        let result = engine.sign_event(FfiSignEventRequest {
            created_at: 1,
            kind: 1,
            tags: Vec::new(),
            content: "body".to_string(),
        });
        assert_eq!(
            result.map(|_| ()).unwrap_err(),
            FfiError::NoCurrentSigningProvider,
            "missing current signing provider must refuse synchronously"
        );
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_refuses_malformed_tags_before_admission() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let _author = engine
            .add_private_key_account(ffi_private_key_byte(31), true)
            .unwrap();

        let result = engine.sign_event(FfiSignEventRequest {
            created_at: 1,
            kind: 1,
            tags: vec![Vec::new()],
            content: "malformed".to_string(),
        });
        assert_eq!(
            result.map(|_| ()).unwrap_err(),
            FfiError::InvalidTag { got: Vec::new() },
            "malformed input must fail before operation admission"
        );
        engine.shutdown();
    }

    // #680 deleted `ffi_sign_event_capacity_refusal_precedes_signer_invocation_and_callback`:
    // it asserted the removed `FfiError::ExecutorSaturated` sign-event capacity
    // refusal (`reserve_native_task` + `max_native_tasks`), a concept #680 removed.

    #[test]
    fn ffi_sign_event_after_engine_close_is_typed() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        engine.shutdown();
        let result = engine.sign_event(pending_ffi_request());
        assert_eq!(
            result.map(|_| ()).unwrap_err(),
            FfiError::EngineClosed,
            "a closed engine must refuse before operation admission"
        );
    }

    /// The pull-based replacement for the old callback-reentrancy proof: the
    /// task that awaits `signed()` runs on its own executor (never the engine
    /// reducer thread), so it can freely re-enter engine verbs and drive
    /// `shutdown()` to completion without deadlock.
    #[tokio::test]
    async fn ffi_sign_event_completion_consumer_can_reenter_verbs_and_shutdown() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let author = engine
            .add_private_key_account(ffi_private_key_byte(32), true)
            .unwrap();
        let handle = engine
            .sign_event(pending_ffi_request())
            .expect("operation must start");

        let signed = handle.signed().await.expect("local signer must complete");
        assert_eq!(signed.pubkey, author.inner.public_key.to_hex());
        // Re-enter engine verbs from the awaiting consumer.
        let active = engine.session().expect("consumer can call an engine verb");
        assert_eq!(
            active.current_public_key.unwrap().bytes(),
            author.public_key().bytes()
        );
        engine.shutdown();
        assert!(matches!(engine.session(), Err(FfiError::EngineClosed)));
    }

    /// #52's headline falsifier through the FFI boundary, re-expressed for
    /// #1237: a tampered `FfiWritePayload::Signed` is an INSTRUCTION THAT
    /// CANNOT RESOLVE, so `NmpEngine::publish` refuses the call itself. No
    /// receipt, no stream, and no queue entry exist to inspect -- the write
    /// was never taken into custody, which is what "fails closed" now means.
    #[tokio::test]
    async fn ffi_tampered_signed_publish_is_refused_by_publish_itself() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");

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
                // Tampered after signing: id/sig no longer match this content.
                content: "tampered".to_string(),
                sig: event.sig.to_string(),
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec!["wss://write.example".to_string()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        };

        let error = engine
            .publish(intent)
            .err()
            .expect("a tampered Signed payload must be refused by publish itself");
        match error {
            FfiError::PublishRefused { reason } => assert!(
                reason.contains("signature"),
                "the refusal must name the unverifiable signature, got {reason:?}"
            ),
            other => panic!("expected FfiError::PublishRefused, got {other:?}"),
        }
        assert!(
            engine
                .publish_queue(None, u8::MAX)
                .expect("engine is open")
                .is_empty(),
            "a refused call takes no custody -- there is no queue entry to inspect"
        );

        engine.shutdown();
    }

    /// #47 through the FFI boundary: an `Identity::Explicit` naming a
    /// pubkey with NO registered signer capability is accepted and PARKED as
    /// `Signing { AwaitingSigner }`. It must never silently terminate: after
    /// `AwaitingSigner` the stream stays open (a timeout, never `None`).
    #[tokio::test]
    async fn ffi_explicit_identity_for_unregistered_pubkey_parks_awaiting_capability() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let active = nostr::Keys::generate();
        let overridden = nostr::Keys::generate();
        engine
            .add_public_key_account(ffi_public_key(active.public_key()), true)
            .expect("current account must activate");

        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: crate::types::FfiEventBuilder {
                    kind: 9999,
                    tags: vec![],
                    content: "override park".to_string(),
                    created_at: Some(nostr::Timestamp::now().as_secs()),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec!["wss://write.example".to_string()],
            },
            identity: FfiIdentity::Explicit {
                pubkey: overridden.public_key().to_hex(),
            },
            correlation: None,
        };

        let receipt = engine
            .publish(intent)
            .expect("a well-formed override intent must enqueue");
        assert!(
            receipt.id() > 0,
            "publish must expose its stable receipt id"
        );

        // Acceptance is `publish` returning Ok, not a stream item; the first
        // fact is the park itself.
        assert_eq!(
            next_status(&receipt).await,
            Some(FfiWriteFact::Signing {
                state: FfiSigningState::AwaitingSigner {
                    pubkey: overridden.public_key().to_hex()
                }
            }),
            "the parked pubkey must be the frozen override, never the current account"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), receipt.next())
                .await
                .is_err(),
            "an unregistered override must park retained -- no further fact, and the stream \
             must stay open (a terminal None would be a silent termination)"
        );

        engine.shutdown();
    }

    #[tokio::test]
    async fn ffi_cancel_returns_and_observes_the_same_typed_durable_fact() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .add_public_key_account(ffi_public_key(keys.public_key()), true)
            .unwrap();
        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: crate::types::FfiEventBuilder {
                    kind: 1,
                    tags: Vec::new(),
                    content: "cancel through ffi".to_string(),
                    created_at: Some(10),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec!["wss://write.example".to_string()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        };
        // `publish` returning Ok IS acceptance -- no stream item to await.
        let receipt = engine.publish(intent).unwrap();
        let receipt_id = receipt.id();

        assert_eq!(
            engine.cancel(receipt_id),
            Ok(FfiCancelWriteOutcome::Cancelled)
        );
        let cancelled = FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::NotSent {
                reason: FfiNotSentReason::Cancelled,
            },
        };
        let mut observed = false;
        while let Some(status) = next_status(&receipt).await {
            if status == cancelled {
                observed = true;
                break;
            }
        }
        assert!(observed);
        assert_eq!(
            engine.cancel(receipt_id),
            Ok(FfiCancelWriteOutcome::Cancelled)
        );
        assert_eq!(
            engine.cancel(u64::MAX),
            Err(FfiCancelWriteError::UnknownReceipt {
                receipt_id: u64::MAX
            })
        );
        engine.shutdown();
        assert_eq!(
            engine.cancel(receipt_id),
            Err(FfiCancelWriteError::EngineClosed)
        );
    }

    #[test]
    fn session_projects_the_rust_current_account_and_closed_state() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let pubkey = nostr::Keys::generate().public_key();

        assert!(
            engine
                .session()
                .expect("engine is open")
                .current_public_key
                .is_none(),
            "a new engine must remain read-only"
        );
        engine
            .add_public_key_account(ffi_public_key(pubkey), true)
            .expect("account must activate");
        assert_eq!(
            engine
                .session()
                .expect("engine is open")
                .current_public_key
                .unwrap()
                .bytes(),
            pubkey.to_bytes()
        );

        engine.shutdown();
        assert!(matches!(engine.session(), Err(FfiError::EngineClosed)));
    }

    /// #99 end-to-end reattach: a real durable intent (no signing provider is configured,
    /// so it parks in a retained `Signing { AwaitingSigner }` steady state) is
    /// reattached through a SECOND, independent stream that replays the
    /// identical durable `WriteFact` prefix.
    #[tokio::test]
    async fn ffi_reattach_replays_real_receipt_facts_through_a_fresh_stream() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .add_public_key_account(ffi_public_key(keys.public_key()), true)
            .expect("account must activate");

        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: crate::types::FfiEventBuilder {
                    kind: 9999,
                    tags: vec![],
                    content: "reattach e2e".to_string(),
                    created_at: Some(nostr::Timestamp::now().as_secs()),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec!["wss://write.example".to_string()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        };

        let receipt = engine
            .publish(intent)
            .expect("a well-formed unsigned intent must enqueue");
        let receipt_id = receipt.id();
        assert!(receipt_id > 0, "publish must expose its stable receipt id");

        // Block for the exact retained steady state on the ORIGINAL stream first.
        let parked = FfiWriteFact::Signing {
            state: FfiSigningState::AwaitingSigner {
                pubkey: keys.public_key().to_hex(),
            },
        };
        assert_eq!(next_status(&receipt).await, Some(parked.clone()));

        // Reattach through a FRESH stream -- exercises the real durable-prefix
        // replay in `NmpEngine::reattach_receipt`.
        let replay = match engine
            .reattach_receipt(receipt_id)
            .expect("reattach call must succeed while the engine is open")
        {
            FfiReceiptReattachment::Attached { stream } => stream,
            FfiReceiptReattachment::NotFound => panic!("expected Attached, got NotFound"),
            FfiReceiptReattachment::RetainedButUnreadable => {
                panic!("expected Attached, got RetainedButUnreadable")
            }
        };

        assert_eq!(
            next_status(&replay).await,
            Some(parked),
            "replay must reconstruct the identical durable prefix from the store"
        );

        engine.shutdown();
    }

    /// #99: an unknown receipt id reattaches to `NotFound` (no stream, no facts).
    #[test]
    fn ffi_reattach_of_unknown_id_is_not_found() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let outcome = engine
            .reattach_receipt(999_999)
            .expect("reattach call must succeed while the engine is open");
        assert!(matches!(outcome, FfiReceiptReattachment::NotFound));
        engine.shutdown();
    }

    /// #99's `RetainedButUnreadable` half: a GENUINELY corrupt retained receipt
    /// (real undecodable bytes in a real `RedbStore` file) reattaches to
    /// `RetainedButUnreadable` (no stream, no facts) through the FFI boundary.
    #[tokio::test]
    async fn ffi_reattach_of_corrupt_retained_receipt_is_unreadable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("corrupt-receipt.redb");

        let receipt_id = {
            let engine = NmpEngine::new(
                NmpEngineConfig {
                    store_path: Some(path.to_string_lossy().into_owned()),
                    ..NmpEngineConfig::default()
                },
                None,
            )
            .expect("engine must build");
            let keys = nostr::Keys::generate();
            engine
                .add_public_key_account(ffi_public_key(keys.public_key()), true)
                .expect("account must activate");
            let intent = FfiWriteIntent {
                payload: FfiWritePayload::Event {
                    builder: crate::types::FfiEventBuilder {
                        kind: 9999,
                        tags: vec![],
                        content: "corrupt-receipt".to_string(),
                        created_at: Some(nostr::Timestamp::now().as_secs()),
                    },
                },
                routing: FfiWriteRouting::Explicit {
                    relays: vec!["wss://write.example".to_string()],
                },
                identity: FfiIdentity::Active,
                correlation: None,
            };
            let receipt = engine
                .publish(intent)
                .expect("a well-formed unsigned intent must enqueue");
            let receipt_id = receipt.id();
            assert_eq!(
                next_status(&receipt).await,
                Some(FfiWriteFact::Signing {
                    state: FfiSigningState::AwaitingSigner {
                        pubkey: keys.public_key().to_hex()
                    }
                }),
                "the write must have a durable retained fact before it is corrupted"
            );
            engine.shutdown();
            receipt_id
        };

        // Overwrite the receipt's own durable row with undecodable bytes.
        const RECEIPTS: redb::TableDefinition<&[u8; 8], &[u8]> =
            redb::TableDefinition::new("publish_queue_receipts");
        let db = redb::Database::open(&path).expect("redb: reopen for corruption");
        let tx = db.begin_write().expect("redb: begin_write");
        {
            let mut table = tx
                .open_table(RECEIPTS)
                .expect("redb: open publish_queue_receipts");
            let key = receipt_id.to_be_bytes();
            let mut value = table
                .get(&key)
                .expect("redb: read retained receipt")
                .expect("retained receipt row")
                .value()
                .to_vec();
            value[4] = 200;
            table
                .insert(&key, value.as_slice())
                .expect("redb: write corrupt receipt bytes");
        }
        tx.commit().expect("redb: commit corruption");
        drop(db);

        let engine = NmpEngine::new(
            NmpEngineConfig {
                store_path: Some(path.to_string_lossy().into_owned()),
                ..NmpEngineConfig::default()
            },
            None,
        )
        .expect("engine must reopen over the corrupted store");
        let outcome = engine
            .reattach_receipt(receipt_id)
            .expect("reattach call must succeed while the engine is open");
        assert!(matches!(
            outcome,
            FfiReceiptReattachment::RetainedButUnreadable
        ));

        engine.shutdown();
    }

    /// codex-nova's cancellation proof, ported to the pull-based handle: calling
    /// `cancel()` on the SAME `NmpRowStream` from two `Arc` owners, then dropping
    /// both, wakes a parked `next()` to `None` and keeps yielding `None` -- never
    /// a hang, never a post-cancel frame.
    #[tokio::test]
    async fn ffi_repeated_cancel_across_arc_owners_and_drop_yields_terminal_none() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");

        let handle = engine
            .observe(
                FfiFilter {
                    kinds: Some(vec![9999]),
                    ..FfiFilter::default()
                },
                None,
            )
            .expect("a well-formed filter must be accepted");

        // Two independent `Arc` owners of the SAME `NmpRowStream` -- both call
        // `cancel()`, then both are dropped.
        let handle_other_owner = Arc::clone(&handle);
        handle.cancel();
        handle_other_owner.cancel();
        handle.cancel(); // idempotent

        assert!(
            next_frame(&handle).await.is_none(),
            "cancel wakes next() to a terminal None, never a hang"
        );
        assert!(
            next_frame(&handle).await.is_none(),
            "None is stable after cancel -- no post-cancel frame is ever observed"
        );
        drop(handle);
        drop(handle_other_owner);

        engine.shutdown();
    }

    /// The slow-consumer conflation falsifier, ported to pull-based delivery.
    /// After the initial current-state frame is consumed, the consumer stops
    /// pulling while durable local acceptance produces many distinct rows and
    /// cancellation retracts half of them. The single subsequent `next()` must
    /// deliver the exact net transition (the engine mailbox folds obsolete
    /// intermediates), then cancellation closes it once.
    #[tokio::test]
    async fn ffi_slow_consumer_receives_one_exact_rebased_frame_then_closes() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .add_public_key_account(ffi_public_key(keys.public_key()), true)
            .expect("engine must accept a read-only active identity");

        let kind = nostr::Kind::Custom(44_646);
        let handle = engine
            .observe(
                FfiFilter {
                    kinds: Some(vec![kind.as_u16()]),
                    ..FfiFilter::default()
                },
                None,
            )
            .expect("query must open");

        // Consume the initial current-state frame (empty store -> empty deltas).
        let initial = next_frame(&handle)
            .await
            .expect("the initial current-state frame must arrive");
        assert!(initial.deltas.is_empty());

        // Now, WITHOUT pulling again, produce 64 rows and cancel the evens. They
        // fold into the single engine-owned mailbox slot -- the slow-consumer path.
        let mut expected = BTreeSet::new();
        for index in 0..64u64 {
            let unsigned = nostr::UnsignedEvent::new(
                keys.public_key(),
                nostr::Timestamp::from(10_000 + index),
                kind,
                Vec::new(),
                format!("blocked-row-{index}"),
            );
            let event_id = nostr::EventId::new(
                &unsigned.pubkey,
                &unsigned.created_at,
                &unsigned.kind,
                &unsigned.tags,
                &unsigned.content,
            );
            let receipt = engine
                .engine
                .publish(nmp::WriteIntent {
                    payload: nmp::WritePayload::Event(nmp::EventBuilder {
                        kind: unsigned.kind,
                        tags: unsigned.tags.iter().cloned().collect(),
                        content: unsigned.content.clone(),
                        created_at: Some(unsigned.created_at),
                    }),
                    routing: nmp::WriteRouting::Auto,
                    identity: nmp::Identity::Active,
                    correlation: None,
                })
                .expect("local durable acceptance must succeed");
            if index % 2 == 0 {
                engine
                    .engine
                    .cancel(receipt.id)
                    .expect("an unsigned pending row must remain cancellable");
            } else {
                expected.insert(event_id.to_hex());
            }
        }

        // A synchronous diagnostics open is a command-loop barrier: every
        // preceding publish/cancel effect has folded into the row mailbox slot.
        drop(
            engine
                .engine
                .observe_diagnostics()
                .expect("barrier observation must open"),
        );

        let rebased = next_frame(&handle)
            .await
            .expect("the exact rebased frame must follow");
        let actual: BTreeSet<_> = rebased
            .deltas
            .iter()
            .map(|delta| match delta {
                FfiRowDelta::Added { row } => row.id.clone(),
                other => panic!("net add/remove cancellation must leave only additions: {other:?}"),
            })
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(rebased.deltas.len(), expected.len());

        handle.cancel();
        assert!(
            next_frame(&handle).await.is_none(),
            "cancel must close the stream once"
        );
        engine.shutdown();
    }

    /// #125's falsifier ported to the pull path: a receipt stream must terminate
    /// in `None` when its `WriteFact` sender is dropped, after real delivery.
    ///
    /// The old vehicle was a tampered `Signed` payload, which #1237 moved to a
    /// synchronous `publish` refusal that creates no stream at all. The
    /// property under test is about a stream that ENDS, so it now rides on a
    /// real whole-write terminal: an explicit cancellation.
    #[tokio::test]
    async fn ffi_receipt_stream_ends_with_none_when_sender_dropped() {
        let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine must build");

        let keys = nostr::Keys::generate();
        engine
            .add_public_key_account(ffi_public_key(keys.public_key()), true)
            .expect("account must activate");
        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: crate::types::FfiEventBuilder {
                    kind: 9999,
                    tags: vec![],
                    content: "stream terminal".to_string(),
                    created_at: Some(nostr::Timestamp::now().as_secs()),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec!["wss://write.example".to_string()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        };

        let receipt = engine
            .publish(intent)
            .expect("a well-formed unsigned intent must enqueue");
        engine
            .cancel(receipt.id())
            .expect("an unsigned write cancels");

        // The stream is genuinely active first (the terminal fact arrives).
        let cancelled = FfiWriteFact::Outcome {
            outcome: FfiWriteOutcome::NotSent {
                reason: FfiNotSentReason::Cancelled,
            },
        };
        let mut observed = false;
        while let Some(status) = next_status(&receipt).await {
            if status == cancelled {
                observed = true;
                break;
            }
        }
        assert!(observed, "the whole-write terminal must be delivered");

        assert!(
            next_status(&receipt).await.is_none(),
            "the receipt stream must end in None once the sender is dropped, not hang"
        );

        engine.shutdown();
    }
}
