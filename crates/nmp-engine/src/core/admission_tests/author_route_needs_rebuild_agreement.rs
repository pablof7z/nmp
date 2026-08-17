//! The falsifier `AuthorRouteNeeds`'s wholesale rebuild never had, mirroring
//! `wire_rebuild_agreement.rs` next door -- but proving a different property,
//! because this owner does not have `WireOwnership`'s property.
//!
//! `rebuild_author_outbox_route_needs` (the wholesale path, triggered by
//! every `recompile`) and `retain_author_outbox_wire_owner` /
//! `release_author_outbox_wire_owner` (the incremental path, triggered by
//! every wire attach/detach) both maintain the same owner, but they do not
//! see the same inputs. Both react to attach/detach, and for that input
//! alone they must already agree at every instant -- proved by
//! [`rebuilding_author_route_needs_reproduces_the_incremental_state_exactly`]
//! and [`rebuilding_author_route_needs_twice_changes_nothing`] below. But
//! `AuthorRouteNeeds` also depends on routing facts, which only the rebuild
//! re-reads (see `AuthorRouteNeeds`'s module doc): the incremental path
//! never reconsiders an author's route status while their wire ownership
//! stays live, so it is *expected*, by design, to fall stale exactly there.
//! `recompile` IS a repair pass for that one input -- proved by
//! [`a_route_learned_between_rebuilds_is_reflected_by_the_next_rebuild_only`]
//! below, which drives an author to gain a positive route while their wire
//! ownership never lapses, and checks both halves: that the incremental-only
//! state is genuinely wrong first, and that the rebuild repairs it after.

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

    core.white_box("rebuild_wire_ownership", |s| s.rebuild_wire_ownership());
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
/// `EngineCore::replace_author_routes` is the sole production writer of
/// routing facts, and it always calls `recompile` -- which reaches this
/// owner's rebuild -- synchronously in the same turn a route changes. So
/// production never actually leaves the incremental-only state sitting
/// around observable from outside `EngineCore`; there is no multi-turn
/// staleness window to reach here. What this test does is decompose that
/// one atomic turn into its two halves, using `routing_facts.writer()`
/// directly instead of `replace_author_routes` so the intermediate,
/// incremental-only state can be inspected and asserted on its own, the way
/// `replace_author_routes` never lets a caller observe it. Doing that proves
/// the mechanism `finish_rebuild`'s exact diff exists for: within that one
/// turn, `recompile`'s `flush_author_outbox_route_need_changes` is the only
/// thing that can notice this retirement and publish
/// `Effect::AuthorRouteNeedsChanged` for a subscriber with no open write
/// intent (`rewrite_open_routes` returns early, without resyncing, when
/// nothing is pending) -- so an inexact flag there would silently drop the
/// event on exactly this path. The assertions below prove both halves of
/// the mechanism: the incremental-only half is genuinely wrong on its own
/// (first `assert!`), and the unified rebuild path corrects it (second
/// `assert!`).
#[test]
fn a_route_learned_between_rebuilds_is_reflected_by_the_next_rebuild_only() {
    let author = Keys::generate().public_key();
    let relay = RelayUrl::parse("wss://author-route-needs-rebuild.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);

    core.handle(EngineMsg::Subscribe(routeless_outbox_query(author)));
    flush(&mut core);
    assert!(core.author_outbox_route_needs.needs_set().contains(&author));
    assert_eq!(core.author_outbox_route_needs.wire_owner_count(&author), 1);

    // Isolate the first half of `replace_author_routes`'s one atomic turn:
    // write the fact directly, WITHOUT the `recompile` call
    // `replace_author_routes` always makes in the same turn in production,
    // so the incremental-only state below can be inspected on its own. The
    // author's wire-owner count never lapses to zero and back -- this is a
    // route learned mid-flight, not a departure/re-arrival.
    core.white_box("routing_facts.writer", |s| {
        s.routing_facts.writer().replace(
            author,
            AuthorRouteReplacement::Present(AuthorRoutes::new([relay], [])),
        )
    });
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

    core.white_box("rebuild_wire_ownership", |s| s.rebuild_wire_ownership());
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

    core.white_box("rebuild_wire_ownership", |s| s.rebuild_wire_ownership());
    core.assert_owner_consistency("after one rebuild");
    let once = (
        core.author_outbox_route_needs.wire_owner_count(&author),
        core.author_outbox_route_needs.needs_set(),
    );
    core.white_box("rebuild_wire_ownership", |s| s.rebuild_wire_ownership());
    core.assert_owner_consistency("after two rebuilds");
    let twice = (
        core.author_outbox_route_needs.wire_owner_count(&author),
        core.author_outbox_route_needs.needs_set(),
    );
    assert_eq!(once, twice);
}
