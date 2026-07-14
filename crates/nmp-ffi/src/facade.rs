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
//! `nmp-ffi` is now only: config/type mirroring across the FFI boundary, and
//! the drain-thread bridge from `nmp`'s blocking `recv()` verbs to UniFFI's
//! callback-interface observers (`convert`/`observer`).
//!
//! Directory: `nmp_router::LiveDirectory` (M5's self-bootstrapping outbox,
//! now assembled inside `nmp::Engine::new`) is what backs every `NmpEngine`
//! -- a Swift app supplies ONLY the operator indexer relay set; every
//! author's NIP-65 write relays (including the app's own account) are
//! discovered by the engine itself, live, via its own internal kind:10002
//! reads against those same indexers (`nmp_engine::core::EngineCore`'s
//! auto-discovery). `NmpEngineConfig` no longer accepts a pre-resolved
//! write-relay map -- there is nothing for a caller to resolve up front.

use std::sync::Arc;
#[cfg(test)]
use std::thread;

use crate::convert::{
    demand_from_ffi, diagnostics_snapshot_to_ffi, evidence_to_ffi, filter_from_ffi,
    history_batch_to_ffi, history_query_from_ffi, parse_pubkey, row_delta_to_ffi,
    sign_event_failure, sign_event_request_from_ffi, sign_event_start_error, signed_event_to_ffi,
    write_intent_from_ffi, write_status_to_ffi, FfiError, FfiHistoryLoadError, WriteStatusRef,
};
use crate::nip02::{
    action_status_to_ffi, handle as follow_handle, snapshot_to_ffi, FollowActionObserver,
    FollowObserver, NmpFollowHandle,
};
use crate::observer::{
    DiagnosticsObserver, HistoryObserver, ReceiptObserver, RowObserver, SignEventObserver,
};
use crate::types::{
    FfiDemand, FfiFilter, FfiHistoryQuery, FfiReceiptReattachment, FfiRelayInformation,
    FfiRelayInformationCachePolicy, FfiRelayInformationDocument, FfiRelayInformationFreshness,
    FfiRelayInformationLimitations, FfiSignEventRequest, FfiWriteIntent, NmpHistoryContinuation,
};
use nmp::ReceiptReattachment;

fn spawn_native_bridge(
    reservation: nmp::NativeTaskReservation,
    cancel: nmp::NativeTaskCancel,
    component: &'static str,
    task: impl FnOnce() + Send + 'static,
) -> Result<(), FfiError> {
    start_native_bridge(reservation, cancel, component).map(|starter| starter.run(task))
}

fn start_native_bridge(
    reservation: nmp::NativeTaskReservation,
    cancel: nmp::NativeTaskCancel,
    component: &'static str,
) -> Result<nmp::StartedNativeTask, FfiError> {
    reservation
        .start_with_cancel(move || cancel.cancel())
        .map_err(|error| FfiError::ThreadUnavailable {
            component: component.to_string(),
            reason: error.to_string(),
        })
}

#[cfg(test)]
fn spawn_native_bridge_with(
    component: &'static str,
    task: Box<dyn FnOnce() + Send + 'static>,
    spawn: impl FnOnce(
        thread::Builder,
        Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<thread::JoinHandle<()>>,
) -> Result<(), FfiError> {
    spawn(
        thread::Builder::new().name(format!("nmp-ffi-{component}")),
        task,
    )
    .map(|_| ())
    .map_err(|error| FfiError::ThreadUnavailable {
        component: component.to_string(),
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod thread_spawn_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct RecordingFollowActionObserver {
        statuses: Arc<Mutex<Vec<crate::nip02::FfiFollowActionStatus>>>,
        closes: Arc<AtomicUsize>,
    }

    impl FollowActionObserver for RecordingFollowActionObserver {
        fn on_status(&self, status: crate::nip02::FfiFollowActionStatus) {
            self.statuses.lock().unwrap().push(status);
        }

        fn on_closed(&self) {
            self.closes.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn injected_native_bridge_refusal_is_typed_and_preserves_safe_reason() {
        let error = spawn_native_bridge_with(
            "row-observer",
            Box::new(|| panic!("refused task must never run")),
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "injected thread pressure",
                ))
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            FfiError::ThreadUnavailable {
                component: "row-observer".to_string(),
                reason: "injected thread pressure".to_string(),
            }
        );
    }

    #[test]
    fn follow_bridge_refusal_reports_once_before_the_action_can_start() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let start_flag = Arc::clone(&started);
        start_following_action_with(
            engine,
            nostr::Keys::generate().public_key().to_hex(),
            nmp_nip02::FollowChange::Follow,
            Box::new(RecordingFollowActionObserver {
                statuses: Arc::clone(&statuses),
                closes: Arc::clone(&closes),
            }),
            |task| {
                drop(task);
                Err(FfiError::ThreadUnavailable {
                    component: "follow-action-observer".to_string(),
                    reason: "injected bridge pressure".to_string(),
                })
            },
            move |_| {
                start_flag.store(true, Ordering::Release);
            },
        );
        assert!(!started.load(Ordering::Acquire));
        assert_eq!(closes.load(Ordering::Acquire), 1);
        assert_eq!(
            *statuses.lock().unwrap(),
            vec![crate::nip02::FfiFollowActionStatus::Failed {
                failure: crate::nip02::FfiFollowActionFailure::ThreadUnavailable {
                    component: "follow-action-observer".to_string(),
                    reason: "injected bridge pressure".to_string(),
                },
            }]
        );
    }
}

fn reattachment_to_ffi(value: &ReceiptReattachment) -> FfiReceiptReattachment {
    match value {
        ReceiptReattachment::Attached(_) => FfiReceiptReattachment::Attached,
        ReceiptReattachment::NotFound => FfiReceiptReattachment::NotFound,
        ReceiptReattachment::RetainedButUnreadable => FfiReceiptReattachment::RetainedButUnreadable,
    }
}

fn start_following_action(
    engine: Arc<nmp::Engine>,
    target: String,
    change: nmp_nip02::FollowChange,
    observer: Box<dyn FollowActionObserver>,
) {
    let bridge_engine = Arc::clone(&engine);
    start_following_action_with(
        engine,
        target,
        change,
        observer,
        move |task| {
            let reservation = bridge_engine.reserve_native_task("follow-action-observer")?;
            let cancel = bridge_engine.native_task_cancel()?;
            spawn_native_bridge(reservation, cancel, "follow-action-observer", task)
        },
        nmp_nip02::FollowActionRunner::start,
    );
}

fn start_following_action_with(
    engine: Arc<nmp::Engine>,
    target: String,
    change: nmp_nip02::FollowChange,
    observer: Box<dyn FollowActionObserver>,
    spawn_bridge: impl FnOnce(Box<dyn FnOnce() + Send + 'static>) -> Result<(), FfiError>,
    start_runner: impl FnOnce(nmp_nip02::FollowActionRunner),
) {
    let target = match parse_pubkey(&target) {
        Ok(target) => target,
        Err(_) => {
            observer.on_status(crate::nip02::FfiFollowActionStatus::Failed {
                failure: crate::nip02::FfiFollowActionFailure::InvalidTarget { got: target },
            });
            observer.on_closed();
            return;
        }
    };
    let (action, runner) = nmp_nip02::prepare_set_following(engine, target, change);
    let observer: Arc<dyn FollowActionObserver> = Arc::from(observer);
    let bridge = Arc::clone(&observer);
    match spawn_bridge(Box::new(move || {
        while let Ok(status) = action.recv() {
            bridge.on_status(action_status_to_ffi(status));
        }
        bridge.on_closed();
    })) {
        Ok(()) => start_runner(runner),
        Err(error) => {
            let failure = match error {
                FfiError::ThreadUnavailable { component, reason } => {
                    crate::nip02::FfiFollowActionFailure::ThreadUnavailable { component, reason }
                }
                FfiError::ExecutorSaturated {
                    component,
                    capacity,
                } => crate::nip02::FfiFollowActionFailure::ExecutorSaturated {
                    component,
                    capacity,
                },
                other => crate::nip02::FfiFollowActionFailure::ThreadUnavailable {
                    component: "follow-action-observer".to_string(),
                    reason: format!("unexpected bridge refusal: {other:?}"),
                },
            };
            observer.on_status(crate::nip02::FfiFollowActionStatus::Failed { failure });
            observer.on_closed();
        }
    }
}

/// Construction config for [`NmpEngine::new`]. See the module doc: the only
/// relay facts a caller ever supplies are the three operator-configured
/// lanes -- `indexer_relays`, `app_relays`, `fallback_relays`
/// (`routing-and-ownership.md` §2.1) -- everything else is discovered live.
#[derive(uniffi::Record, Clone, Debug, Default)]
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
    /// Maximum immediately-running native observer/action/waiter tasks.
    /// Zero selects the finite default; saturation never queues an accepted
    /// stream behind a long-lived drain.
    #[uniffi(default = 12)]
    pub max_native_tasks: u32,
}

/// The default relay-count ceiling for a freshly-constructed engine config
/// (#20). Update BOTH this const AND the `#[uniffi(default = N)]` literal
/// on [`NmpEngineConfig::max_relays`] above — they must match.
pub const DEFAULT_MAX_RELAYS: u32 = 10;
pub const DEFAULT_MAX_NATIVE_TASKS: u32 = 12;

/// Destructively reset a closed persistent NMP store. This removes all
/// canonical engine state at `store_path`, while leaving any separately
/// configured native account checkpoint untouched. The operation is
/// idempotent when the store does not exist.
#[uniffi::export]
pub fn reset_persistent_store(store_path: String) -> Result<(), FfiError> {
    nmp::Engine::reset_persistent_store(store_path)?;
    Ok(())
}

// Compile-time guard that the Rust `Default` derive for `NmpEngineConfig`
// Keep the native-facing literal pinned to the canonical finite default.
const _: () = assert!(DEFAULT_MAX_RELAYS == 10);
const _: () = assert!(DEFAULT_MAX_NATIVE_TASKS == 12);

/// Exact, lock-protected executor accounting for deterministic lifecycle and
/// saturation falsifiers. `admitted` includes a reservation during its brief
/// pre-handoff window; `running` counts OS task handles not yet joined.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiNativeTaskCensus {
    pub capacity: u64,
    pub admitted: u64,
    pub running: u64,
    pub accepting: bool,
}

impl From<NmpEngineConfig> for nmp::EngineConfig {
    fn from(config: NmpEngineConfig) -> Self {
        nmp::EngineConfig {
            store_path: config.store_path,
            indexer_relays: config.indexer_relays,
            app_relays: config.app_relays,
            fallback_relays: config.fallback_relays,
            allowed_local_relay_hosts: config.allowed_local_relay_hosts,
            max_relays: config.max_relays as usize,
            max_native_tasks: config.max_native_tasks as usize,
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
            last_error: value.last_error.map(|error| error.to_string()),
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
    /// [`Self::set_active_account`] for that. Returns the account's hex
    /// public key.
    pub fn add_account(&self, secret_key: String) -> Result<String, FfiError> {
        let pk = self.engine.add_account(&secret_key)?;
        Ok(pk.to_hex())
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
    /// returned handle cancels only this signer operation; completion fires
    /// exactly once through `observer`.
    pub fn sign_event(
        &self,
        event: FfiSignEventRequest,
        observer: Box<dyn SignEventObserver>,
    ) -> Result<Arc<NmpSignEventHandle>, FfiError> {
        let request = sign_event_request_from_ffi(event)?;
        let cancel = self
            .engine
            .sign_event_with_completion(request, move |result| match result {
                Ok(event) => observer.on_signed(signed_event_to_ffi(event)),
                Err(error) => observer.on_failed(sign_event_failure(error)),
            })
            .map_err(sign_event_start_error)?;
        Ok(Arc::new(NmpSignEventHandle { cancel }))
    }

    pub fn native_task_census(&self) -> FfiNativeTaskCensus {
        let census = self.engine.native_task_census();
        FfiNativeTaskCensus {
            capacity: census.capacity as u64,
            admitted: census.admitted as u64,
            running: census.running as u64,
            accepting: census.accepting,
        }
    }

    /// Event-driven barrier for lifecycle tests and hosts that require proof
    /// all cancelled native callbacks have returned before releasing state.
    pub fn await_native_tasks_idle(&self) {
        self.engine.wait_for_native_tasks_idle();
    }

    /// Observe the active account's relationship to `target` through the
    /// NMP-owned NIP-02 resource. The returned handle only owns demand
    /// cancellation; contact-list semantics and acquisition state stay in
    /// Rust and arrive as closed snapshots.
    pub fn observe_following(
        &self,
        target: String,
        observer: Box<dyn FollowObserver>,
    ) -> Result<Arc<NmpFollowHandle>, FfiError> {
        let target = parse_pubkey(&target)?;
        let reservation = self.engine.reserve_native_task("follow-observer")?;
        let task_cancel = self.engine.native_task_cancel()?;
        let starter = start_native_bridge(reservation, task_cancel, "follow-observer")?;
        let observation = nmp_nip02::observe_following(self.engine.clone(), target)?;
        let cancel = observation.cancel_handle();
        starter.run(move || {
            while let Some(snapshot) = observation.recv() {
                observer.on_snapshot(snapshot_to_ffi(snapshot));
            }
            observer.on_closed();
        });
        Ok(follow_handle(cancel))
    }

    /// Ask NMP to follow `target`. This is the complete NIP-02 action: it
    /// waits for the module's source-evidence policy, preserves the exact
    /// kind:3 base, atomically guards that base, signs, routes, and streams
    /// the durable receipt. The native button owns none of those steps.
    pub fn follow(&self, target: String, observer: Box<dyn FollowActionObserver>) {
        start_following_action(
            self.engine.clone(),
            target,
            nmp_nip02::FollowChange::Follow,
            observer,
        );
    }

    /// The inverse of [`Self::follow`], with the same acquisition,
    /// compare-and-swap, signer, routing, and receipt guarantees.
    pub fn unfollow(&self, target: String, observer: Box<dyn FollowActionObserver>) {
        start_following_action(
            self.engine.clone(),
            target,
            nmp_nip02::FollowChange::Unfollow,
            observer,
        );
    }

    /// Open a live subscription. `observer` is driven from a dedicated drain
    /// thread (M4 plan §4b) -- never the engine thread itself. The returned
    /// [`NmpQueryHandle`]'s `Drop` withdraws the subscription (deinit-tied
    /// demand drop, plan §4c); call [`NmpQueryHandle::cancel`] for an
    /// explicit early teardown instead of waiting on Swift's own `deinit`
    /// timing.
    pub fn observe(
        &self,
        query: FfiFilter,
        observer: Box<dyn RowObserver>,
    ) -> Result<Arc<NmpQueryHandle>, FfiError> {
        let filter = filter_from_ffi(query)?;
        let reservation = self.engine.reserve_native_task("row-observer")?;
        let task_cancel = self.engine.native_task_cancel()?;
        let starter = start_native_bridge(reservation, task_cancel, "row-observer")?;
        let subscription = self.engine.observe(nmp::LiveQuery::from_filter(filter))?;
        let cancel = subscription.cancel_handle();

        starter.run(move || {
            while let Ok((deltas, evidence)) = subscription.recv() {
                let ffi_deltas = deltas.iter().map(row_delta_to_ffi).collect();
                observer.on_batch(ffi_deltas, evidence_to_ffi(evidence));
            }
            observer.on_closed();
        });

        Ok(Arc::new(NmpQueryHandle { cancel }))
    }

    /// Open a live subscription over an explicit [`FfiDemand`] (#107) --
    /// the constructor an app reaches for once [`Self::observe`]'s bare
    /// `FfiFilter` (which always takes `Demand::from_filter`'s static
    /// default) isn't enough: declaring `Pinned` wire authority, a non-
    /// default `AccessContext`, or a non-`Agnostic` `CacheMode`. Same
    /// drain-thread/cancel-handle shape as `observe` in every other respect.
    pub fn observe_demand(
        &self,
        query: FfiDemand,
        observer: Box<dyn RowObserver>,
    ) -> Result<Arc<NmpQueryHandle>, FfiError> {
        let demand = demand_from_ffi(query)?;
        let reservation = self.engine.reserve_native_task("demand-observer")?;
        let task_cancel = self.engine.native_task_cancel()?;
        let starter = start_native_bridge(reservation, task_cancel, "demand-observer")?;
        let subscription = self.engine.observe(nmp::LiveQuery(demand))?;
        let cancel = subscription.cancel_handle();

        starter.run(move || {
            while let Ok((deltas, evidence)) = subscription.recv() {
                let ffi_deltas = deltas.iter().map(row_delta_to_ffi).collect();
                observer.on_batch(ffi_deltas, evidence_to_ffi(evidence));
            }
            observer.on_closed();
        });

        Ok(Arc::new(NmpQueryHandle { cancel }))
    }

    /// Open one coordinated bounded-history session. The drain task owns the
    /// blocking receiver exclusively; the returned handle owns only the
    /// opaque advance/cancel capability for that same session. Every older
    /// request must return a continuation issued in the latest callback.
    pub fn observe_history(
        &self,
        query: FfiHistoryQuery,
        observer: Box<dyn HistoryObserver>,
    ) -> Result<Arc<NmpHistoryHandle>, FfiError> {
        let query = history_query_from_ffi(query)?;
        let reservation = self.engine.reserve_native_task("history-observer")?;
        let task_cancel = self.engine.native_task_cancel()?;
        let starter = start_native_bridge(reservation, task_cancel, "history-observer")?;
        let subscription = self.engine.observe_history(query)?;
        let advance = subscription.advance_handle();

        starter.run(move || {
            while let Ok(batch) = subscription.recv() {
                observer.on_batch(history_batch_to_ffi(batch));
            }
            observer.on_closed();
        });

        Ok(Arc::new(NmpHistoryHandle { advance }))
    }

    /// Enqueue a write. `observer` streams every `WriteStatus` this intent
    /// ever reaches (ledger #9 -- enqueue is not converged; the first value
    /// is never a terminal for a durable/at-most-once intent). A
    /// caller-supplied `Signed` payload that fails verification is no
    /// longer a synchronous error here (that guarantee moved to
    /// `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary,
    /// Unit A0/#56, so it holds for every entry point, not only this one) --
    /// it surfaces as `WriteStatus::Failed`, the FIRST and only status
    /// `observer` receives, with no preceding `Accepted`.
    /// Exhaustion of the pre-acceptance correlation namespace instead returns
    /// a typed `FfiError` synchronously: no receipt id or stream exists.
    pub fn publish(
        &self,
        intent: FfiWriteIntent,
        observer: Box<dyn ReceiptObserver>,
    ) -> Result<u64, FfiError> {
        let write_intent = write_intent_from_ffi(intent)?;
        let reservation = self.engine.reserve_native_task("receipt-observer")?;
        let task_cancel = self.engine.native_task_cancel()?;
        let starter = start_native_bridge(reservation, task_cancel, "receipt-observer")?;
        let receipt = self.engine.publish_tracked(write_intent)?;
        let receipt_id = receipt.id.0;
        let receipt_rx = receipt.statuses;

        starter.run(move || {
            while let Ok(status) = receipt_rx.recv() {
                observer.on_status(write_status_to_ffi(WriteStatusRef(&status)));
            }
            observer.on_closed();
        });

        Ok(receipt_id)
    }

    /// Compose an ordinary kind:9 NIP-29 message from semantic inputs
    /// (#156). The caller supplies no author, timestamp, kind, bech32
    /// encoding, or raw tags: NMP reads the active account, owns event time,
    /// materializes ordered/deduplicated `nostr:npub…` content references,
    /// and composes `p`/reply-`e`/`h`/`previous` plus pinned-host routing.
    /// `previous` comes from an engine-owned strict-cache snapshot for this
    /// exact host/group; no caller row or provenance claim enters the path.
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

    /// Publish a `nmp_nip29::compose_group_send`-composed intent (#115).
    /// Take-once: `intent` is consumed by this call (`FfiComposedWriteIntent
    /// ::take`) -- a second call on the SAME handle fails closed with
    /// `FfiError::IntentAlreadyConsumed` rather than silently re-publishing
    /// a stale template. Otherwise identical to [`Self::publish`]'s body
    /// (same receipt-stream drain-thread bridge); `write_intent_from_ffi`
    /// never runs for this path -- the intent was already composed
    /// directly, never round-tripped through the raw `FfiWriteRouting`
    /// conversion (which withholds `PinnedHost` entirely).
    pub fn publish_composed(
        &self,
        intent: Arc<crate::nip29::FfiComposedWriteIntent>,
        observer: Box<dyn ReceiptObserver>,
    ) -> Result<u64, FfiError> {
        let reservation = self
            .engine
            .reserve_native_task("composed-receipt-observer")?;
        let task_cancel = self.engine.native_task_cancel()?;
        let starter = start_native_bridge(reservation, task_cancel, "composed-receipt-observer")?;
        let write_intent = intent.take()?;
        let receipt = self.engine.publish_tracked(write_intent)?;
        let receipt_id = receipt.id.0;
        let receipt_rx = receipt.statuses;

        starter.run(move || {
            while let Ok(status) = receipt_rx.recv() {
                observer.on_status(write_status_to_ffi(WriteStatusRef(&status)));
            }
            observer.on_closed();
        });

        Ok(receipt_id)
    }

    /// Attach to a retained receipt without collapsing corrupt durable
    /// evidence into the same result as an unknown id.
    pub fn reattach_receipt(
        &self,
        receipt_id: u64,
        observer: Box<dyn ReceiptObserver>,
    ) -> Result<FfiReceiptReattachment, FfiError> {
        let reservation = self
            .engine
            .reserve_native_task("reattached-receipt-observer")?;
        let task_cancel = self.engine.native_task_cancel()?;
        let starter = start_native_bridge(reservation, task_cancel, "reattached-receipt-observer")?;
        let result = self.engine.reattach_receipt(nmp::ReceiptId(receipt_id))?;
        let ffi_result = reattachment_to_ffi(&result);
        match result {
            ReceiptReattachment::Attached(receipt_rx) => {
                starter.run(move || {
                    while let Ok(status) = receipt_rx.recv() {
                        observer.on_status(write_status_to_ffi(WriteStatusRef(&status)));
                    }
                    observer.on_closed();
                });
            }
            ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => {
                drop(starter);
            }
        }
        Ok(ffi_result)
    }

    /// Open a live diagnostics stream (M5 plan §1.2 step 5) -- "the
    /// acceptance test rendered on screen, permanently." `observer` is
    /// driven from a dedicated drain thread, mirroring [`Self::observe`];
    /// the returned [`NmpDiagnosticsHandle`]'s `Drop` withdraws the
    /// observer (deinit-tied teardown, same discipline as
    /// [`NmpQueryHandle`]). Delivers the CURRENT snapshot immediately, then
    /// a fresh one on every recompile/EOSE-driven coverage change --
    /// pushed reactively, never polled.
    pub fn observe_diagnostics(
        &self,
        observer: Box<dyn DiagnosticsObserver>,
    ) -> Result<Arc<NmpDiagnosticsHandle>, FfiError> {
        let reservation = self.engine.reserve_native_task("diagnostics-observer")?;
        let task_cancel = self.engine.native_task_cancel()?;
        let starter = start_native_bridge(reservation, task_cancel, "diagnostics-observer")?;
        let subscription = self.engine.observe_diagnostics()?;
        let cancel = subscription.cancel_handle();

        starter.run(move || {
            while let Some(snapshot) = subscription.recv() {
                observer.on_snapshot(diagnostics_snapshot_to_ffi(snapshot));
            }
            observer.on_closed();
        });

        Ok(Arc::new(NmpDiagnosticsHandle { cancel }))
    }

    /// Stop the engine. Idempotent: a second call is a no-op (`nmp::Engine`'s
    /// own serialized lifecycle gate, see that type's doc).
    pub fn shutdown(&self) {
        self.engine.shutdown();
    }
}

/// The app-facing handle to a live subscription (returned by
/// [`NmpEngine::observe`]). `Drop` withdraws the subscription -- the SDK
/// never requires an app-owned container or lifecycle hook to make this
/// happen (plan §7's kill test). Holds ONLY the opaque
/// [`nmp::ObservationCancel`] token (`Subscription::cancel_handle`) -- no
/// `Handle`/`QueryHandle` (the raw imperative engine-control capability)
/// ever reaches this crate. The receiving half of the subscription is owned
/// entirely by [`NmpEngine::observe`]'s drain thread, since `recv()`
/// blocks; `cancel()`/`Drop` here and the drain thread's own teardown
/// converge on the token's single withdrawal guard (see that type's doc).
#[derive(uniffi::Object)]
pub struct NmpQueryHandle {
    cancel: nmp::ObservationCancel,
}

#[uniffi::export]
impl NmpQueryHandle {
    /// Withdraw the subscription now, rather than waiting for `Drop` (a
    /// Swift `deinit` can be delayed by ARC in ways an app may want to
    /// preempt explicitly). Safe to call more than once, and safe to never
    /// call at all (in which case `Drop` is what withdraws it).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for NmpQueryHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Native owner for one bounded-history session. `Drop` and explicit
/// `cancel` share the subscription's one withdrawal guard, closing every
/// engine-owned acquisition handle opened by prior advances.
#[derive(uniffi::Object)]
pub struct NmpHistoryHandle {
    advance: nmp::HistoryAdvance,
}

#[uniffi::export]
impl NmpHistoryHandle {
    pub fn load_older(
        &self,
        continuation: Arc<NmpHistoryContinuation>,
    ) -> Result<(), FfiHistoryLoadError> {
        self.advance
            .load_older(continuation.inner.clone())
            .map_err(FfiHistoryLoadError::from)
    }

    pub fn cancel(&self) {
        self.advance.cancel();
    }
}

impl Drop for NmpHistoryHandle {
    fn drop(&mut self) {
        self.advance.cancel();
    }
}

/// The app-facing handle to a live diagnostics stream (returned by
/// [`NmpEngine::observe_diagnostics`]). Same discipline as [`NmpQueryHandle`]
/// -- holds ONLY the opaque [`nmp::ObservationCancel`] token
/// (`DiagnosticsSubscription::cancel_handle`), the SAME type
/// [`NmpQueryHandle`] holds.
#[derive(uniffi::Object)]
pub struct NmpDiagnosticsHandle {
    cancel: nmp::ObservationCancel,
}

/// Scoped cancellation handle for one sign-only operation. It owns no
/// signer registration and cannot affect accepted durable writes.
#[derive(uniffi::Object)]
pub struct NmpSignEventHandle {
    cancel: nmp::SignEventCancel,
}

#[uniffi::export]
impl NmpSignEventHandle {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for NmpSignEventHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[uniffi::export]
impl NmpDiagnosticsHandle {
    /// Withdraw this diagnostics observer now, rather than waiting for
    /// `Drop`. Safe to call more than once; safe to never call at all.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for NmpDiagnosticsHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        FfiAccessContext, FfiBinding, FfiCacheMode, FfiDemand, FfiDurability, FfiFilter,
        FfiHistoryBatch, FfiHistoryLoadFact, FfiHistoryQuery, FfiSignEventFailure,
        FfiSignEventRequest, FfiSignedEvent, FfiSourceAuthority, FfiWritePayload, FfiWriteRouting,
        FfiWriteStatus,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

    struct CensusDiagnosticsObserver {
        closes: Arc<AtomicUsize>,
    }

    struct RecordingHistoryObserver {
        batches: mpsc::Sender<FfiHistoryBatch>,
        closes: Arc<AtomicUsize>,
    }

    impl HistoryObserver for RecordingHistoryObserver {
        fn on_batch(&self, batch: FfiHistoryBatch) {
            let _ = self.batches.send(batch);
        }

        fn on_closed(&self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn ffi_history_query(author: String, page_size: u64, max_rows: u64) -> FfiHistoryQuery {
        FfiHistoryQuery {
            demand: FfiDemand {
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
            },
            page_size,
            max_rows,
        }
    }

    fn recv_history_fact(
        batches: &mpsc::Receiver<FfiHistoryBatch>,
        wanted: impl Fn(FfiHistoryLoadFact) -> bool,
    ) -> FfiHistoryBatch {
        loop {
            let batch = batches
                .recv_timeout(Duration::from_secs(5))
                .expect("history callback must arrive within the lifecycle bound");
            if wanted(batch.load) {
                return batch;
            }
        }
    }

    #[test]
    fn ffi_history_projects_exact_batches_misuse_errors_and_drop_lifecycle() {
        use nmp_store::EventStore;

        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("ffi-history.redb");
        let keys = nostr::Keys::generate();
        let relay = nostr::RelayUrl::parse("wss://ffi-history.example").unwrap();
        {
            let mut store = nmp_store::RedbStore::open(&path).unwrap();
            for index in 0..3 {
                let event = nostr::UnsignedEvent::new(
                    keys.public_key(),
                    nostr::Timestamp::from(100),
                    nostr::Kind::Custom(7_778),
                    Vec::new(),
                    format!("ffi-history-{index}"),
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
            max_native_tasks: 4,
            ..NmpEngineConfig::default()
        })
        .unwrap();
        let query = ffi_history_query(keys.public_key().to_hex(), 1, 2);
        let (batch_tx, batch_rx) = mpsc::channel();
        let closes = Arc::new(AtomicUsize::new(0));
        let handle = engine
            .observe_history(
                query.clone(),
                Box::new(RecordingHistoryObserver {
                    batches: batch_tx,
                    closes: Arc::clone(&closes),
                }),
            )
            .unwrap();
        let first = recv_history_fact(&batch_rx, |fact| fact == FfiHistoryLoadFact::Idle);
        assert_eq!(first.deltas.len(), 1);
        let first_continuation = first.continuation.unwrap();

        handle.load_older(Arc::clone(&first_continuation)).unwrap();
        let second = recv_history_fact(&batch_rx, |fact| {
            fact == FfiHistoryLoadFact::Returned { added: 1 }
        });
        assert_eq!(second.deltas.len(), 1);
        let second_continuation = second.continuation.unwrap();
        assert_eq!(
            handle
                .load_older(Arc::clone(&first_continuation))
                .unwrap_err(),
            FfiHistoryLoadError::StaleGeneration
        );

        let (other_tx, _other_rx) = mpsc::channel();
        let other_closes = Arc::new(AtomicUsize::new(0));
        let other_session = engine
            .observe_history(
                query.clone(),
                Box::new(RecordingHistoryObserver {
                    batches: other_tx,
                    closes: Arc::clone(&other_closes),
                }),
            )
            .unwrap();
        assert_eq!(
            other_session
                .load_older(Arc::clone(&second_continuation))
                .unwrap_err(),
            FfiHistoryLoadError::WrongSession
        );
        let refreshed_original =
            recv_history_fact(&batch_rx, |fact| fact == FfiHistoryLoadFact::Idle);
        let current_original_continuation = refreshed_original
            .continuation
            .expect("session refresh retains its exact boundary");

        let other_engine = NmpEngine::new(NmpEngineConfig::default()).unwrap();
        let (wrong_tx, _wrong_rx) = mpsc::channel();
        let wrong_closes = Arc::new(AtomicUsize::new(0));
        let wrong_engine_session = other_engine
            .observe_history(
                query,
                Box::new(RecordingHistoryObserver {
                    batches: wrong_tx,
                    closes: Arc::clone(&wrong_closes),
                }),
            )
            .unwrap();
        assert_eq!(
            wrong_engine_session
                .load_older(Arc::clone(&second_continuation))
                .unwrap_err(),
            FfiHistoryLoadError::WrongEngine
        );

        assert_eq!(
            handle
                .load_older(current_original_continuation)
                .unwrap_err(),
            FfiHistoryLoadError::AtBound { max_rows: 2 }
        );
        let at_bound = recv_history_fact(&batch_rx, |fact| {
            fact == FfiHistoryLoadFact::AtBound { max_rows: 2 }
        });
        assert!(at_bound.continuation.is_some());

        drop(handle);
        drop(other_session);
        engine.await_native_tasks_idle();
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(other_closes.load(Ordering::SeqCst), 1);

        drop(wrong_engine_session);
        other_engine.await_native_tasks_idle();
        assert_eq!(wrong_closes.load(Ordering::SeqCst), 1);
        engine.shutdown();
        other_engine.shutdown();
    }

    impl DiagnosticsObserver for CensusDiagnosticsObserver {
        fn on_snapshot(&self, _snapshot: crate::types::FfiDiagnosticsSnapshot) {}

        fn on_closed(&self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct CensusRowObserver;

    impl RowObserver for CensusRowObserver {
        fn on_batch(
            &self,
            _deltas: Vec<crate::types::FfiRowDelta>,
            _evidence: crate::types::FfiAcquisitionEvidence,
        ) {
        }

        fn on_closed(&self) {}
    }

    struct CensusFollowObserver;

    impl crate::nip02::FollowObserver for CensusFollowObserver {
        fn on_snapshot(&self, _snapshot: crate::nip02::FfiFollowSnapshot) {}

        fn on_closed(&self) {}
    }

    #[test]
    fn simultaneous_query_demand_follow_and_receipt_drains_charge_five_tasks() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let query = engine
            .observe(FfiFilter::default(), Box::new(CensusRowObserver))
            .expect("query observer must start");
        let demand = engine
            .observe_demand(
                crate::types::FfiDemand {
                    selection: FfiFilter::default(),
                    source: crate::types::FfiSourceAuthority::Public,
                    access: crate::types::FfiAccessContext::Public,
                    cache: crate::types::FfiCacheMode::Agnostic,
                },
                Box::new(CensusRowObserver),
            )
            .expect("demand observer must start");
        let target = nostr::Keys::generate().public_key().to_hex();
        let follow = engine
            .observe_following(target, Box::new(CensusFollowObserver))
            .expect("follow projection and bridge must start");

        let (receipt_tx, receipt_rx) = mpsc::channel::<()>();
        let reservation = engine
            .engine
            .reserve_native_task("receipt-observer")
            .unwrap();
        let cancel = engine.engine.native_task_cancel().unwrap();
        spawn_native_bridge(reservation, cancel, "receipt-observer", move || {
            while receipt_rx.recv().is_ok() {}
        })
        .unwrap();

        assert_eq!(
            engine.native_task_census().admitted,
            5,
            "query + demand + follow projection/bridge + receipt drain"
        );
        query.cancel();
        demand.cancel();
        follow.cancel();
        drop(query);
        drop(demand);
        drop(follow);
        drop(receipt_tx);
        engine.await_native_tasks_idle();
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
        engine.shutdown();
    }

    #[test]
    fn finite_native_executor_refuses_before_acceptance_and_returns_exact_baseline() {
        let engine = NmpEngine::new(NmpEngineConfig {
            max_native_tasks: 1,
            ..NmpEngineConfig::default()
        })
        .expect("engine must build");
        let closes = Arc::new(AtomicUsize::new(0));
        let handle = engine
            .observe_diagnostics(Box::new(CensusDiagnosticsObserver {
                closes: Arc::clone(&closes),
            }))
            .expect("cap-sized observer must start");
        assert_eq!(engine.native_task_census().admitted, 1);

        let refusal = match engine.observe_diagnostics(Box::new(CensusDiagnosticsObserver {
            closes: Arc::new(AtomicUsize::new(0)),
        })) {
            Ok(_) => panic!("a cap-sized executor must refuse another observer"),
            Err(error) => error,
        };
        assert_eq!(
            refusal,
            FfiError::ExecutorSaturated {
                component: "diagnostics-observer".to_string(),
                capacity: 1,
            }
        );

        handle.cancel();
        drop(handle);
        engine.await_native_tasks_idle();
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    #[test]
    fn ffi_persistent_store_reset_is_destructive_and_idempotent() {
        let fixture = tempfile::tempdir().expect("tempdir");
        let path = fixture.path().join("nmp.redb");
        let config = NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        };
        let engine = NmpEngine::new(config.clone()).expect("persistent engine must build");
        engine.shutdown();

        reset_persistent_store(path.to_string_lossy().into_owned())
            .expect("closed FFI store must reset");
        assert!(!path.exists(), "FFI reset must remove the canonical store");
        reset_persistent_store(path.to_string_lossy().into_owned())
            .expect("missing FFI store is already reset");

        let reopened = NmpEngine::new(config).expect("reset store must reopen fresh");
        reopened.shutdown();
    }

    #[test]
    fn reattachment_mapping_is_exhaustive_and_distinct() {
        let (_tx, rx) = mpsc::channel();
        assert_eq!(
            reattachment_to_ffi(&ReceiptReattachment::Attached(rx)),
            FfiReceiptReattachment::Attached
        );
        assert_eq!(
            reattachment_to_ffi(&ReceiptReattachment::NotFound),
            FfiReceiptReattachment::NotFound
        );
        assert_eq!(
            reattachment_to_ffi(&ReceiptReattachment::RetainedButUnreadable),
            FfiReceiptReattachment::RetainedButUnreadable
        );
    }

    struct ChannelReceiptObserver {
        tx: Mutex<mpsc::Sender<FfiWriteStatus>>,
    }

    enum SignEventOutcome {
        Signed(FfiSignedEvent),
        Failed(FfiSignEventFailure),
    }

    struct ChannelSignEventObserver {
        tx: Mutex<mpsc::Sender<SignEventOutcome>>,
    }

    impl SignEventObserver for ChannelSignEventObserver {
        fn on_signed(&self, event: FfiSignedEvent) {
            let _ = self
                .tx
                .lock()
                .unwrap()
                .send(SignEventOutcome::Signed(event));
        }

        fn on_failed(&self, failure: FfiSignEventFailure) {
            let _ = self
                .tx
                .lock()
                .unwrap()
                .send(SignEventOutcome::Failed(failure));
        }
    }

    struct CountingFfiSigner {
        keys: nostr::Keys,
        calls: Arc<AtomicUsize>,
    }

    impl nmp_signer::SigningCapability for CountingFfiSigner {
        fn public_key(&self) -> Option<nostr::PublicKey> {
            Some(self.keys.public_key())
        }

        fn sign(&self, unsigned: nostr::UnsignedEvent) -> nmp_signer::SignerOp<nostr::Event> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            nmp_signer::SignerOp::ok(unsigned.sign_with_keys(&self.keys).unwrap())
        }
    }

    #[test]
    fn ffi_sign_event_cap_one_returns_the_exact_verified_event_without_publish_api_use() {
        let engine = NmpEngine::new(NmpEngineConfig {
            max_native_tasks: 1,
            ..NmpEngineConfig::default()
        })
        .expect("engine must build");
        let author = engine
            .add_account(format!("{:064x}", 17u8))
            .expect("account must register");
        engine
            .set_active_account(Some(author.clone()))
            .expect("account must activate");
        let request = FfiSignEventRequest {
            created_at: 1_723_456_789,
            kind: 27_272,
            tags: vec![vec!["t".to_string(), "ffi-sign-only".to_string()]],
            content: "exact ffi body".to_string(),
        };
        let (tx, rx) = mpsc::channel();
        let handle = engine
            .sign_event(
                request.clone(),
                Box::new(ChannelSignEventObserver { tx: Mutex::new(tx) }),
            )
            .expect("sign operation must start");

        let signed = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            SignEventOutcome::Signed(event) => event,
            SignEventOutcome::Failed(failure) => panic!("unexpected sign failure: {failure:?}"),
        };
        assert_eq!(signed.pubkey, author);
        assert_eq!(signed.created_at, request.created_at);
        assert_eq!(signed.kind, request.kind);
        assert_eq!(signed.tags, request.tags);
        assert_eq!(signed.content, request.content);
        assert_eq!(signed.id.len(), 64);
        assert_eq!(signed.sig.len(), 128);
        drop(handle);
        engine.await_native_tasks_idle();
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_missing_active_signer_is_typed_and_starts_no_callback() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        engine
            .set_active_account(Some(keys.public_key().to_hex()))
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let result = engine.sign_event(
            FfiSignEventRequest {
                created_at: 1,
                kind: 1,
                tags: Vec::new(),
                content: "body".to_string(),
            },
            Box::new(ChannelSignEventObserver { tx: Mutex::new(tx) }),
        );
        match result {
            Err(error) => assert_eq!(error, FfiError::NoActiveSigner),
            Ok(_) => panic!("missing signer must refuse synchronously"),
        }
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        engine.await_native_tasks_idle();
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_refuses_malformed_tags_before_callback_or_admission() {
        let engine = NmpEngine::new(NmpEngineConfig {
            max_native_tasks: 1,
            ..NmpEngineConfig::default()
        })
        .expect("engine must build");
        let author = engine.add_account(format!("{:064x}", 31u8)).unwrap();
        engine.set_active_account(Some(author)).unwrap();
        let (tx, rx) = mpsc::channel();

        let result = engine.sign_event(
            FfiSignEventRequest {
                created_at: 1,
                kind: 1,
                tags: vec![Vec::new()],
                content: "malformed".to_string(),
            },
            Box::new(ChannelSignEventObserver { tx: Mutex::new(tx) }),
        );
        match result {
            Err(error) => assert_eq!(error, FfiError::InvalidTag { got: Vec::new() }),
            Ok(_) => panic!("malformed input must fail before operation admission"),
        }
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_capacity_refusal_precedes_signer_invocation_and_callback() {
        let engine = NmpEngine::new(NmpEngineConfig {
            max_native_tasks: 1,
            ..NmpEngineConfig::default()
        })
        .expect("engine must build");
        let keys = nostr::Keys::generate();
        let calls = Arc::new(AtomicUsize::new(0));
        engine
            .engine
            .add_signer(CountingFfiSigner {
                keys: keys.clone(),
                calls: Arc::clone(&calls),
            })
            .unwrap();
        engine
            .engine
            .set_active_account(Some(keys.public_key()))
            .unwrap();
        let held = engine
            .engine
            .reserve_native_task("capacity fixture")
            .unwrap();
        let (tx, rx) = mpsc::channel();

        let result = engine.sign_event(
            pending_ffi_request(),
            Box::new(ChannelSignEventObserver { tx: Mutex::new(tx) }),
        );
        match result {
            Err(error) => assert_eq!(
                error,
                FfiError::ExecutorSaturated {
                    component: "sign-event".to_string(),
                    capacity: 1,
                }
            ),
            Ok(_) => panic!("capacity must refuse before operation admission"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(held);
        engine.await_native_tasks_idle();
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_after_engine_close_is_typed_and_never_calls_back() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        engine.shutdown();
        let (tx, rx) = mpsc::channel();
        let result = engine.sign_event(
            pending_ffi_request(),
            Box::new(ChannelSignEventObserver { tx: Mutex::new(tx) }),
        );
        match result {
            Err(error) => assert_eq!(error, FfiError::EngineClosed),
            Ok(_) => panic!("a closed engine must refuse before operation admission"),
        }
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
    }

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

    #[test]
    fn ffi_sign_event_reports_malicious_output_without_fabricating_success() {
        let engine = NmpEngine::new(NmpEngineConfig {
            max_native_tasks: 1,
            ..NmpEngineConfig::default()
        })
        .expect("engine must build");
        let reported = nostr::Keys::generate().public_key();
        engine
            .engine
            .add_signer(MismatchedFfiSigner {
                reported,
                actual: nostr::Keys::generate(),
            })
            .unwrap();
        engine.engine.set_active_account(Some(reported)).unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = engine
            .sign_event(
                pending_ffi_request(),
                Box::new(ChannelSignEventObserver { tx: Mutex::new(tx) }),
            )
            .expect("operation must start");
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            SignEventOutcome::Failed(FfiSignEventFailure::InvalidSignerOutput { .. })
        ));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(handle);
        engine.await_native_tasks_idle();
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
        engine.shutdown();
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
        let engine = NmpEngine::new(NmpEngineConfig {
            max_native_tasks: 1,
            ..NmpEngineConfig::default()
        })
        .expect("engine must build");
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

    struct ReentrantSignObserver {
        engine: Arc<NmpEngine>,
        tx: Mutex<mpsc::Sender<(String, bool)>>,
    }

    impl SignEventObserver for ReentrantSignObserver {
        fn on_signed(&self, event: FfiSignedEvent) {
            let active_before_shutdown = self
                .engine
                .active_account()
                .expect("callback can call an engine verb")
                .expect("fixture has an active account");
            self.engine.shutdown();
            let closed_after_shutdown =
                matches!(self.engine.active_account(), Err(FfiError::EngineClosed));
            let _ = self.tx.lock().unwrap().send((
                format!("{}:{}", active_before_shutdown, event.id),
                closed_after_shutdown,
            ));
        }

        fn on_failed(&self, failure: FfiSignEventFailure) {
            panic!("unexpected sign failure: {failure:?}");
        }
    }

    #[test]
    fn ffi_sign_event_callback_can_reenter_verbs_and_shutdown_without_deadlock() {
        let engine = NmpEngine::new(NmpEngineConfig {
            max_native_tasks: 1,
            ..NmpEngineConfig::default()
        })
        .expect("engine must build");
        let author = engine.add_account(format!("{:064x}", 32u8)).unwrap();
        engine.set_active_account(Some(author.clone())).unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = engine
            .sign_event(
                pending_ffi_request(),
                Box::new(ReentrantSignObserver {
                    engine: Arc::clone(&engine),
                    tx: Mutex::new(tx),
                }),
            )
            .expect("operation must start");

        let (value, closed_after_shutdown) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reentrant callback must complete");
        assert!(value.starts_with(&author));
        assert!(closed_after_shutdown);
        drop(handle);
        engine.await_native_tasks_idle();
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
    }

    #[test]
    fn ffi_sign_event_caller_cancel_completes_once_and_returns_executor_to_zero() {
        let (engine, cancellations) = pending_ffi_sign_engine();
        let (tx, rx) = mpsc::channel();
        let handle = engine
            .sign_event(
                pending_ffi_request(),
                Box::new(ChannelSignEventObserver { tx: Mutex::new(tx) }),
            )
            .expect("operation must start");
        handle.cancel();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            SignEventOutcome::Failed(FfiSignEventFailure::Cancelled)
        ));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(handle);
        engine.await_native_tasks_idle();
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
        engine.shutdown();
    }

    #[test]
    fn ffi_sign_event_shutdown_completes_once_and_joins_to_zero() {
        let (engine, cancellations) = pending_ffi_sign_engine();
        let (tx, rx) = mpsc::channel();
        let handle = engine
            .sign_event(
                pending_ffi_request(),
                Box::new(ChannelSignEventObserver { tx: Mutex::new(tx) }),
            )
            .expect("operation must start");
        engine.shutdown();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            SignEventOutcome::Failed(FfiSignEventFailure::Cancelled)
        ));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(handle);
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        assert_eq!(engine.native_task_census().admitted, 0);
        assert_eq!(engine.native_task_census().running, 0);
    }

    impl ReceiptObserver for ChannelReceiptObserver {
        fn on_status(&self, status: FfiWriteStatus) {
            let _ = self.tx.lock().unwrap().send(status);
        }

        fn on_closed(&self) {}
    }

    /// #52's headline falsifier, exercised through the FFI boundary this
    /// time (the direct-Rust equivalent lives in `nmp::Engine`'s own tests):
    /// a tampered `FfiWritePayload::Signed` is no longer a synchronous
    /// `FfiError` -- `NmpEngine::publish` accepts it and the rejection
    /// surfaces on the receipt stream as `WriteStatus::Failed`, the FIRST
    /// and only status delivered, proving the verify inherited from
    /// `nmp::Engine`'s acceptance boundary (Unit A0) covers this entry point
    /// too, not only direct-Rust.
    #[test]
    fn ffi_tampered_signed_publish_fails_closed_on_receipt_stream() {
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
                // Tampered after signing: id/sig no longer match this
                // content, but every field still parses fine at the FFI
                // boundary (marshaling only, no verify here anymore).
                content: "tampered".to_string(),
                sig: event.sig.to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
        };

        let (tx, rx) = mpsc::channel();
        let observer = Box::new(ChannelReceiptObserver { tx: Mutex::new(tx) });

        let receipt_id = engine
            .publish(intent, observer)
            .expect("a well-formed (if tampered) Signed payload must parse at the FFI boundary");
        assert!(receipt_id > 0, "publish must expose its stable receipt id");

        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a Durable intent must yield a status")
        {
            FfiWriteStatus::Failed { .. } => {}
            other => panic!("expected FfiWriteStatus::Failed, got {other:?}"),
        }
        assert!(
            rx.recv_timeout(Duration::from_secs(1)).is_err(),
            "Failed must be the sole terminal status -- no Accepted, nothing further"
        );

        engine.shutdown();
    }

    /// #156 account-switch falsifier through the public native boundary.
    /// Composition snapshots A, but switching to B is serialized ahead of
    /// publish on the sole engine command path. Acceptance must reject the
    /// stale A draft before `Accepted`, canonical storage, or the durable
    /// outbox journal can observe it. The lower engine test uses counting
    /// signers to prove neither account capability is invoked.
    #[test]
    fn ffi_group_message_composed_as_a_cannot_publish_after_switching_to_b() {
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

        engine.set_active_account(Some(a)).expect("A must activate");
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
            .set_active_account(Some(b))
            .expect("B must activate before publish");

        let (tx, rx) = mpsc::channel();
        let receipt_id = engine
            .publish_composed(
                intent,
                Box::new(ChannelReceiptObserver { tx: Mutex::new(tx) }),
            )
            .expect("pre-acceptance failure still has a stream-local correlation id");
        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("stale author must fail deterministically")
        {
            FfiWriteStatus::Failed { reason } => assert_eq!(
                reason,
                "unsigned draft author does not match current active account"
            ),
            other => panic!("Failed must be first, before Accepted; got {other:?}"),
        }
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected),
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
            .observe(nmp::LiveQuery(cache_probe))
            .expect("canonical query must open");
        let (deltas, _evidence) = subscription
            .recv_timeout(Duration::from_secs(5))
            .expect("canonical query must deliver its current empty snapshot");
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, nmp::RowDelta::Added(_))),
            "a pre-acceptance rejection must create no canonical row"
        );
        drop(subscription);

        let (reattach_tx, reattach_rx) = mpsc::channel();
        let outcome = engine
            .reattach_receipt(
                receipt_id,
                Box::new(ChannelReceiptObserver {
                    tx: Mutex::new(reattach_tx),
                }),
            )
            .expect("reattach lookup must succeed");
        assert_eq!(outcome, FfiReceiptReattachment::NotFound);
        assert_eq!(
            reattach_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
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

    /// #99: PR #97's FFI reattach coverage stopped at `reattachment_to_ffi`,
    /// a pure enum-mapping unit test -- it never drove the real
    /// `NmpEngine::reattach_receipt` method, so a broken observer-forwarding
    /// observer bridge spawn (facade.rs's `Attached` arm) could leave direct Rust
    /// correct while every FFI caller silently received nothing. This test
    /// publishes a real durable intent (no signer ever attaches, so it
    /// settles into a genuinely RETAINED `Accepted`+`AwaitingCapability`
    /// steady state -- see `EngineCore::reattach_receipt`'s replay match),
    /// reattaches with a SECOND, independent observer, and proves that
    /// fresh observer receives the identical replayed fact sequence the
    /// original one saw -- through the real forwarding thread, not a mock.
    #[test]
    fn ffi_reattach_replays_real_receipt_facts_through_a_fresh_observer() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let keys = nostr::Keys::generate();
        // Active WITHOUT `add_account`: satisfies publish's "there must be
        // an active account" gate while registering no signer capability at
        // all, so the accepted intent has no way to ever leave
        // `AwaitingCapability` -- exactly the retained steady state this
        // test needs to reattach against.
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
        };

        let (tx, rx) = mpsc::channel();
        let observer = Box::new(ChannelReceiptObserver { tx: Mutex::new(tx) });
        let receipt_id = engine
            .publish(intent, observer)
            .expect("a well-formed unsigned intent must enqueue");
        assert!(receipt_id > 0, "publish must expose its stable receipt id");

        // Real synchronization on the ORIGINAL observer first: block for
        // the exact retained steady state (Accepted, then AwaitingCapability
        // because no signer is ever attached) before reattaching at all --
        // proves the obligation is genuinely retained, not a guessed delay.
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10))
                .expect("must observe Accepted"),
            FfiWriteStatus::Accepted
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10))
                .expect("must observe AwaitingCapability"),
            FfiWriteStatus::AwaitingCapability
        );

        // Reattach through a FRESH observer/channel -- exercises the real
        // observer forwarding path in `NmpEngine::reattach_receipt`,
        // not just the enum mapping.
        let (tx2, rx2) = mpsc::channel();
        let replay_observer = Box::new(ChannelReceiptObserver {
            tx: Mutex::new(tx2),
        });
        let outcome = engine
            .reattach_receipt(receipt_id, replay_observer)
            .expect("reattach call must succeed while the engine is open");
        assert_eq!(outcome, FfiReceiptReattachment::Attached);

        assert_eq!(
            rx2.recv_timeout(Duration::from_secs(10))
                .expect("replay must deliver Accepted"),
            FfiWriteStatus::Accepted
        );
        assert_eq!(
            rx2.recv_timeout(Duration::from_secs(10))
                .expect("replay must deliver AwaitingCapability"),
            FfiWriteStatus::AwaitingCapability
        );

        engine.shutdown();
    }

    /// #99: a `NotFound`/`RetainedButUnreadable` reattach must spawn NO
    /// forwarding thread and deliver NO facts -- `NmpEngine::reattach_receipt`
    /// simply never moves `observer` out of its own stack frame on those
    /// arms, so it is dropped, synchronously, before this call even returns.
    /// That makes the proof fully deterministic (no bounded wait needed at
    /// all, let alone a sleep): if a forwarding thread had wrongly captured
    /// `observer` (or a clone of its sender), the channel would still be
    /// open and `try_recv` would block forever/return `Empty`, not
    /// `Disconnected`.
    #[test]
    fn ffi_reattach_of_unknown_id_spawns_no_forwarding_thread_and_delivers_no_facts() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");
        let (tx, rx) = mpsc::channel();
        let observer = Box::new(ChannelReceiptObserver { tx: Mutex::new(tx) });

        let outcome = engine
            .reattach_receipt(999_999, observer)
            .expect("reattach call must succeed while the engine is open");
        assert_eq!(outcome, FfiReceiptReattachment::NotFound);

        assert_eq!(
            rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected),
            "no forwarding thread must have been spawned -- the dropped observer's sender must \
             already be gone by the time reattach_receipt returns, not merely quiet"
        );

        engine.shutdown();
    }

    /// #99's other `RetainedButUnreadable` half: a GENUINELY corrupt
    /// retained receipt (real undecodable bytes in a real `RedbStore` file,
    /// the same technique `nmp-engine`'s own restart/corruption tests use)
    /// must report `RetainedButUnreadable` through the FFI boundary too,
    /// and -- like `NotFound` above -- spawn no forwarding thread and
    /// deliver no facts (same code path: `NotFound | RetainedButUnreadable
    /// => {}`).
    #[test]
    fn ffi_reattach_of_corrupt_retained_receipt_is_unreadable_and_spawns_no_thread() {
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
            };
            let (tx, rx) = mpsc::channel();
            let observer = Box::new(ChannelReceiptObserver { tx: Mutex::new(tx) });
            let receipt_id = engine
                .publish(intent, observer)
                .expect("a well-formed unsigned intent must enqueue");
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(10))
                    .expect("must observe Accepted"),
                FfiWriteStatus::Accepted
            );
            engine.shutdown();
            receipt_id
        };

        // Overwrite the receipt's own durable row with undecodable bytes --
        // the store must have already released the file after `shutdown()`.
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
        let (tx2, rx2) = mpsc::channel();
        let observer = Box::new(ChannelReceiptObserver {
            tx: Mutex::new(tx2),
        });
        let outcome = engine
            .reattach_receipt(receipt_id, observer)
            .expect("reattach call must succeed while the engine is open");
        assert_eq!(outcome, FfiReceiptReattachment::RetainedButUnreadable);
        assert_eq!(
            rx2.try_recv(),
            Err(mpsc::TryRecvError::Disconnected),
            "an unreadable retained receipt must spawn no forwarding thread either"
        );

        engine.shutdown();
    }

    struct ClosedCountingRowObserver {
        closed_tx: Mutex<mpsc::Sender<()>>,
    }

    impl RowObserver for ClosedCountingRowObserver {
        fn on_batch(
            &self,
            _deltas: Vec<crate::types::FfiRowDelta>,
            _evidence: crate::types::FfiAcquisitionEvidence,
        ) {
        }

        fn on_closed(&self) {
            let _ = self.closed_tx.lock().unwrap().send(());
        }
    }

    /// codex-nova's non-negotiable proof #2, wired all the way through the
    /// real FFI drain thread (the isolated `ObservationCancel` guard proof
    /// lives in `nmp::subscription::tests`): calling `cancel()` on the SAME
    /// `NmpQueryHandle` from two different `Arc` owners, then dropping
    /// both, must still withdraw exactly once and deliver the drain
    /// thread's `RowObserver::on_closed` exactly once -- never zero (a
    /// hang), never more than once.
    #[test]
    fn ffi_repeated_cancel_across_arc_owners_and_drop_yields_exactly_one_on_closed() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine must build");

        let (closed_tx, closed_rx) = mpsc::channel();
        let observer = Box::new(ClosedCountingRowObserver {
            closed_tx: Mutex::new(closed_tx),
        });

        let filter = FfiFilter {
            kinds: Some(vec![9999]),
            ..FfiFilter::default()
        };
        let handle = engine
            .observe(filter, observer)
            .expect("a well-formed filter must be accepted");

        // Two independent `Arc` owners of the SAME `NmpQueryHandle` -- both
        // call `cancel()`, then both are dropped, mirroring a caller that
        // cancels explicitly and also lets its last reference go out of
        // scope.
        let handle_other_owner = Arc::clone(&handle);
        handle.cancel();
        handle_other_owner.cancel();
        handle.cancel();
        drop(handle);
        drop(handle_other_owner);

        closed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("on_closed must fire once the subscription is withdrawn, not hang");
        assert!(
            closed_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "on_closed must fire EXACTLY once, not once per cancel() call/Arc owner/Drop"
        );

        engine.shutdown();
    }

    struct ClosedCountingReceiptObserver {
        status_tx: Mutex<mpsc::Sender<FfiWriteStatus>>,
        closed_tx: Mutex<mpsc::Sender<()>>,
    }

    impl ReceiptObserver for ClosedCountingReceiptObserver {
        fn on_status(&self, status: FfiWriteStatus) {
            let _ = self.status_tx.lock().unwrap().send(status);
        }

        fn on_closed(&self) {
            let _ = self.closed_tx.lock().unwrap().send(());
        }
    }

    /// #125's falsifier, mirroring the `RowObserver` close proof above but for
    /// the receipt drain: a receipt stream must terminate its observer with
    /// exactly one `on_closed` when the receipt `Sender` is dropped (here via
    /// a tampered `Signed` payload that fails closed -- `Failed` is the sole
    /// terminal, after which the engine drops the receipt sender). Before this
    /// fix `NmpEngine::publish`'s drain loop ended silently, so a Swift/Kotlin
    /// receipt stream was never finished and its continuation leaked/hung.
    #[test]
    fn ffi_receipt_stream_yields_exactly_one_on_closed_when_sender_dropped() {
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
                // Tampered after signing: guarantees a fail-closed terminal so
                // the receipt sender is dropped and the drain loop ends.
                content: "tampered".to_string(),
                sig: event.sig.to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
        };

        let (status_tx, status_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let observer = Box::new(ClosedCountingReceiptObserver {
            status_tx: Mutex::new(status_tx),
            closed_tx: Mutex::new(closed_tx),
        });

        engine
            .publish(intent, observer)
            .expect("a well-formed (if tampered) Signed payload must parse at the FFI boundary");

        // The stream is genuinely active first (the terminal fact arrives),
        // proving `on_closed` follows real delivery, not an empty stream.
        match status_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a Durable intent must yield a status")
        {
            FfiWriteStatus::Failed { .. } => {}
            other => panic!("expected FfiWriteStatus::Failed, got {other:?}"),
        }

        closed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("on_closed must fire once the receipt sender is dropped, not hang");
        assert!(
            closed_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "on_closed must fire EXACTLY once when the receipt stream terminates"
        );

        engine.shutdown();
    }

    /// #156: `publish_composed` takes its `FfiComposedWriteIntent`
    /// exactly once. No signer is ever attached (`set_active_account` without
    /// `add_account`), so the first call settles into the SAME retained
    /// `Accepted`+`AwaitingCapability` steady state
    /// `ffi_reattach_replays_real_receipt_facts_through_a_fresh_observer`
    /// relies on -- no live relay needed to prove take-once. A second call on
    /// the identical `Arc<FfiComposedWriteIntent>` must fail closed with
    /// `FfiError::IntentAlreadyConsumed`, never silently re-publish the same
    /// template or hand back a fresh receipt.
    #[test]
    fn ffi_publish_composed_takes_the_intent_exactly_once() {
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

        let (tx_a, rx_a) = mpsc::channel();
        let observer_a = Box::new(ChannelReceiptObserver {
            tx: Mutex::new(tx_a),
        });
        let receipt_id = engine
            .publish_composed(intent.clone(), observer_a)
            .expect("the first publish_composed call must consume the intent and succeed");
        assert!(receipt_id > 0, "publish_composed must expose a receipt id");
        assert!(matches!(
            rx_a.recv_timeout(Duration::from_secs(5)),
            Ok(FfiWriteStatus::Accepted)
        ));

        let (tx_b, _rx_b) = mpsc::channel();
        let observer_b = Box::new(ChannelReceiptObserver {
            tx: Mutex::new(tx_b),
        });
        match engine.publish_composed(intent, observer_b) {
            Err(FfiError::IntentAlreadyConsumed) => {}
            other => {
                panic!("expected FfiError::IntentAlreadyConsumed on the second call, got {other:?}")
            }
        }

        engine.shutdown();
    }
}
