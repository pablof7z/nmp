//! Direct evidence for two things `attach_wire_handle` is responsible for and
//! nothing was asserting.
//!
//! ## Why these exist
//!
//! `attach_wire_handle` indexes a handle's complete atom set *before* retaining
//! any of its atoms. That ordering was changed during the `WireOwnership`
//! extraction and shipped on the strength of "the suite still passes", which
//! proves nothing about an ordering the suite never observed. Retaining an
//! owner can trigger an evidence refresh, and that refresh reads the very
//! reverse indexes attach is in the middle of writing — so a half-attached
//! handle is one whose own atoms are invisible to a refresh its own arrival
//! caused.
//!
//! The second is the reattachment rule the deferred-close machinery exists for:
//! a resolver-reported close that is superseded by a replacement owner in the
//! same turn must never reach the wire. Deleting the line that cancels it left
//! the entire corpus green.

use super::*;

/// A resolver close cancelled by a replacement owner in the same turn must not
/// close anything on the wire, and must leave no deferred close behind a live
/// demand.
///
/// This is the whole reason `pending_resolver_closes` defers instead of closing
/// immediately: close+open of one atom should reattach to retained physical
/// coverage rather than churn the subscription. Deleting
/// `wire.clear_deferred_close` in `retain_wire_atom_owner_with_effects` used to
/// leave every test in the corpus green.
#[test]
fn a_replacement_owner_in_the_same_turn_cancels_a_resolver_close() {
    let relay = RelayUrl::parse("wss://reattach-cancels-close.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);

    let first = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "reattaching",
        Freshness::Live,
    ))));
    flush(&mut core);
    let handle = core.observations[&first].branches[0];
    let atom = core
        .wire
        .atoms_for_handle(handle)
        .into_iter()
        .next()
        .expect("the live observation owns one atom");
    core.assert_owner_consistency("attached");

    // The resolver reports the atom closed. Its demand becomes ownerless and
    // the close is deferred rather than sent.
    core.white_box("consume_resolver_delta", |s| {
        s.consume_resolver_delta(DemandDelta {
            ops: vec![DemandOp::Close(atom.clone())],
        })
    });
    core.assert_owner_consistency("after resolver close");
    // Assert the precondition before the break can make anything observable:
    // this test is worthless unless a close really is deferred here.
    assert_eq!(
        core.wire.deferred_close_count(),
        1,
        "the resolver close was not deferred, so nothing below tests the deferral"
    );

    // A replacement observation claims the same demand before the transaction
    // finishes. That is the case the deferral exists for -- and `Subscribe`
    // drains the deferral itself, so the wire ops it emits are the observable.
    let reopened = core.handle(EngineMsg::Subscribe(query(
        &relay,
        "reattaching",
        Freshness::Live,
    )));
    let second = observation_id(&reopened);
    core.assert_owner_consistency("after replacement attach");
    assert_eq!(
        core.wire.deferred_close_count(),
        0,
        "the replacement turn left a deferred close behind"
    );

    let closes: Vec<_> = wire_ops(&reopened)
        .into_iter()
        .filter(|op| matches!(op, WireOp::Close(_)))
        .collect();
    assert!(
        closes.is_empty(),
        "a demand reattached by a replacement owner was still closed on the wire: {closes:?}"
    );
    core.assert_owner_consistency("after flush");
    assert!(
        core.wire_demand().contains(&atom),
        "the reattached demand left live wire ownership"
    );

    core.handle(EngineMsg::Unsubscribe(first));
    core.handle(EngineMsg::Unsubscribe(second));
    core.assert_owner_consistency("after teardown");
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

/// An arriving handle must be completely indexed before its own atoms are
/// retained, because retaining them can refresh evidence that reads the index.
///
/// The falsifier for the ordering is direct: a handle whose arrival adds a
/// routing fact to a demand an existing handle already owns triggers
/// `refresh_evidence_for_coverage_keys` from inside its own retain loop. If the
/// arriving handle is not yet in `handles_by_coverage`, it is not a refresh
/// candidate, and it silently misses the evidence its own arrival produced.
#[test]
fn an_arriving_handle_is_fully_indexed_before_its_atoms_are_retained() {
    let relay = RelayUrl::parse("wss://attach-ordering.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);

    let incumbent = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "ordering",
        Freshness::Live,
    ))));
    flush(&mut core);
    core.assert_owner_consistency("incumbent attached");
    let incumbent_handle = core.observations[&incumbent].branches[0];
    let atom = core
        .wire
        .atoms_for_handle(incumbent_handle)
        .into_iter()
        .next()
        .expect("the incumbent owns one atom");
    let claim = coverage_key(&atom);
    let demand = DemandKey::for_atom(&atom);

    // A second observation on the same shape arrives. Its retain runs against
    // an index that must already name it.
    let arriving = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "ordering",
        Freshness::Live,
    ))));
    let arriving_handle = core.observations[&arriving].branches[0];

    // The exact property the ordering buys: at the moment the arriving handle's
    // atoms are owned, the handle is already a refresh candidate for every
    // coverage and demand key it owns. Index-after-retain leaves both of these
    // naming only the incumbent.
    assert_eq!(
        core.wire.handles_for_coverage(&claim).cloned(),
        Some(BTreeSet::from([incumbent_handle, arriving_handle])),
        "the arriving handle is not a coverage-refresh candidate for a claim it owns"
    );
    assert_eq!(
        core.wire.handles_for_demand(&demand).cloned(),
        Some(BTreeSet::from([incumbent_handle, arriving_handle])),
        "the arriving handle is not a request-refresh candidate for a demand it owns"
    );
    core.assert_owner_consistency("after arriving attach");

    // And the arrival did not duplicate the incumbent's ownership: one owner
    // per handle per atom, not two.
    assert_eq!(
        core.bench_ownership_census().wire_owner_refs,
        2,
        "two handles owning one demand is exactly two owner refs"
    );

    core.handle(EngineMsg::Unsubscribe(arriving));
    core.assert_owner_consistency("after arriving withdrawal");
    assert!(
        core.wire_demand().contains(&atom),
        "the incumbent lost its demand when its sibling withdrew"
    );

    core.handle(EngineMsg::Unsubscribe(incumbent));
    core.assert_owner_consistency("after teardown");
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

// Indexing one handle twice used to be the reachable hazard tested here:
// `index_handle` self-healed by dropping the old index edges without
// releasing their owner counts. #1774 found double-indexing is not actually
// reachable from any `attach_wire_handle` call site (every one passes a
// freshly minted `HandleId`) and made `index_handle` refuse it outright,
// matching `owner_index.rs::insert`. The falsifier for that refusal now
// lives with the method it falsifies, in `wire_ownership.rs`'s own
// `mod tests` (`index_handle_refuses_a_handle_already_indexed`).
