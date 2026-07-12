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
use std::thread;

use crate::convert::{
    demand_from_ffi, diagnostics_snapshot_to_ffi, evidence_to_ffi, filter_from_ffi, parse_pubkey,
    row_delta_to_ffi, write_intent_from_ffi, write_status_to_ffi, FfiError, WriteStatusRef,
};
use crate::observer::{DiagnosticsObserver, ReceiptObserver, RowObserver};
use crate::types::{FfiDemand, FfiFilter, FfiReceiptReattachment, FfiWriteIntent};
use nmp::ReceiptReattachment;

fn reattachment_to_ffi(value: &ReceiptReattachment) -> FfiReceiptReattachment {
    match value {
        ReceiptReattachment::Attached(_) => FfiReceiptReattachment::Attached,
        ReceiptReattachment::NotFound => FfiReceiptReattachment::NotFound,
        ReceiptReattachment::RetainedButUnreadable => FfiReceiptReattachment::RetainedButUnreadable,
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
    /// OPT-IN, defense-in-depth ceiling on concurrently-connected relays
    /// (issue #121). `0` imposes no cap; a non-zero value refuses relay dials
    /// past it and counts them in the diagnostics `relays_rejected_over_cap`.
    /// NOT the primary worker-exhaustion defense — fan-out is already bounded
    /// by `nmp-router`'s solver cap; this is a coarse absolute backstop.
    ///
    /// The `default =` literal below MUST stay equal to
    /// [`DEFAULT_MAX_RELAYS`] (uniffi record defaults accept only a literal,
    /// never a const path) — the const is the single Rust-side knob; the
    /// literal is its foreign-binding mirror. The default VALUE itself is an
    /// open owner decision (sane cap vs. uncapped, issue #121); `0` is the
    /// interim uncapped placeholder.
    #[uniffi(default = 0)]
    pub max_relays: u32,
}

/// The default relay-count ceiling for a freshly-constructed engine config
/// (issue #121). HOLD: the value is an open owner decision (a sane cap vs.
/// uncapped); `0` (uncapped) is the interim placeholder. When the owner picks
/// a number, update BOTH this const AND the `#[uniffi(default = N)]` literal
/// on [`NmpEngineConfig::max_relays`] above — they must match.
pub const DEFAULT_MAX_RELAYS: u32 = 0;

// Compile-time guard that the Rust `Default` derive for `NmpEngineConfig`
// (which yields `0` for `max_relays`) still agrees with `DEFAULT_MAX_RELAYS`.
// If the owner raises the const without giving `NmpEngineConfig` a matching
// manual `Default`, this fails the build rather than silently diverging.
const _: () = assert!(DEFAULT_MAX_RELAYS == 0);

impl From<NmpEngineConfig> for nmp::EngineConfig {
    fn from(config: NmpEngineConfig) -> Self {
        nmp::EngineConfig {
            store_path: config.store_path,
            indexer_relays: config.indexer_relays,
            app_relays: config.app_relays,
            fallback_relays: config.fallback_relays,
            allowed_local_relay_hosts: config.allowed_local_relay_hosts,
            max_relays: config.max_relays as usize,
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
    engine: nmp::Engine,
}

#[uniffi::export]
impl NmpEngine {
    #[uniffi::constructor]
    pub fn new(config: NmpEngineConfig) -> Result<Arc<Self>, FfiError> {
        let engine = nmp::Engine::new(config.into())?;
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
        let subscription = self.engine.observe(nmp::LiveQuery::from_filter(filter))?;
        let cancel = subscription.cancel_handle();

        thread::spawn(move || {
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
        let subscription = self.engine.observe(nmp::LiveQuery(demand))?;
        let cancel = subscription.cancel_handle();

        thread::spawn(move || {
            while let Ok((deltas, evidence)) = subscription.recv() {
                let ffi_deltas = deltas.iter().map(row_delta_to_ffi).collect();
                observer.on_batch(ffi_deltas, evidence_to_ffi(evidence));
            }
            observer.on_closed();
        });

        Ok(Arc::new(NmpQueryHandle { cancel }))
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
        let receipt = self.engine.publish_tracked(write_intent)?;
        let receipt_id = receipt.id.0;
        let receipt_rx = receipt.statuses;

        thread::spawn(move || {
            while let Ok(status) = receipt_rx.recv() {
                observer.on_status(write_status_to_ffi(WriteStatusRef(&status)));
            }
            observer.on_closed();
        });

        Ok(receipt_id)
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
        let write_intent = intent.take()?;
        let receipt = self.engine.publish_tracked(write_intent)?;
        let receipt_id = receipt.id.0;
        let receipt_rx = receipt.statuses;

        thread::spawn(move || {
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
        let result = self.engine.reattach_receipt(nmp::ReceiptId(receipt_id))?;
        let ffi_result = reattachment_to_ffi(&result);
        match result {
            ReceiptReattachment::Attached(receipt_rx) => {
                thread::spawn(move || {
                    while let Ok(status) = receipt_rx.recv() {
                        observer.on_status(write_status_to_ffi(WriteStatusRef(&status)));
                    }
                    observer.on_closed();
                });
            }
            ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => {}
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
        let subscription = self.engine.observe_diagnostics()?;
        let cancel = subscription.cancel_handle();

        thread::spawn(move || {
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

/// The app-facing handle to a live diagnostics stream (returned by
/// [`NmpEngine::observe_diagnostics`]). Same discipline as [`NmpQueryHandle`]
/// -- holds ONLY the opaque [`nmp::ObservationCancel`] token
/// (`DiagnosticsSubscription::cancel_handle`), the SAME type
/// [`NmpQueryHandle`] holds.
#[derive(uniffi::Object)]
pub struct NmpDiagnosticsHandle {
    cancel: nmp::ObservationCancel,
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
    use crate::types::{FfiDurability, FfiWritePayload, FfiWriteRouting, FfiWriteStatus};
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

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

    /// #99: PR #97's FFI reattach coverage stopped at `reattachment_to_ffi`,
    /// a pure enum-mapping unit test -- it never drove the real
    /// `NmpEngine::reattach_receipt` method, so a broken observer-forwarding
    /// `thread::spawn` (facade.rs's `Attached` arm) could leave direct Rust
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
        // `thread::spawn` forwarding path in `NmpEngine::reattach_receipt`,
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

    /// #115 falsifier 10: `publish_composed` takes its `FfiComposedWriteIntent`
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

        let intent = crate::nip29::group_send_intent(
            "wss://group-host.example.com".to_string(),
            "group-a".to_string(),
            keys.public_key().to_hex(),
            nostr::Timestamp::now().as_secs(),
            9,
            "hi".to_string(),
            vec![],
            vec![],
        )
        .expect("a well-formed group send must compose");

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
