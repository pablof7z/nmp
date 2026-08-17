//! The falsifier the two wire-ownership algorithms never had.
//!
//! Live-wire ownership itself is reached two ways: incrementally, as each
//! branch attaches and detaches, and wholesale, every time `recompile`
//! rebuilds it from the current handle set. For `WireOwnership` specifically,
//! `recompile` is not a repair pass — nothing but handle attach/detach can
//! move this owner's state, and nothing marks the incremental state suspect
//! before a rebuild runs — so the two must already agree at every instant.
//! Nothing checked that they did.
//!
//! That "not a repair pass" property does NOT hold for every owner
//! `rebuild_wire_ownership` also rebuilds. `AuthorRouteNeeds`
//! (`author_route_needs.rs`) is reached by a third input besides
//! attach/detach: routing facts, read only at rebuild time. Its incremental
//! path never reconsiders an author once recorded as needing a provider
//! while their wire ownership persists, so a route learned mid-flight leaves
//! it stale on purpose until the next rebuild repairs it — see that module's
//! doc and `admission_tests/author_route_needs_rebuild_agreement.rs` for the
//! falsifier proving exactly that repair. The census below did not cover
//! `AuthorRouteNeeds` at all until that owner was extracted (#1758); it does
//! now, but a rebuild that merely reaches the SAME quantities is a much
//! weaker claim for this one owner than "agrees with the incremental path",
//! precisely because a rebuild is allowed -- expected -- to correct it.
//!
//! These assert wire ownership's agreement directly: reach a state
//! incrementally, rebuild, and demand the result be identical — both as an
//! exact structure and as a census.
//!
//! The two checks answer different questions and neither replaces the other.
//! `assert_owner_consistency` proves each owner's mirrors are internally exact
//! after the rebuild, catching an association that moved to the wrong key. The
//! census comparison proves the rebuild reached the same *quantities* as the
//! incremental path, catching a demand that disappeared entirely. A rebuild
//! that forgets a map fails the second; a rebuild that misfiles one fails the
//! first.

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

    core.assert_owner_consistency("incremental");
    let incremental = core.bench_ownership_census();
    core.rebuild_wire_ownership();
    core.assert_owner_consistency("rebuilt");
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

    core.assert_owner_consistency("incremental after withdrawal");
    let incremental = core.bench_ownership_census();
    core.rebuild_wire_ownership();
    core.assert_owner_consistency("rebuilt after withdrawal");
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
    core.assert_owner_consistency("after one rebuild");
    let once = core.bench_ownership_census();
    core.rebuild_wire_ownership();
    core.assert_owner_consistency("after two rebuilds");
    assert_eq!(core.bench_ownership_census(), once);
}
