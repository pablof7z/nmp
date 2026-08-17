//! The falsifier `AuthorRouteNeeds`'s wholesale rebuild never had, mirroring
//! `wire_rebuild_agreement.rs` next door.
//!
//! `rebuild_author_outbox_route_needs` (the wholesale path, triggered by
//! every `recompile`) and `retain_author_outbox_wire_owner` /
//! `release_author_outbox_wire_owner` (the incremental path, triggered by
//! every wire attach/detach) both maintain the same owner. They must already
//! agree at every instant a rebuild can run -- `recompile` is not a repair
//! pass, nothing marks the incremental state suspect first. Nothing checked
//! that they did.
//!
//! This exercises the one case `wire_rebuild_agreement.rs` cannot: an author
//! whose route turns positive *between* rebuilds, which the incremental path
//! never notices (see `AuthorRouteNeeds`'s module doc) but a rebuild must.

use super::*;

/// A rebuild over state built entirely by the incremental path must be a
/// no-op: same per-author wire-owner counts, same need set.
#[test]
fn rebuilding_author_route_needs_reproduces_the_incremental_state_exactly() {
    let shared = Keys::generate().public_key();
    let distinct = Keys::generate().public_key();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);

    // Two branches sharing one author's AuthorOutboxes demand (owner count
    // two) plus a distinct author on its own (owner count one).
    core.handle(EngineMsg::Subscribe(routeless_outbox_query(shared)));
    core.handle(EngineMsg::Subscribe(routeless_outbox_query(shared)));
    core.handle(EngineMsg::Subscribe(routeless_outbox_query(distinct)));
    flush(&mut core);

    core.assert_owner_consistency("incremental");
    let before = (
        core.author_outbox_route_needs.wire_owner_count(&shared),
        core.author_outbox_route_needs.wire_owner_count(&distinct),
        core.author_outbox_route_needs.needs_set(),
    );
    assert_eq!(before.0, 2, "the shared author has two live wire owners");
    assert_eq!(before.1, 1, "the distinct author has one live wire owner");
    assert!(before.2.contains(&shared) && before.2.contains(&distinct));

    core.rebuild_wire_ownership();
    core.assert_owner_consistency("rebuilt");
    let after = (
        core.author_outbox_route_needs.wire_owner_count(&shared),
        core.author_outbox_route_needs.wire_owner_count(&distinct),
        core.author_outbox_route_needs.needs_set(),
    );
    assert_eq!(
        after, before,
        "the wholesale rebuild disagrees with the incremental path it replaces"
    );
}

/// The exact case the incremental path cannot notice on its own: a route
/// learned while an author's wire ownership never lapses. Only a rebuild
/// picks this up (see `AuthorRouteNeeds`'s module doc); this proves the
/// coordinator's rebuild call actually reaches it end to end, not just at
/// the owner's own unit-test level.
///
/// This is a real, reachable divergence between the two paths, not a
/// fabricated one: `release` only ever drops an author from `needs` when
/// its wire-owner count reaches zero, and `retain` only ever reconsiders
/// route status on the zero-to-one transition. Neither runs when a route is
/// learned mid-flight with the owner count staying nonzero throughout, so
/// the incremental-only state is genuinely stale -- wrong, not merely
/// theoretically inconsistent -- until the next rebuild repairs it. The
/// assertions below prove both halves: the staleness is real (first
/// `assert!`), and the unified rebuild path corrects it (second `assert!`).
#[test]
fn a_route_learned_between_rebuilds_is_reflected_by_the_next_rebuild_only() {
    let author = Keys::generate().public_key();
    let relay = RelayUrl::parse("wss://author-route-needs-rebuild.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);

    core.handle(EngineMsg::Subscribe(routeless_outbox_query(author)));
    flush(&mut core);
    assert!(core.author_outbox_route_needs.needs_set().contains(&author));
    assert_eq!(core.author_outbox_route_needs.wire_owner_count(&author), 1);

    // Learn a positive route WITHOUT going through `replace_author_routes`
    // (which would itself trigger `recompile`): write the fact directly, so
    // the incremental wire-owner state is provably untouched by this step,
    // and the author's wire-owner count never lapses to zero and back.
    core.routing_facts.writer().replace(
        author,
        AuthorRouteReplacement::Present(AuthorRoutes::new([relay], [])),
    );
    assert_eq!(
        core.author_outbox_route_needs.wire_owner_count(&author),
        1,
        "the owner count must stay nonzero across the whole scenario -- this is not \
         a departure/re-arrival, it is a route learned mid-flight"
    );
    assert!(
        core.author_outbox_route_needs.needs_set().contains(&author),
        "the incremental-only state is now genuinely WRONG: the author has a positive \
         route but is still recorded as needing a provider, because nothing in the \
         incremental path re-examines route status once wire ownership is live"
    );

    core.rebuild_wire_ownership();
    core.assert_owner_consistency("rebuilt after learning a route");
    assert_eq!(
        core.author_outbox_route_needs.wire_owner_count(&author),
        1,
        "the rebuild must not touch wire ownership itself, only the need it implies"
    );
    assert!(
        !core.author_outbox_route_needs.needs_set().contains(&author),
        "the unified rebuild path (reset_for_rebuild + retain replay) must repair the \
         staleness the incremental path left behind"
    );
}

/// Idempotence: rebuilding twice must reach the same state as rebuilding
/// once.
#[test]
fn rebuilding_author_route_needs_twice_changes_nothing() {
    let author = Keys::generate().public_key();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    core.handle(EngineMsg::Subscribe(routeless_outbox_query(author)));
    flush(&mut core);

    core.rebuild_wire_ownership();
    core.assert_owner_consistency("after one rebuild");
    let once = (
        core.author_outbox_route_needs.wire_owner_count(&author),
        core.author_outbox_route_needs.needs_set(),
    );
    core.rebuild_wire_ownership();
    core.assert_owner_consistency("after two rebuilds");
    let twice = (
        core.author_outbox_route_needs.wire_owner_count(&author),
        core.author_outbox_route_needs.needs_set(),
    );
    assert_eq!(once, twice);
}
