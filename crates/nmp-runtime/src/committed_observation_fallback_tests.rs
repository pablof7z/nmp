//! #1830: `RelayFrame::into_ordinary_fallback`'s `OrdinaryFallback::Unrecoverable`
//! arm reaches `reduce_and_dispatch_committed_observations` -- one of the two
//! call sites the issue named -- and must erase the session's exact
//! returned-frame count instead of silently discarding the frame the way
//! matching `Option::None` used to let it. A frame this reducer cannot hand
//! to any request left every request still streaming on the session with an
//! exact count it no longer earned, biasing coordinate reuse
//! (`nmp-engine/src/core/coordinate_coverage.rs`) toward `ProvenAbsent` for a
//! coordinate the dropped frame may have carried.
//!
//! Drives the real private dispatch function end to end: a real `EngineCore`
//! with one covering wire request actually placed and accepted through the
//! ordinary `RelayConnected` / `Subscribe` / `FlushWireAdmission` /
//! `on_wire_request_handoff` sequence (not a stand-in), a real empty `Pool`
//! (so `revalidate_committed_observations` genuinely rejects the synthetic
//! hit instead of being told to), and the real
//! `RelayFrame::into_ordinary_fallback` call this function makes on it.

use super::*;
use std::cell::RefCell;
use std::collections::BTreeSet;

use crate::auth::AuthPolicyRegistry;
use nmp_grammar::{AccessContext, ConcreteFilter, Demand, Filter, LiveQuery, SourceAuthority};
use nmp_router::WireOp;
use nmp_transport::{CommittedObservationHit, PoolConfig, RelayHandle};
use nostr::{EventId, Kind, RelayUrl, Timestamp};

fn test_verifier() -> nmp_transport::Verifier {
    nmp_transport::Verifier::new(
        nmp_transport::VerifyConfig::default(),
        std::sync::Arc::new(nmp_transport::NullKnownSig),
    )
    .expect("test verifier construction must succeed")
}

#[test]
fn unrecoverable_committed_observation_fallback_erases_the_session_count_instead_of_being_dropped(
) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test adapter runtime");

    let relay = RelayUrl::parse("wss://committed-observation-fallback.example.com").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let handle = RelayHandle {
        slot: 0,
        generation: 1,
    };

    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));

    let demand = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
            ..Filter::default()
        },
        SourceAuthority::Pinned(BTreeSet::from([relay])),
        AccessContext::Public,
    )
    .expect("a relay-pinned read is nonempty");
    core.handle(EngineMsg::Subscribe(LiveQuery::single(demand)));
    let admitted = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(1u64)));
    let attempt_id = admitted
        .iter()
        .find_map(|effect| {
            let Effect::Wire(delta) = effect else {
                return None;
            };
            delta.ops.iter().find_map(|(delta_session, ops)| {
                if delta_session != &session {
                    return None;
                }
                ops.iter().find_map(|op| {
                    let WireOp::Req(sub_id, filter) = op else {
                        return None;
                    };
                    let filter: &ConcreteFilter = filter;
                    Some(delta.attempt_id(delta_session, sub_id, filter))
                })
            })
        })
        .expect("the pinned read places exactly one REQ");
    core.on_wire_request_handoff(RequestHandoffOutcome::Accepted { attempt_id, handle });

    assert!(
        core.returned_frame_count_is_exact_for_test(&session),
        "fixture precondition: a freshly accepted request starts with an exact count"
    );

    let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
    let pool = Pool::new(PoolConfig::default(), test_verifier(), pool_tx).expect("test pool");
    let (self_inbox, _self_rx) = std::sync::mpsc::channel();
    let relay_information = RelayInformationService::new(rt.handle().clone());
    let nip11_decisions = RefCell::new(Nip11DecisionState::default());
    let wire_admission = RefCell::new(WireAdmissionState::default());
    let diagnostics_delivery = RefCell::new(DiagnosticsDeliveryState::default());
    let auth_policies = RefCell::new(AuthPolicyRegistry::default());
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
    let registry = SignerRegistry::default();

    // A committed-observation hit that will fail `pool.revalidate_committed_
    // observations` (the pool above has never published anything, so its
    // slot table is empty) and then fail to reclassify from its raw text --
    // exactly the "revalidation rejected it, and reclassifying it also
    // failed" case the two call sites named in #1830 must not silently drop.
    let hit = CommittedObservationHit::for_unrecoverable_fallback_test(
        EventId::all_zeros(),
        Kind::ContactList.as_u16(),
    );
    reduce_and_dispatch_committed_observations(
        &mut core,
        vec![(handle, session.clone(), RelayFrame::CommittedObservation(hit))],
        &pool,
        &mut row_channels,
        &mut history_channels,
        &mut diag_channels,
        &registry,
        dispatch_runtime,
    );

    assert!(
        !core.returned_frame_count_is_exact_for_test(&session),
        "an unrecoverable committed-observation fallback must erase the session's exact \
         returned-frame count instead of being silently dropped"
    );

    pool.shutdown();
}
