use super::*;
use crate::types::{
    FfiAccessContext, FfiBinding, FfiCacheMode, FfiDemand, FfiFilter, FfiFrame, FfiFreshness,
    FfiIdentity, FfiLiveQuery, FfiNotSentReason, FfiRowDelta, FfiSignEventRequest, FfiSigningState,
    FfiSourceAuthority, FfiWindow, FfiWindowLoad, FfiWriteFact, FfiWriteOutcome, FfiWritePayload,
    FfiWriteRouting,
};
use std::collections::BTreeSet;
use std::time::Duration;

/// One `Public` demand branch over `selection`, on an unauthenticated
/// connection with the default cache and freshness policies -- the shortest
/// complete live query a test can declare now that no door infers one.
fn public_query(selection: FfiFilter) -> FfiLiveQuery {
    FfiLiveQuery {
        branches: vec![FfiDemand {
            selection,
            source: FfiSourceAuthority::Public,
            access: FfiAccessContext::Public,
            cache: FfiCacheMode::Agnostic,
            freshness: FfiFreshness::Live,
        }],
        aggregate_result_limit: None,
    }
}

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
    // A config that names no routing algorithm installs none. The claim used
    // to be "the projected config's indexer list is empty"; the projected
    // config has no routing field at all now, so the claim is made where the
    // decision is: the provider this artifact would install.
    #[cfg(feature = "nip65")]
    assert!(
        super::route_provider(&config)
            .expect("no outbox routing selected is not an error")
            .is_none(),
        "core native has no discovery-source setting; optional providers own their sources"
    );
    let projected = nmp::EngineConfig::from(config);

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
    assert!(
        super::route_provider(&config)
            .expect("a well-formed indexer set builds a provider")
            .is_some(),
        "the selected indexers belong to the provider that was constructed from them"
    );
    let projected = nmp::EngineConfig::from(config.clone());
    assert!(
        projected.app_relays.is_empty() && projected.fallback_relays.is_empty(),
        "an indexer must not become a generic operator lane"
    );

    let engine = NmpEngine::new(config, None).expect("a nonempty app-owned indexer set is valid");
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
    assert!(matches!(
        result,
        Err(crate::nip02::FfiFollowActionError::AutomaticRoutingUnavailable)
    ));
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
                    ..
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
        .observe(
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
    let bounded = recv_window_load(&handle, |load| load == FfiWindowLoad::AtBound { max: 2 }).await;
    assert_eq!(bounded.window.unwrap().rows.len(), 2);

    // An UNBOUNDED handle on the same engine has no window to grow --
    // the same verb fails closed, typed.
    let unbounded = engine
        .observe(
            public_query(FfiFilter {
                kinds: Some(vec![7_778]),
                ..FfiFilter::default()
            }),
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
            public_query(FfiFilter::default()),
            Some(FfiWindow::Expandable { initial: 0, max: 4 }),
        )
        .map(|_| ())
        .expect_err("a zero window bound must fail closed");
    assert_eq!(zero, FfiError::WindowZeroRows);

    let inverted = engine
        .observe(
            public_query(FfiFilter::default()),
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
            public_query(FfiFilter {
                limit: Some(1),
                ..FfiFilter::default()
            }),
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
            public_query(FfiFilter {
                kinds: Some(vec![7_778]),
                ..FfiFilter::default()
            }),
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
/// so `found` is `None`. The store-owned fixture retains every physical
/// table detail; retired table names are not recorded here.
#[test]
fn ffi_superseded_epoch_store_is_its_own_refusal_and_damaged_bytes_are_not() {
    let fixture = tempfile::tempdir().expect("tempdir");

    let superseded = fixture.path().join("superseded-epoch.redb");
    nmp_store::testing::create_nonempty_markerless_store(&superseded)
        .expect("epoch fixture must create");
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

    nmp_store::testing::corrupt_publish_queue_receipt(&path, receipt_id)
        .expect("store-owned receipt corruption");

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
            public_query(FfiFilter {
                kinds: Some(vec![9999]),
                ..FfiFilter::default()
            }),
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
            public_query(FfiFilter {
                kinds: Some(vec![kind.as_u16()]),
                ..FfiFilter::default()
            }),
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
