//! #1803: `dispatch_effect`'s `Effect::RelayAuth` arm bound an AUTH
//! capability via `core.handle(EngineMsg::AuthCapabilityBound { .. })`,
//! `debug_assert!`'d the returned effects were empty, and dropped them --
//! compiled out in release. The assert's premise ("binding an AUTH
//! capability is a synchronous state-only transition") describes the one
//! arm (`on_auth_capability_bound`, which does return `Vec::new()` on
//! every path) but not `EngineCore::handle` as a whole: its epilogue can
//! append effects after ANY arm returns.
//!
//! This drives the real call site -- `dispatch_effect`, not a lower-level
//! stand-in -- with the exact precondition the epilogue's
//! `prune_unowned_relay_state` branch checks (a stale `relay_open_failures`
//! entry `auth_required_sessions` also names, for a session nothing
//! currently requires) already sitting on `EngineCore` before the AUTH-bind
//! call, seeded via `EngineCore::seed_stale_relay_open_failure_for_test`
//! (see that method's doc for why the state can't be built up through an
//! ordinary sequence of `handle()` calls -- whichever call causes a
//! session to stop being required is always the one credited with
//! noticing it, so nothing durably STAYS stale for a later, unrelated
//! message to discover, except state seeded before the first call).

use super::*;
use crate::auth::{AuthPolicy, AuthPolicyOp, AuthPolicyRegistry, AuthPolicyRequest};
use nmp_engine::core::{AuthEffect, AuthEpoch, AuthOpToken};
use nmp_grammar::{AccessContext, RelaySessionKey};
use nmp_transport::{PoolConfig, RelayHandle};
use nostr::{Keys, RelayUrl};
use std::cell::RefCell;

fn test_verifier() -> nmp_transport::Verifier {
    nmp_transport::Verifier::new(
        nmp_transport::VerifyConfig::default(),
        std::sync::Arc::new(nmp_transport::NullKnownSig),
    )
    .expect("test verifier construction must succeed")
}

/// Always allows. Never resolved in this test -- `bind` fires synchronously
/// inside `start_auth_task`, before the operation this policy backs ever
/// runs on the adapter runtime, so what the policy eventually decides is
/// irrelevant to what's under test here.
struct AllowPolicy;

impl AuthPolicy for AllowPolicy {
    fn evaluate(&self, _request: AuthPolicyRequest) -> AuthPolicyOp {
        AuthPolicyOp::allow()
    }
}

#[test]
fn relay_auth_dispatch_delivers_the_epilogue_effect_instead_of_dropping_it() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test adapter runtime");

    let relay = RelayUrl::parse("wss://relay-auth-epilogue.example.com").unwrap();
    let expected_pubkey = Keys::generate().public_key();
    let session = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(expected_pubkey));

    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    // Nothing subscribes to or writes through `session`, so
    // `relay_worker_requirements().all` will not contain it -- the "nothing
    // currently requires this session" half of the epilogue precondition.
    // This is also the first `handle()` call this fresh core will ever
    // process, so the dispatch below is the exact call whose epilogue
    // absorbs the cleanup -- not an earlier, unobserved one (see the
    // seeding method's doc comment).
    core.seed_stale_relay_open_failure_for_test(
        session.clone(),
        "injected: relay worker failed to open".to_string(),
    );

    let token = AuthOpToken {
        epoch: AuthEpoch {
            handle: RelayHandle {
                slot: 0,
                generation: 1,
            },
            session,
            sequence: 0,
        },
        sequence: 0,
    };

    let mut policies = AuthPolicyRegistry::default();
    policies.add(
        expected_pubkey,
        nmp_engine::core::AuthCapabilityInstance(1),
        Box::new(AllowPolicy),
    );

    let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
    let pool = Pool::new(PoolConfig::default(), test_verifier(), pool_tx).expect("test pool");
    let (self_inbox, _self_rx) = std::sync::mpsc::channel();
    let relay_information = RelayInformationService::new(rt.handle().clone());
    let nip11_decisions = RefCell::new(Nip11DecisionState::default());
    let wire_admission = RefCell::new(WireAdmissionState::default());
    let diagnostics_delivery = RefCell::new(DiagnosticsDeliveryState::default());
    let auth_policies = RefCell::new(policies);
    let auth_tasks = RefCell::new(auth::AuthTaskRegistry::default());
    let receipt_deliveries = RefCell::new(ReceiptDeliveryRegistry::default());
    let route_provider = RefCell::new(None);
    let dispatch_runtime = DispatchRuntime {
        self_inbox: &self_inbox,
        relay_information: &relay_information,
        runtime: rt.handle(),
        nip11_decisions: &nip11_decisions,
        wire_admission: &wire_admission,
        diagnostics_delivery: &diagnostics_delivery,
        auth_policies: &auth_policies,
        auth_tasks: &auth_tasks,
        receipt_deliveries: &receipt_deliveries,
        route_provider: &route_provider,
    };

    let mut row_channels = HashMap::new();
    let mut history_channels = HashMap::new();
    let mut diag_channels = HashMap::new();
    let (diag_tx, diag_rx) = latest_channel::<DiagnosticsSnapshot>();
    diag_channels.insert(1, diag_tx);
    let registry = SignerRegistry::default();

    dispatch_effect(
        &mut core,
        Effect::RelayAuth(AuthEffect::RequestPolicy {
            token,
            expected_pubkey,
            challenge: "epilogue-falsifier".to_string(),
        }),
        &pool,
        &mut row_channels,
        &mut history_channels,
        &mut diag_channels,
        &registry,
        dispatch_runtime,
    );

    assert!(
        diag_rx.try_recv().is_ok(),
        "the AUTH-bind's epilogue effect (Effect::EmitDiagnostics from \
         prune_unowned_relay_state) must reach the registered diagnostics \
         observer instead of being dropped at the RelayAuth call site"
    );

    pool.shutdown();
}
