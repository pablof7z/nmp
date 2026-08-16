//! The falsifier the three hand-written reverse indexes never had.
//!
//! Every NIP-77 child map is mirrored by an index from the plan that owns it.
//! Six functions used to maintain those mirrors by hand, and three more sites
//! removed from both maps in the other direction. Nothing checked that a
//! forward entry and a reverse edge ever agreed — the census reports the two
//! counts, and no test compared them.
//!
//! `plan_edges == len` is the whole invariant: every child is named by exactly
//! one plan's set, so the total number of edges is the number of children.

use super::*;

/// Assert every NIP-77 index mirrors its forward map exactly.
fn assert_mirrors(core: &EngineCore, at: &str) {
    let census = core.bench_ownership_census();
    assert_eq!(
        census.pending_neg_plan_edges, census.pending_neg_handoffs,
        "{at}: handoff reverse index does not mirror its forward map"
    );
    assert_eq!(
        census.neg_session_plan_edges, census.neg_sessions,
        "{at}: reconciliation reverse index does not mirror its forward map"
    );
    assert_eq!(
        census.pending_backfill_plan_edges, census.pending_backfills,
        "{at}: backfill reverse index does not mirror its forward map"
    );
}

/// One plan's children arrive and leave through the ordinary lifecycle. The
/// mirrors have to hold at every step, not only at rest.
#[test]
fn nip77_plan_indexes_mirror_their_forward_maps_through_open_and_close() {
    const PLANS: u16 = 8;
    let relay = RelayUrl::parse("wss://plan-index-mirror.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    core.prober
        .states
        .insert(relay.clone(), crate::negentropy::ProbeState::Supported);
    assert_mirrors(&core, "empty");

    let mut observations = Vec::with_capacity(PLANS as usize);
    for index in 0..PLANS {
        observations.push(observation_id(&core.handle(EngineMsg::Subscribe(
            unbounded_incompatible_query(&relay, index),
        ))));
        assert_mirrors(&core, "after subscribe");
    }

    flush(&mut core);
    assert_eq!(core.nip77.handoffs.len(), PLANS as usize);
    assert_mirrors(&core, "after admission");

    for observation in observations {
        core.handle(EngineMsg::Unsubscribe(observation));
        assert_mirrors(&core, "after unsubscribe");
    }
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

/// A relay whose connection generation dies takes its repair children with it.
/// That path removes by predicate rather than by plan, which is the third
/// removal direction and the one most likely to forget a mirror.
#[test]
fn losing_a_relay_generation_leaves_no_orphaned_plan_edges() {
    const PLANS: u16 = 4;
    let relay = RelayUrl::parse("wss://plan-index-mirror-disconnect.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    core.prober
        .states
        .insert(relay.clone(), crate::negentropy::ProbeState::Supported);
    let handle = TransportRelayHandle {
        slot: 17,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));

    let mut observations = Vec::with_capacity(PLANS as usize);
    for index in 0..PLANS {
        observations.push(observation_id(&core.handle(EngineMsg::Subscribe(
            unbounded_incompatible_query(&relay, index),
        ))));
    }
    flush(&mut core);
    assert_eq!(core.nip77.handoffs.len(), PLANS as usize);
    assert_mirrors(&core, "before disconnect");

    core.handle(EngineMsg::RelayDisconnected(
        handle,
        session.clone(),
        DisconnectReason::Error,
    ));
    assert_mirrors(&core, "after disconnect");
    assert!(
        core.nip77.handoffs.is_empty(),
        "the dead generation's handoffs survived its disconnect"
    );

    for observation in observations {
        core.handle(EngineMsg::Unsubscribe(observation));
    }
    assert_mirrors(&core, "after teardown");
}
