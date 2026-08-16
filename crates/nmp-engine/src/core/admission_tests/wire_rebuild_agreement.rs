//! The falsifier the two wire-ownership algorithms never had.
//!
//! Live-wire ownership is reached two ways: incrementally, as each branch
//! attaches and detaches, and wholesale, every time `recompile` rebuilds it
//! from the current handle set. `recompile` is not a repair pass — nothing
//! marks the incremental state suspect before it runs — so the two must
//! already agree at every instant. Nothing checked that they did.
//!
//! These assert it directly: reach a state incrementally, rebuild, and demand
//! the ownership census be bit-identical. A rebuild that forgets a map, or an
//! incremental path that stops maintaining one, is red here.

use super::*;

/// One rebuild over a state built entirely by the incremental path must be a
/// no-op.
#[test]
fn rebuilding_wire_ownership_reproduces_the_incremental_state_exactly() {
    let relay = RelayUrl::parse("wss://wire-rebuild-agreement.example").unwrap();
    let other = RelayUrl::parse("wss://wire-rebuild-agreement-two.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);

    // Three shapes that exercise different corners of the owner: two
    // observations sharing one demand (owner count above one), a distinct
    // demand on the same relay, and a bounded query on another relay.
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "shared",
        Freshness::Live,
    )));
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "shared",
        Freshness::Live,
    )));
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "distinct",
        Freshness::Live,
    )));
    core.handle(EngineMsg::Subscribe(bounded_query(&other, "bounded")));
    flush(&mut core);

    let incremental = core.bench_ownership_census();
    core.rebuild_wire_ownership();
    assert_eq!(
        core.bench_ownership_census(),
        incremental,
        "the wholesale rebuild disagrees with the incremental path it replaces"
    );
}

/// The same equality has to survive teardown, which is where a rebuild that
/// clears more than it repopulates would otherwise hide behind live state.
#[test]
fn rebuilding_after_partial_teardown_still_reproduces_the_incremental_state() {
    let relay = RelayUrl::parse("wss://wire-rebuild-teardown.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let first = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "shared",
        Freshness::Live,
    ))));
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "shared",
        Freshness::Live,
    )));
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "distinct",
        Freshness::Live,
    )));
    flush(&mut core);

    core.handle(EngineMsg::Unsubscribe(first));
    flush(&mut core);

    let incremental = core.bench_ownership_census();
    core.rebuild_wire_ownership();
    assert_eq!(
        core.bench_ownership_census(),
        incremental,
        "the rebuild disagrees with the incremental path after a withdrawal"
    );
}

/// A rebuild is idempotent. Running it twice must reach the same state as
/// running it once — the property the twelve hand-written `.clear()` calls
/// used to be responsible for and could not enforce.
#[test]
fn rebuilding_wire_ownership_twice_changes_nothing() {
    let relay = RelayUrl::parse("wss://wire-rebuild-idempotent.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "shared",
        Freshness::Live,
    )));
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "shared",
        Freshness::Live,
    )));
    flush(&mut core);

    core.rebuild_wire_ownership();
    let once = core.bench_ownership_census();
    core.rebuild_wire_ownership();
    assert_eq!(core.bench_ownership_census(), once);
}
