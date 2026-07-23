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
//! `nmp`'s async pull surfaces. Long-lived observations expose `next()` and
//! `cancel()` directly; no drain-thread/callback-observer bridge remains.
//!
//! Directory: `nmp_router::LiveDirectory` (M5's self-bootstrapping outbox,
//! now assembled inside `nmp::Engine::new`) is what backs every `NmpEngine`
//! -- a Swift app supplies ONLY the operator indexer relay set; every
//! author's NIP-65 write relays (including the app's own account) are
//! discovered by the engine itself, live, via its own internal kind:10002
//! reads against those same indexers (`nmp_engine::core::EngineCore`'s
//! auto-discovery). `NmpEngineConfig` no longer accepts a pre-resolved
//! write-relay map -- there is nothing for a caller to resolve up front.

use std::sync::{Arc, Mutex};

use crate::auth::{
    FfiAccountRegistration, FfiAuthPolicyAdapter, FfiAuthPolicyCallback, FfiAuthPolicyRegistration,
};
use crate::convert::{
    cancel_write_error_to_ffi, cancel_write_outcome_to_ffi, demand_from_ffi,
    diagnostics_snapshot_to_ffi, filter_from_ffi, frame_to_ffi, parse_pubkey,
    relay_information_error_kind, sign_event_failure, sign_event_request_from_ffi,
    sign_event_start_error, signed_event_to_ffi, window_from_ffi, write_intent_from_ffi,
    write_status_to_ffi, FfiError, FfiRequestRowsError, WriteStatusRef,
};
use crate::nip02::{NmpFollowActionStream, NmpFollowStream};
use crate::types::{
    FfiCancelWriteError, FfiCancelWriteOutcome, FfiCorrelationReattachment, FfiDemand,
    FfiDiagnosticsSnapshot, FfiFilter, FfiFrame, FfiReceiptReattachment, FfiRelayInformation,
    FfiRelayInformationCachePolicy, FfiRelayInformationDocument, FfiRelayInformationFreshness,
    FfiRelayInformationLimitations, FfiSignEventFailure, FfiSignEventRequest, FfiSignedEvent,
    FfiWindow, FfiWriteIntent, FfiWriteStatus,
};
use nmp::ReceiptReattachment;

/// Start a follow/unfollow action and expose its status stream (#680). A valid
/// target starts the transient action worker (one thread on the engine's
/// internal blocking-adapter pool, per user action — never observation-count
/// driven); an unparseable target yields a one-shot stream carrying a single
/// `Failed(InvalidTarget)` fact. The status FIFO is delivered pull-based over
/// [`NmpFollowActionStream`], so no drain thread bridges it.
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

/// Construction config for [`NmpEngine::new`]. See the module doc: the only
/// relay facts a caller ever supplies are the three operator-configured
/// lanes -- `indexer_relays`, `app_relays`, `fallback_relays`
/// (`routing-and-ownership.md` §2.1) -- everything else is discovered live.
#[derive(uniffi::Record, Clone, Debug)]
pub struct NmpEngineConfig {
    /// `None` -> in-memory store (nothing survives a restart). `Some(path)`
    /// -> a persistent `RedbStore` opened at that path (the same file
    /// reopened across restarts is what preserves source-scoped evidence for
    /// a cold, offline read -- ledger #7).
    pub store_path: Option<String>,
    pub indexer_relays: Vec<String>,
    /// Operator app relay set (`Lane::AppRelay`). Default empty.
    pub app_relays: Vec<String>,
    /// Operator fallback relay set (`Lane::Fallback`). Default empty.
    pub fallback_relays: Vec<String>,
    /// Local/private relay HOSTS the operator explicitly opts into despite
    /// the SSRF admission policy (issue #121). A DISCOVERED (network-sourced
    /// kind:10002) relay on a loopback / RFC-1918 / link-local / `.onion`
    /// host is rejected by default; listing its host here (e.g. `"127.0.0.1"`
    /// or `"localhost"`) re-admits discovered relays on that exact host.
    /// Host-only match (port- and path-insensitive). Default empty.
    ///
    /// `default = []` keeps this field OPTIONAL for existing foreign-language
    /// callers — adding it must not break records constructed before #121.
    #[uniffi(default = [])]
    pub allowed_local_relay_hosts: Vec<String>,
    /// The one whole-engine relay ceiling. It bounds the complete compiled
    /// demand and the transport worker set with the same effective value.
    /// Legacy zero is normalized to the finite default, never uncapped.
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
            indexer_relays: Vec::new(),
            app_relays: Vec::new(),
            fallback_relays: Vec::new(),
            allowed_local_relay_hosts: Vec::new(),
            max_relays: DEFAULT_MAX_RELAYS,
            max_auth_capabilities: DEFAULT_MAX_AUTH_CAPABILITIES,
        }
    }
}

/// Destructively reset a closed persistent NMP store. This removes all
/// canonical engine state at `store_path`, while leaving any separately
/// configured native account checkpoint untouched. A live engine in this
/// process using the same canonical path is refused with
/// `FfiError::StoreStillOpen` without touching the file. Shut down or drop
/// that engine first. The operation is idempotent when the store does not
/// exist; cross-process exclusion is not provided.
#[uniffi::export]
pub fn reset_persistent_store(store_path: String) -> Result<(), FfiError> {
    nmp::Engine::reset_persistent_store(store_path)?;
    Ok(())
}

/// Generate a fresh local-account secret key via OS RNG (hex-encoded,
/// `NmpEngine::add_account`-compatible) -- the one keygen-only FFI door #588
/// asks for. This function touches no engine state, installs no signer, and
/// persists nothing: a native wrapper composes it with the existing
/// `add_account` to give a clean-start client its first identity, inheriting
/// that method's save-with-rollback choreography and checkpoint tracking
/// wholesale instead of a second, parallel registration pipeline.
#[uniffi::export]
pub fn generate_account_secret_key() -> String {
    nostr::Keys::generate().secret_key().to_secret_hex()
}

// Keep the native-facing literal pinned to the canonical finite default.
const _: () = assert!(DEFAULT_MAX_RELAYS == 10);
const _: () = assert!(DEFAULT_MAX_AUTH_CAPABILITIES == 64);

impl From<NmpEngineConfig> for nmp::EngineConfig {
    fn from(config: NmpEngineConfig) -> Self {
        nmp::EngineConfig {
            store_path: config.store_path,
            indexer_relays: config.indexer_relays,
            app_relays: config.app_relays,
            fallback_relays: config.fallback_relays,
            allowed_local_relay_hosts: config.allowed_local_relay_hosts,
            max_relays: config.max_relays as usize,
            max_auth_capabilities: config.max_auth_capabilities as usize,
        }
    }
}

/// The UniFFI-exported engine object. `new` is the ONE construction call the
/// M4 kill test (plan §7) requires -- everything past construction is a
/// method call on this object, never a second container the app must adopt.
/// Wraps a single [`nmp::Engine`] -- the one supported Rust product surface
/// -- rather than independently assembling `nmp-store`/`nmp-router`/
/// `nmp-transport`/`nmp-resolver` mechanism types (#52).
#[derive(uniffi::Object)]
pub struct NmpEngine {
    pub(crate) engine: Arc<nmp::Engine>,
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
    pub fn new(config: NmpEngineConfig) -> Result<Arc<Self>, FfiError> {
        let engine = Arc::new(nmp::Engine::new(config.into())?);
        Ok(Arc::new(Self { engine }))
    }

    /// Register an account from its secret key (hex or bech32 `nsec`). The
    /// key crosses this boundary exactly once, as a value, and lives in the
    /// engine from this point on (VISION ledger #12; M4 plan §5) -- this
    /// method does NOT make the account active, call
    /// [`Self::set_active_account`] for that. Returns the opaque exact
    /// registration required for stale-safe explicit removal.
    pub fn add_account(&self, secret_key: String) -> Result<Arc<FfiAccountRegistration>, FfiError> {
        let registration = self.engine.add_account(&secret_key)?;
        Ok(Arc::new(FfiAccountRegistration {
            inner: registration,
        }))
    }

    /// Remove only the account installation proven by `registration`.
    /// Repeated or stale cleanup returns `false`.
    pub fn remove_account(
        &self,
        registration: Arc<FfiAccountRegistration>,
    ) -> Result<bool, FfiError> {
        Ok(self.engine.remove_account(&registration.inner)?)
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

    /// Re-root every reactive query AND the active signing capability
    /// together onto `pubkey` (`None` -> logged-out / read-only). `pubkey`
    /// need not have been added via [`Self::add_account`] -- read-only
    /// browsing of an account this app holds no key for is legal; any
    /// publish attempted while active in that state terminates
    /// `WriteStatus::Failed`, never a panic (M4 plan §5).
    pub fn set_active_account(&self, pubkey: Option<String>) -> Result<(), FfiError> {
        let pk = pubkey.as_deref().map(parse_pubkey).transpose()?;
        self.engine.set_active_account(pk)?;
        Ok(())
    }

    /// Return the account currently rooting reactive identity and unsigned
    /// writes. The secret and signer capability remain engine-owned; native
    /// callers receive only the public key needed for presentation.
    pub fn active_account(&self) -> Result<Option<String>, FfiError> {
        Ok(self.engine.active_account()?.map(|pubkey| pubkey.to_hex()))
    }

    /// Sign one exact event through the active account without accepting a
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

    /// Observe the active account's relationship to `target` through the
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

    /// Ask NMP to follow `target`. This is the complete NIP-02 action: it
    /// waits for the module's source-evidence policy, preserves the exact
    /// kind:3 base, atomically guards that base, signs, routes, and streams
    /// the durable receipt. The native button owns none of those steps; it
    /// only awaits [`NmpFollowActionStream::next`].
    pub fn follow(&self, target: String) -> Arc<NmpFollowActionStream> {
        start_following_action(self.engine.clone(), target, nmp_nip02::FollowChange::Follow)
    }

    /// The inverse of [`Self::follow`], with the same acquisition,
    /// compare-and-swap, signer, routing, and receipt guarantees.
    pub fn unfollow(&self, target: String) -> Arc<NmpFollowActionStream> {
        start_following_action(
            self.engine.clone(),
            target,
            nmp_nip02::FollowChange::Unfollow,
        )
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
        let subscription = self
            .engine
            .observe_async(nmp::LiveQuery::from_filter(filter), window)?;
        Ok(Arc::new(NmpRowStream {
            inner: subscription,
        }))
    }

    /// Open a live subscription over an explicit [`FfiDemand`] (#107) --
    /// the constructor an app reaches for once [`Self::observe`]'s bare
    /// `FfiFilter` (which always takes `Demand::from_filter`'s static
    /// default) isn't enough: declaring `Pinned` wire authority, a non-
    /// default `AccessContext`, or a non-`Agnostic` `CacheMode`. Same
    /// pull-based/cancel/window shape as `observe` in every other respect
    /// (see that method's doc for the `window` policy).
    pub fn observe_demand(
        &self,
        query: FfiDemand,
        window: Option<FfiWindow>,
    ) -> Result<Arc<NmpRowStream>, FfiError> {
        let demand = demand_from_ffi(query)?;
        let window = window_from_ffi(window)?;
        let subscription = self.engine.observe_async(nmp::LiveQuery(demand), window)?;
        Ok(Arc::new(NmpRowStream {
            inner: subscription,
        }))
    }

    /// Enqueue a write (#680). The returned [`NmpReceiptStream`] exposes the
    /// stable receipt id ([`NmpReceiptStream::id`]) and streams every
    /// `WriteStatus` this intent ever reaches (ledger #9 -- enqueue is not
    /// converged; the first value is never a terminal for a durable/
    /// at-most-once intent) via `async fn next()`. A caller-supplied `Signed`
    /// payload that fails verification is no longer a synchronous error here
    /// (that guarantee moved to `nmp-engine::core::EngineCore::on_publish`'s
    /// acceptance boundary, Unit A0/#56, so it holds for every entry point) --
    /// it surfaces as `WriteStatus::Failed`, the FIRST and only status the
    /// stream delivers, with no preceding `Accepted`. Exhaustion of the
    /// pre-acceptance correlation namespace instead returns a typed `FfiError`
    /// synchronously: no receipt id or stream exists.
    pub fn publish(&self, intent: FfiWriteIntent) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let write_intent = write_intent_from_ffi(intent)?;
        let receipt = self.engine.publish_tracked(write_intent)?;
        Ok(NmpReceiptStream::new(receipt))
    }

    /// Compose an ordinary kind:9 NIP-29 message from semantic inputs
    /// (#156). The caller supplies no author, timestamp, kind, bech32
    /// encoding, or raw tags: NMP reads the active account, owns event time,
    /// materializes ordered/deduplicated `nostr:npub…` content references,
    /// and composes `p`/reply-`e`/`h` plus pinned-host routing. `previous` is
    /// temporarily omitted until NMP can prove a live host acceptance window;
    /// no caller row or provenance claim enters the path.
    /// Publish the returned take-once value through [`Self::publish_composed`].
    pub fn group_message_intent(
        &self,
        host: String,
        group_id: String,
        content: String,
        recipient_pubkeys: Vec<String>,
        reply_to: Option<crate::nip29::FfiGroupReplyParent>,
    ) -> Result<Arc<crate::nip29::FfiComposedWriteIntent>, FfiError> {
        crate::nip29::group_message_intent(
            &self.engine,
            host,
            group_id,
            content,
            recipient_pubkeys,
            reply_to,
        )
    }

    /// Compose a durable, author-outbox-routed NIP-22 comment `WriteIntent`
    /// (#572). Unlike [`Self::group_message_intent`], this needs no engine
    /// state at all -- `nmp_nip22::comment_intent` takes author/time as
    /// explicit caller parameters -- but lives here for the same "engine
    /// door" naming symmetry. `correlation` (#591) passes straight through
    /// to `WriteIntent.correlation`. Publish the returned take-once value
    /// through [`Self::publish_composed`].
    #[allow(clippy::too_many_arguments)]
    pub fn comment_intent(
        &self,
        root: crate::nip22::FfiCommentRoot,
        parent: crate::nip22::FfiCommentParent,
        author_pubkey: String,
        created_at: u64,
        content: String,
        correlation: Option<String>,
    ) -> Result<Arc<crate::nip29::FfiComposedWriteIntent>, FfiError> {
        crate::nip22::comment_intent(
            root,
            parent,
            author_pubkey,
            created_at,
            content,
            correlation,
        )
    }

    /// Publish a `nmp_nip29::compose_group_send`-composed intent (#115).
    /// Take-once: `intent` is consumed by this call (`FfiComposedWriteIntent
    /// ::take`) -- a second call on the SAME handle fails closed with
    /// `FfiError::IntentAlreadyConsumed` rather than silently re-publishing
    /// a stale template. Otherwise identical to [`Self::publish`]'s body
    /// (same pull-based receipt stream); `write_intent_from_ffi`
    /// never runs for this path -- the intent was already composed
    /// directly, never round-tripped through the raw `FfiWriteRouting`
    /// conversion (which withholds `PinnedHost` entirely).
    pub fn publish_composed(
        &self,
        intent: Arc<crate::nip29::FfiComposedWriteIntent>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let write_intent = intent.take()?;
        let receipt = self.engine.publish_tracked(write_intent)?;
        Ok(NmpReceiptStream::new(receipt))
    }

    /// Attach to a retained receipt without collapsing corrupt durable
    /// evidence into the same result as an unknown id (#680). The `Attached`
    /// variant carries an [`NmpReceiptStream`] that transparently traverses
    /// durable `WriteStatus` facts in finite pages and streams onward,
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
    /// could durably persist the receipt id `publish`/`publish_composed`
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

/// The app-facing pull-based handle to a live subscription (returned by
/// [`NmpEngine::observe`], #680). Await [`Self::next`] for the next
/// [`FfiFrame`], or `None` once the observation ends; `Drop`/[`Self::cancel`]
/// withdraw it. Holds ONLY the `Send + Sync` [`nmp::AsyncSubscription`] — no
/// dedicated drain thread and no raw engine-control capability ever reaches
/// this crate. Awaiting `next()` reserves no native thread and no runtime; the
/// engine mailbox wakes the parked future.
#[derive(uniffi::Object)]
pub struct NmpRowStream {
    inner: nmp::AsyncSubscription,
}

#[uniffi::export]
impl NmpRowStream {
    /// Await the next observation [`FfiFrame`], or `None` once the engine has
    /// torn the subscription down (cancel / shutdown / producer drop).
    /// [`FfiError::ConcurrentNext`] if a `next()` is already in flight — the
    /// stream is single-consumer.
    pub async fn next(&self) -> Result<Option<FfiFrame>, FfiError> {
        match self.inner.next().await {
            Ok(Some(frame)) => Ok(Some(frame_to_ffi(frame))),
            Ok(None) => Ok(None),
            Err(_) => Err(FfiError::ConcurrentNext),
        }
    }

    /// Withdraw the subscription now, rather than waiting for `Drop` (a Swift
    /// `deinit` can be delayed by ARC in ways an app may want to preempt).
    /// Wakes any parked `next()` to `None`. Safe to call more than once, and
    /// safe to never call at all.
    pub fn cancel(&self) {
        self.inner.cancel();
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
        self.inner
            .request_rows(at_least)
            .map_err(FfiRequestRowsError::from)
    }
}

impl Drop for NmpRowStream {
    fn drop(&mut self) {
        self.inner.cancel();
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

/// The app-facing pull-based receipt stream (returned by [`NmpEngine::publish`]/
/// [`NmpEngine::publish_composed`], and the `Attached` reattachment, #680). It
/// exposes the stable store-issued receipt id via [`Self::id`] and delivers
/// ordered `WriteStatus` facts via `async fn next()`. Live delivery is a finite
/// FIFO that reports typed lag. Receipt facts are durable: the persisted
/// outbox/redb store is the source of truth, so a dropped or lagged stream can
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
        receiver: Arc<nmp::AsyncFifoReceiver<nmp::WriteStatus>>,
        next_cursor: Option<u64>,
    },
    Cancelled,
}

impl NmpReceiptStream {
    fn new(receipt: nmp::ReceiptStream) -> Arc<Self> {
        Arc::new(Self {
            id: receipt.id,
            engine: None,
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
        statuses: nmp::FifoReceiver<nmp::WriteStatus>,
        next_cursor: Option<u64>,
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
    ) -> Option<(Arc<nmp::AsyncFifoReceiver<nmp::WriteStatus>>, Option<u64>)> {
        let delivery = self.delivery.lock().unwrap();
        match &*delivery {
            ReceiptDelivery::Active {
                receiver,
                next_cursor,
            } => Some((receiver.clone(), *next_cursor)),
            ReceiptDelivery::Cancelled => None,
        }
    }

    fn install_page(
        &self,
        prior: &Arc<nmp::AsyncFifoReceiver<nmp::WriteStatus>>,
        statuses: nmp::FifoReceiver<nmp::WriteStatus>,
        next_cursor: Option<u64>,
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
}

#[uniffi::export]
impl NmpReceiptStream {
    /// The stable store-issued receipt id, needed for process-later
    /// reattachment ([`NmpEngine::reattach_receipt`]) and explicit cancellation
    /// ([`NmpEngine::cancel`]).
    pub fn id(&self) -> u64 {
        self.id.0
    }

    /// Await the next `WriteStatus`, or `None` once the intent has fully
    /// resolved or the engine has shut down. [`FfiError::ConcurrentNext`] on an
    /// overlapping call.
    pub async fn next(&self) -> Result<Option<FfiWriteStatus>, FfiError> {
        use std::sync::atomic::Ordering;

        if self.reading.swap(true, Ordering::AcqRel) {
            return Err(FfiError::ConcurrentNext);
        }
        let _reading = ReceiptReadingGuard(&self.reading);

        loop {
            let Some((receiver, next_cursor)) = self.current_receiver() else {
                return Ok(None);
            };
            match receiver.next().await {
                Ok(Some(status)) => {
                    return Ok(Some(write_status_to_ffi(WriteStatusRef(&status))));
                }
                Err(nmp::FifoNextError::ConcurrentNext) => {
                    return Err(FfiError::ConcurrentNext);
                }
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
        FfiAccessContext, FfiBinding, FfiCacheMode, FfiDemand, FfiDurability, FfiFilter, FfiFrame,
        FfiRowDelta, FfiSignEventFailure, FfiSignEventRequest, FfiSourceAuthority, FfiWindow,
        FfiWindowLoad, FfiWritePayload, FfiWriteRouting, FfiWriteStatus,
    };
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    // #680 replaced push/callback observers with pull-based async stream handles:
    // `observe`/`observe_demand`/`observe_diagnostics`/`publish`/`sign_event` take
    // no observer argument and return `Arc<Nmp*Stream>`/`Arc<NmpSignEventHandle>`
    // whose `async fn next()`/`signed()` drive delivery. `None` from `next()`
    // replaces `on_closed`. The `RowObserver`/`DiagnosticsObserver`/
    // `ReceiptObserver`/`SignEventObserver`/`FollowObserver` traits are deleted,
    // as is the native-task capacity/census surface. Tests below drive the async
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
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a frame must arrive within the lifecycle bound")
            .expect("row next() is not a concurrent-misuse")
    }

    /// Await the next receipt status within the lifecycle bound. `None` is the
    /// terminal signal (the intent fully resolved / engine shutdown).
    async fn next_status(stream: &NmpReceiptStream) -> Option<FfiWriteStatus> {
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
    fn ffi_account_registration_is_explicit_repeatable_and_stale_safe() {
        let engine = NmpEngine::new(NmpEngineConfig {
            max_auth_capabilities: 1,
            ..NmpEngineConfig::default()
        })
        .unwrap();
        let secret = format!("{:064x}", 41u8);
        let first = engine.add_account(secret.clone()).unwrap();
        let replacement = engine.add_account(secret).unwrap();

        assert_eq!(first.public_key(), replacement.public_key());
        assert!(!engine.remove_account(Arc::clone(&first)).unwrap());
        assert!(engine.remove_account(Arc::clone(&replacement)).unwrap());
        assert!(!engine.remove_account(replacement).unwrap());
    }

    #[test]
    fn ffi_auth_policy_registration_is_explicit_repeatable_and_stale_safe() {
        let engine = NmpEngine::new(NmpEngineConfig {
            max_auth_capabilities: 1,
            ..NmpEngineConfig::default()
        })
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
        let engine = NmpEngine::new(NmpEngineConfig {
            max_auth_capabilities: 0,
            ..NmpEngineConfig::default()
        })
        .unwrap();
        assert_eq!(
            engine.add_account(format!("{:064x}", 42u8)).unwrap_err(),
            FfiError::AuthCapabilityRegistryFull { limit: 0 }
        );
    }

    fn ffi_windowed_demand(author: String) -> FfiDemand {
        FfiDemand {
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

        let engine = NmpEngine::new(NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        })
        .unwrap();
        let handle = engine
            .observe_demand(
                ffi_windowed_demand(keys.public_key().to_hex()),
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
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");

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
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
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
        let engine = NmpEngine::new(config.clone()).expect("persistent engine must build");
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

        engine.shutdown();

        reset_persistent_store(path.to_string_lossy().into_owned())
            .expect("closed FFI store must reset");
        assert!(!path.exists(), "FFI reset must remove the canonical store");
        reset_persistent_store(path.to_string_lossy().into_owned())
            .expect("missing FFI store is already reset");

        let reopened = NmpEngine::new(config).expect("reset store must reopen fresh");
        reopened.shutdown();
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
    // the async `NmpSignEventHandle::signed()` surface is exercised by the
    // sign-event handle tests instead.

    struct MismatchedFfiSigner {
        reported: nostr::PublicKey,
        actual: nostr::Keys,
    }

    impl nmp_signer::SigningCapability for MismatchedFfiSigner {
        fn public_key(&self) -> Option<nostr::PublicKey> {
            Some(self.reported)
        }

        fn sign(&self, unsigned: nostr::UnsignedEvent) -> nmp_signer::SignerOp<nostr::Event> {
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

    struct PendingFfiSigner {
        public_key: nostr::PublicKey,
        cancellations: Arc<AtomicUsize>,
        completion: Mutex<Option<nmp_signer::PendingSignerSender<nostr::Event>>>,
    }

    impl nmp_signer::SigningCapability for PendingFfiSigner {
        fn public_key(&self) -> Option<nostr::PublicKey> {
            Some(self.public_key)
        }

        fn sign(&self, _unsigned: nostr::UnsignedEvent) -> nmp_signer::SignerOp<nostr::Event> {
            let cancellations = Arc::clone(&self.cancellations);
            let (sender, operation) =
                nmp_signer::SignerOp::pending_channel_with_cancel(move || {
                    cancellations.fetch_add(1, Ordering::SeqCst);
                });
            *self
                .completion
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(sender);
            operation
        }
    }

    fn pending_ffi_sign_engine() -> (Arc<NmpEngine>, Arc<AtomicUsize>) {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        let cancellations = Arc::new(AtomicUsize::new(0));
        engine
            .engine
            .add_signer(PendingFfiSigner {
                public_key: keys.public_key(),
                cancellations: Arc::clone(&cancellations),
                completion: Mutex::new(None),
            })
            .unwrap();
        engine
            .engine
            .set_active_account(Some(keys.public_key()))
            .unwrap();
        (engine, cancellations)
    }

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
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let author = engine
            .add_account(format!("{:064x}", 17u8))
            .expect("account must register");
        engine
            .set_active_account(Some(author.public_key()))
            .expect("account must activate");
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
        assert_eq!(signed.pubkey, author.public_key());
        assert_eq!(signed.created_at, request.created_at);
        assert_eq!(signed.kind, request.kind);
        assert_eq!(signed.tags, request.tags);
        assert_eq!(signed.content, request.content);
        assert_eq!(signed.id.len(), 64);
        assert_eq!(signed.sig.len(), 128);
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_missing_active_signer_is_typed() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .set_active_account(Some(keys.public_key().to_hex()))
            .unwrap();
        let result = engine.sign_event(FfiSignEventRequest {
            created_at: 1,
            kind: 1,
            tags: Vec::new(),
            content: "body".to_string(),
        });
        assert_eq!(
            result.map(|_| ()).unwrap_err(),
            FfiError::NoActiveSigner,
            "missing signer must refuse synchronously"
        );
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_refuses_malformed_tags_before_admission() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let author = engine.add_account(format!("{:064x}", 31u8)).unwrap();
        engine
            .set_active_account(Some(author.public_key()))
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
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        engine.shutdown();
        let result = engine.sign_event(pending_ffi_request());
        assert_eq!(
            result.map(|_| ()).unwrap_err(),
            FfiError::EngineClosed,
            "a closed engine must refuse before operation admission"
        );
    }

    #[tokio::test]
    async fn ffi_sign_event_reports_malicious_output_without_fabricating_success() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let reported = nostr::Keys::generate().public_key();
        engine
            .engine
            .add_signer(MismatchedFfiSigner {
                reported,
                actual: nostr::Keys::generate(),
            })
            .unwrap();
        engine.engine.set_active_account(Some(reported)).unwrap();
        let handle = engine
            .sign_event(pending_ffi_request())
            .expect("operation must start");
        assert!(matches!(
            handle.signed().await.unwrap_err(),
            FfiSignEventFailure::InvalidSignerOutput { .. }
        ));
        // One-shot: the single result was already delivered to the first await.
        assert!(matches!(
            handle.signed().await.unwrap_err(),
            FfiSignEventFailure::AlreadyConsumed
        ));
        engine.shutdown();
    }

    /// The pull-based replacement for the old callback-reentrancy proof: the
    /// task that awaits `signed()` runs on its own executor (never the engine
    /// reducer thread), so it can freely re-enter engine verbs and drive
    /// `shutdown()` to completion without deadlock.
    #[tokio::test]
    async fn ffi_sign_event_completion_consumer_can_reenter_verbs_and_shutdown() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let author = engine.add_account(format!("{:064x}", 32u8)).unwrap();
        engine
            .set_active_account(Some(author.public_key()))
            .unwrap();
        let handle = engine
            .sign_event(pending_ffi_request())
            .expect("operation must start");

        let signed = handle.signed().await.expect("local signer must complete");
        assert_eq!(signed.pubkey, author.public_key());
        // Re-enter engine verbs from the awaiting consumer.
        let active = engine
            .active_account()
            .expect("callback consumer can call an engine verb")
            .expect("fixture has an active account");
        assert_eq!(active, author.public_key());
        engine.shutdown();
        assert!(matches!(
            engine.active_account(),
            Err(FfiError::EngineClosed)
        ));
    }

    #[tokio::test]
    async fn ffi_sign_event_caller_cancel_completes_once() {
        let (engine, cancellations) = pending_ffi_sign_engine();
        let handle = engine
            .sign_event(pending_ffi_request())
            .expect("operation must start");
        handle.cancel();
        assert!(matches!(
            handle.signed().await.unwrap_err(),
            FfiSignEventFailure::Cancelled
        ));
        // One-shot: a second await sees the drained result.
        assert!(matches!(
            handle.signed().await.unwrap_err(),
            FfiSignEventFailure::AlreadyConsumed
        ));
        // The cancel hook runs inside `recv_or_cancel` before the completion
        // resolves, so it has fired exactly once by the time `signed()` returns.
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    #[tokio::test]
    async fn ffi_sign_event_shutdown_completes_once() {
        let (engine, cancellations) = pending_ffi_sign_engine();
        let handle = engine
            .sign_event(pending_ffi_request())
            .expect("operation must start");
        engine.shutdown();
        assert!(matches!(
            handle.signed().await.unwrap_err(),
            FfiSignEventFailure::Cancelled
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    }

    /// #52's headline falsifier through the FFI boundary: a tampered
    /// `FfiWritePayload::Signed` is no longer a synchronous `FfiError` --
    /// `NmpEngine::publish` accepts it and the rejection surfaces on the receipt
    /// stream as `WriteStatus::Failed`, the FIRST and only status delivered.
    #[tokio::test]
    async fn ffi_tampered_signed_publish_fails_closed_on_receipt_stream() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");

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
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        };

        let receipt = engine
            .publish(intent)
            .expect("a well-formed (if tampered) Signed payload must parse at the FFI boundary");
        assert!(
            receipt.id() > 0,
            "publish must expose its stable receipt id"
        );

        match next_status(&receipt)
            .await
            .expect("a Durable intent must yield a status")
        {
            FfiWriteStatus::Failed { .. } => {}
            other => panic!("expected FfiWriteStatus::Failed, got {other:?}"),
        }
        assert!(
            next_status(&receipt).await.is_none(),
            "Failed must be the sole terminal status -- the stream then ends"
        );

        engine.shutdown();
    }

    /// #47 Unit A through the FFI boundary: an `identity_override` naming a
    /// pubkey with NO registered signer capability is accepted and PARKED as
    /// `AwaitingCapability`. It must never silently terminate: after
    /// `AwaitingCapability` the stream stays open (a timeout, never `None`).
    #[tokio::test]
    async fn ffi_override_publish_for_unregistered_pubkey_parks_awaiting_capability() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let active = nostr::Keys::generate();
        let overridden = nostr::Keys::generate();
        engine
            .set_active_account(Some(active.public_key().to_hex()))
            .expect("active account must activate");

        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: overridden.public_key().to_hex(),
                created_at: nostr::Timestamp::now().as_secs(),
                kind: 9999,
                tags: vec![],
                content: "override park".to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: Some(overridden.public_key().to_hex()),
            correlation: None,
        };

        let receipt = engine
            .publish(intent)
            .expect("a well-formed override intent must enqueue");
        assert!(
            receipt.id() > 0,
            "publish must expose its stable receipt id"
        );

        assert_eq!(
            next_status(&receipt).await,
            Some(FfiWriteStatus::Accepted),
            "must observe Accepted"
        );
        assert_eq!(
            next_status(&receipt).await,
            Some(FfiWriteStatus::AwaitingCapability {
                pubkey: overridden.public_key().to_hex()
            }),
            "the parked pubkey must be the frozen override, never the active account"
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
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .set_active_account(Some(keys.public_key().to_hex()))
            .unwrap();
        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: keys.public_key().to_hex(),
                created_at: 10,
                kind: 1,
                tags: Vec::new(),
                content: "cancel through ffi".to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        };
        let receipt = engine.publish(intent).unwrap();
        let receipt_id = receipt.id();
        assert_eq!(next_status(&receipt).await, Some(FfiWriteStatus::Accepted));

        assert_eq!(
            engine.cancel(receipt_id),
            Ok(FfiCancelWriteOutcome::Cancelled)
        );
        let mut observed = false;
        while let Some(status) = next_status(&receipt).await {
            if status == FfiWriteStatus::Cancelled {
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

    /// #156 account-switch falsifier through the public native boundary.
    /// Composition snapshots A, but switching to B is serialized ahead of
    /// publish. Acceptance must reject the stale A draft before `Accepted` or
    /// any canonical/durable residue.
    #[tokio::test]
    async fn ffi_group_message_composed_as_a_cannot_publish_after_switching_to_b() {
        use redb::{ReadableDatabase, ReadableTableMetadata};

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("stale-account.redb");
        let engine = NmpEngine::new(NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        })
        .expect("engine must build");
        let a = engine
            .add_account(format!("{:064x}", 1u8))
            .expect("A must register through the public FFI surface");
        let b = engine
            .add_account(format!("{:064x}", 2u8))
            .expect("B must register through the public FFI surface");

        engine
            .set_active_account(Some(a.public_key()))
            .expect("A must activate");
        let intent = engine
            .group_message_intent(
                "wss://group-host.example.com".to_string(),
                "group-a".to_string(),
                "stale A message".to_string(),
                vec![],
                None,
            )
            .expect("composition as active A must succeed");
        engine
            .set_active_account(Some(b.public_key()))
            .expect("B must activate before publish");

        let receipt = engine
            .publish_composed(intent)
            .expect("pre-acceptance failure still has a stream-local correlation id");
        let receipt_id = receipt.id();
        match next_status(&receipt)
            .await
            .expect("stale author must fail deterministically")
        {
            FfiWriteStatus::Failed { reason } => assert_eq!(
                reason,
                "unsigned draft author does not match current active account"
            ),
            other => panic!("Failed must be first, before Accepted; got {other:?}"),
        }
        assert!(
            next_status(&receipt).await.is_none(),
            "Failed must be the sole receipt fact"
        );
        // Reuse the protocol selection but ask the empty configured Public
        // lane, so this is a canonical-cache assertion with no relay dial.
        let mut cache_probe = nmp_nip29::group_content_demand(
            nostr::RelayUrl::parse("wss://group-host.example.com").unwrap(),
            "group-a",
        );
        cache_probe.source = nmp::SourceAuthority::Public;
        let subscription = engine
            .engine
            .observe(nmp::LiveQuery(cache_probe), None)
            .expect("canonical query must open");
        let frame = subscription
            .recv_timeout(Duration::from_secs(5))
            .expect("canonical query must deliver its current empty snapshot");
        assert!(
            !frame
                .deltas
                .iter()
                .any(|delta| matches!(delta, nmp::RowDelta::Added(_))),
            "a pre-acceptance rejection must create no canonical row"
        );
        drop(subscription);

        let outcome = engine
            .reattach_receipt(receipt_id)
            .expect("reattach lookup must succeed");
        assert!(
            matches!(outcome, FfiReceiptReattachment::NotFound),
            "a pre-acceptance rejection retains no durable receipt"
        );
        engine.shutdown();

        let db = redb::Database::open(&path).expect("reopen store for residue audit");
        let read = db.begin_read().expect("begin residue audit");
        for table_name in ["outbox_intents", "outbox_receipts", "outbox_meta"] {
            let definition: redb::TableDefinition<&str, &str> =
                redb::TableDefinition::new(table_name);
            let table = read.open_table(definition).expect("open outbox table");
            assert_eq!(
                table.len().expect("count outbox rows"),
                0,
                "pre-acceptance rejection left journal residue in {table_name}"
            );
        }
    }

    #[test]
    fn active_account_projects_the_rust_authority_and_closed_state() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let pubkey = nostr::Keys::generate().public_key().to_hex();

        assert_eq!(
            engine.active_account().expect("engine is open"),
            None,
            "a new engine must remain read-only"
        );
        engine
            .set_active_account(Some(pubkey.clone()))
            .expect("account must activate");
        assert_eq!(
            engine.active_account().expect("engine is open"),
            Some(pubkey)
        );

        engine.shutdown();
        assert!(matches!(
            engine.active_account(),
            Err(FfiError::EngineClosed)
        ));
    }

    /// #99 end-to-end reattach: a real durable intent (no signer ever attaches,
    /// so it settles into a retained `Accepted`+`AwaitingCapability` steady
    /// state) is reattached through a SECOND, independent stream that replays the
    /// identical durable `WriteStatus` prefix.
    #[tokio::test]
    async fn ffi_reattach_replays_real_receipt_facts_through_a_fresh_stream() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .set_active_account(Some(keys.public_key().to_hex()))
            .expect("account must activate");

        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: keys.public_key().to_hex(),
                created_at: nostr::Timestamp::now().as_secs(),
                kind: 9999,
                tags: vec![],
                content: "reattach e2e".to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        };

        let receipt = engine
            .publish(intent)
            .expect("a well-formed unsigned intent must enqueue");
        let receipt_id = receipt.id();
        assert!(receipt_id > 0, "publish must expose its stable receipt id");

        // Block for the exact retained steady state on the ORIGINAL stream first.
        assert_eq!(
            next_status(&receipt).await,
            Some(FfiWriteStatus::Accepted),
            "must observe Accepted"
        );
        assert_eq!(
            next_status(&receipt).await,
            Some(FfiWriteStatus::AwaitingCapability {
                pubkey: keys.public_key().to_hex()
            })
        );

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
            Some(FfiWriteStatus::Accepted),
            "replay must deliver Accepted"
        );
        assert_eq!(
            next_status(&replay).await,
            Some(FfiWriteStatus::AwaitingCapability {
                pubkey: keys.public_key().to_hex()
            })
        );

        engine.shutdown();
    }

    /// #99: an unknown receipt id reattaches to `NotFound` (no stream, no facts).
    #[test]
    fn ffi_reattach_of_unknown_id_is_not_found() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
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
            let engine = NmpEngine::new(NmpEngineConfig {
                store_path: Some(path.to_string_lossy().into_owned()),
                ..NmpEngineConfig::default()
            })
            .expect("engine must build");
            let keys = nostr::Keys::generate();
            engine
                .set_active_account(Some(keys.public_key().to_hex()))
                .expect("account must activate");
            let intent = FfiWriteIntent {
                payload: FfiWritePayload::Unsigned {
                    pubkey: keys.public_key().to_hex(),
                    created_at: nostr::Timestamp::now().as_secs(),
                    kind: 9999,
                    tags: vec![],
                    content: "corrupt-receipt".to_string(),
                },
                durability: FfiDurability::Durable,
                routing: FfiWriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            };
            let receipt = engine
                .publish(intent)
                .expect("a well-formed unsigned intent must enqueue");
            let receipt_id = receipt.id();
            assert_eq!(
                next_status(&receipt).await,
                Some(FfiWriteStatus::Accepted),
                "must observe Accepted"
            );
            engine.shutdown();
            receipt_id
        };

        // Overwrite the receipt's own durable row with undecodable bytes.
        const RECEIPTS: redb::TableDefinition<&str, &str> =
            redb::TableDefinition::new("outbox_receipts");
        let db = redb::Database::open(&path).expect("redb: reopen for corruption");
        let tx = db.begin_write().expect("redb: begin_write");
        {
            let mut table = tx.open_table(RECEIPTS).expect("redb: open outbox_receipts");
            table
                .insert(format!("{receipt_id:020}").as_str(), "{")
                .expect("redb: write corrupt receipt bytes");
        }
        tx.commit().expect("redb: commit corruption");
        drop(db);

        let engine = NmpEngine::new(NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        })
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
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");

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
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .engine
            .set_active_account(Some(keys.public_key()))
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
                .publish_tracked(nmp::WriteIntent {
                    payload: nmp::WritePayload::Unsigned(unsigned),
                    durability: nmp::Durability::Durable,
                    routing: nmp::WriteRouting::AuthorOutbox,
                    identity_override: None,
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
    /// in `None` when its `WriteStatus` sender is dropped (here via a tampered
    /// `Signed` payload whose `Failed` is the sole terminal), after real delivery.
    #[tokio::test]
    async fn ffi_receipt_stream_ends_with_none_when_sender_dropped() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");

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
                content: "tampered".to_string(),
                sig: event.sig.to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        };

        let receipt = engine
            .publish(intent)
            .expect("a well-formed (if tampered) Signed payload must parse at the FFI boundary");

        // The stream is genuinely active first (the terminal fact arrives).
        match next_status(&receipt)
            .await
            .expect("a Durable intent must yield a status")
        {
            FfiWriteStatus::Failed { .. } => {}
            other => panic!("expected FfiWriteStatus::Failed, got {other:?}"),
        }

        assert!(
            next_status(&receipt).await.is_none(),
            "the receipt stream must end in None once the sender is dropped, not hang"
        );

        engine.shutdown();
    }

    /// #156: `publish_composed` takes its `FfiComposedWriteIntent` exactly once.
    /// A second call on the identical `Arc<FfiComposedWriteIntent>` must fail
    /// closed with `FfiError::IntentAlreadyConsumed`.
    #[tokio::test]
    async fn ffi_publish_composed_takes_the_intent_exactly_once() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .set_active_account(Some(keys.public_key().to_hex()))
            .expect("account must activate");

        let intent = engine
            .group_message_intent(
                "wss://group-host.example.com".to_string(),
                "group-a".to_string(),
                "hi".to_string(),
                vec![],
                None,
            )
            .expect("a well-formed group message must compose");

        let receipt = engine
            .publish_composed(intent.clone())
            .expect("the first publish_composed call must consume the intent and succeed");
        assert!(
            receipt.id() > 0,
            "publish_composed must expose a receipt id"
        );
        assert!(matches!(
            next_status(&receipt).await,
            Some(FfiWriteStatus::Accepted)
        ));

        match engine.publish_composed(intent) {
            Err(FfiError::IntentAlreadyConsumed) => {}
            Err(other) => {
                panic!("expected FfiError::IntentAlreadyConsumed on the second call, got {other:?}")
            }
            Ok(_) => {
                panic!("a second publish_composed must not re-publish the consumed intent")
            }
        }

        engine.shutdown();
    }
}
