//! M2 contract test 15: `dropped_handle_close_reaches_wire`
//! (`docs/plans/M2-compiler-router-plan.md` §8.2). Discharges M1 nit #2:
//! `QueryHandle::Drop` used to enqueue its withdrawal but the engine
//! discarded the resulting `DemandDelta` (`let _ = ...`), so a dropped
//! handle's CLOSE never reached anything past the resolver. The M2 fix
//! (`Engine::drain_pending_drops` now MERGES the drop into the returned
//! delta; `Engine::poll_pending_drops`/`Harness::poll_pending_drops` flush
//! it even with no other activity) plus the router's full-recompile-then-
//! diff design (a withdrawn atom just vanishes from `active_demand()`)
//! together mean the withdrawal reaches the WIRE as a real `Close`.

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Filter};
use nmp_resolver::testkit::Harness;
use nmp_resolver::LiveQuery;
use nostr::Keys;

use nmp_router::{
    test_relay, DiscoveryKinds, FixtureDirectory, RelayLimits, Router, RuleRegistry, WireOp,
};

fn literal_author_filter(author_hex: &str) -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        ..Filter::default()
    }
}

#[test]
fn dropped_handle_close_reaches_wire() {
    let author = Keys::generate();
    let author_hex = author.public_key().to_hex();

    let dir =
        FixtureDirectory::new().with_write(author_hex.clone(), [test_relay(0), test_relay(1)]);
    let mut router = Router::new(
        RelayLimits::default(),
        DiscoveryKinds::default(),
        RuleRegistry::default_widen_only(),
    );

    let mut h = Harness::new();
    let (handle, _open_delta) = h.subscribe(LiveQuery(literal_author_filter(&author_hex)));

    let demand_open = h.demand();
    assert_eq!(demand_open.len(), 1);
    let opening_delta = router.compile(&demand_open, &dir, 10);
    assert!(
        !opening_delta.ops.is_empty(),
        "the initial compile must open a REQ on the author's write relays"
    );
    let opened_sub_ids: BTreeSet<_> = router
        .plan()
        .reqs
        .values()
        .flatten()
        .map(|req| req.sub_id.clone())
        .collect();
    assert!(!opened_sub_ids.is_empty());

    // Drop the handle -- M1's `Drop` impl enqueues the withdrawal; without
    // another mutating call, nothing has drained it yet.
    drop(handle);

    // Flush the bare drop (M1 nit #2's new seam): the resolver's own
    // returned delta now genuinely carries the Close (previously
    // discarded).
    let drop_delta = h.poll_pending_drops();
    assert!(
        drop_delta
            .closed()
            .iter()
            .any(|cf| cf.authors.as_ref() == Some(&BTreeSet::from([author_hex.clone()]))),
        "the resolver's own delta must surface the dropped atom's Close"
    );

    // The atom is already absent from `active_demand()` -- the router's
    // full-recompile-then-diff design means the NEXT compile emits the
    // withdrawal as a real wire Close, without any special-casing.
    let demand_after_drop = h.demand();
    assert!(demand_after_drop.is_empty());

    let closing_delta = router.compile(&demand_after_drop, &dir, 10);
    let closed_sub_ids: BTreeSet<_> = closing_delta
        .ops
        .iter()
        .flat_map(|(_, ops)| ops.iter())
        .filter_map(|op| match op {
            WireOp::Close(sub_id) => Some(sub_id.clone()),
            WireOp::Req(_, _) => None,
        })
        .collect();
    assert_eq!(
        closed_sub_ids, opened_sub_ids,
        "every previously-opened sub-id must now be closed on the wire"
    );
    assert!(
        router.plan().reqs.is_empty(),
        "no demand left -- the plan is now empty"
    );
}
