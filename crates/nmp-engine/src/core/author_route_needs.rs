//! Typed ownership for the neutral author-outbox provider need: which
//! authors currently have live `AuthorOutboxes` wire demand, and which of
//! those still lack a positive outbound route.
//!
//! Three fields used to sit directly on `CoreState`, maintained by *two*
//! hand-written algorithms nothing checked against each other: an
//! incremental path (`retain_author_outbox_wire_owner` /
//! `release_author_outbox_wire_owner`) and a wholesale
//! `rebuild_author_outbox_route_needs` that open-coded the owner counting a
//! second time -- correct only because a caller in a different file cleared
//! the counts map first. The refcount also absorbed under/overflow with
//! `saturating_add`/`saturating_sub` while [`wire_ownership`](super::wire_ownership)
//! next door panics on the identical shape of bug.
//!
//! Here there is one implementation. [`AuthorRouteNeeds::retain`] is the only
//! place an author's wire-owner count can increase,
//! [`AuthorRouteNeeds::release`] the only place it can decrease, and
//! [`AuthorRouteNeeds::reset_for_rebuild`] plus a replay through `retain` is
//! the only rebuild -- so the counts map cannot be forgotten by a reset that
//! does not mention it, and the two algorithms cannot drift because there is
//! only one.
//!
//! ## What this owner deliberately does not know
//!
//! Whether an author currently has a positive outbound route is a
//! `RoutingFactStore` question. This owner never reads routing facts itself;
//! the coordinator passes the answer in at [`AuthorRouteNeeds::retain`], the
//! one moment the incremental path can act on it (an author's count going
//! from zero live owners to one). Once an author is recorded as needing a
//! provider, the *incremental* path never reconsiders that record while the
//! author keeps a live owner -- a route learned later does not erase the
//! need mid-flight through `retain`/`release` alone. Only a wholesale
//! rebuild reconsiders every live author against the routing facts of the
//! moment, because it replays `retain` from empty: an author who now has a
//! positive route is not re-inserted, and so drops out of the rebuilt need
//! set even though their wire ownership never lapsed. This is existing
//! behavior this owner preserves exactly, not a new invariant introduced
//! here.
//!
//! This is not an open-ended staleness window in production today: the
//! coordinator's sole production writer of routing facts,
//! `CoreState::replace_author_routes`, always calls `recompile` --
//! and therefore this owner's rebuild -- synchronously in the same turn a
//! route changes, so nothing outside `CoreState` ever observes the
//! incremental-only state. What the two-path split *does* make load-bearing
//! is entirely inside that one turn: `recompile`'s own
//! `flush_author_outbox_route_need_changes` is the only mechanism that can
//! notice a route-driven retirement and publish `Effect::AuthorRouteNeedsChanged`
//! when there is no open write intent for `rewrite_open_routes` to walk (that
//! function returns early, and does not resync, when nothing is pending) --
//! see [`AuthorRouteNeeds::finish_rebuild`] below for why that gate needs an
//! exact answer.
//!
//! Likewise, which live-wire atoms even name an author for `AuthorOutboxes`
//! demand is a `ContextualAtom` question the coordinator answers before
//! calling in; this owner works in plain `PublicKey`s.
//!
//! ## One refcount rule
//!
//! A wire owner count that goes negative is a bug in the caller, not a state
//! this owner should absorb. `release` requires the author to already be
//! counted and panics naming the invariant otherwise, matching
//! `wire_ownership.rs`'s `release_ref`.
//!
//! ## The pending-change flag
//!
//! `changed` used to be set at two sites in `write.rs` and one in `query.rs`,
//! read in `write.rs`, and cleared by a *different* function than the one
//! that read it -- a flag "maintained by consensus among callers" is exactly
//! the shape this extraction exists to remove.
//! [`AuthorRouteNeeds::take_pending_change`] returns and clears it as one
//! transition, so there is no window where it is true-but-unread from
//! outside this owner.
//!
//! A rebuild cannot derive the flag from what `retain` touches during replay
//! alone: an author whose route just turned positive is *removed* from
//! `needs` by a rebuild without a single `retain`/`release` call ever
//! mentioning them (see above), so nothing during replay would set `changed`
//! for exactly the transition that matters. [`AuthorRouteNeeds::finish_rebuild`]
//! instead compares the need set after replay against the set
//! [`AuthorRouteNeeds::reset_for_rebuild`] returned before it, and assigns
//! the flag from that exact difference -- overwriting whatever replay noise
//! `retain` produced along the way, not merely OR-ing into it.

use std::collections::{BTreeMap, BTreeSet};

use nostr::PublicKey;

/// The census contribution, so the root counts this owner's state without
/// naming its maps. `wire_rebuild_agreement.rs` compares this census across
/// a rebuild the same way it compares `WireOwnership`'s; without these
/// fields that comparison is blind to everything this owner holds.
#[cfg(any(test, feature = "bench-instrumentation"))]
pub(super) struct AuthorRouteNeedsCounts {
    pub(super) wire_owner_keys: usize,
    pub(super) wire_owner_refs: usize,
    pub(super) needs: usize,
}

/// Exact live-wire owner count per author contributed by `AuthorOutboxes`
/// demand, and the subset of those authors still needing a neutral provider.
#[derive(Default)]
pub(super) struct AuthorRouteNeeds {
    /// Every author with at least one live `AuthorOutboxes` wire owner, and
    /// exactly how many. Never holds a zero entry.
    wire_owner_counts: BTreeMap<PublicKey, usize>,
    /// Authors with live `AuthorOutboxes` demand and, as of the moment their
    /// count first went from zero to one, no positive outbound route. A
    /// subset of `wire_owner_counts`'s keys.
    needs: BTreeSet<PublicKey>,
    /// Whether the live need set may have changed since the last check.
    changed: bool,
}

impl AuthorRouteNeeds {
    /// Add one live-wire owner of `author`'s `AuthorOutboxes` demand.
    ///
    /// `has_positive_outbox` is consulted only on the zero-to-one transition:
    /// a continuing owner cannot change whether `author` was already
    /// recorded as needing a provider, because nothing removes that record
    /// except the last owner departing (see module doc).
    pub(super) fn retain(&mut self, author: PublicKey, has_positive_outbox: bool) {
        let count = self.wire_owner_counts.entry(author).or_insert(0);
        *count = count
            .checked_add(1)
            .expect("author-outbox wire owner count cannot overflow");
        if *count == 1 && !has_positive_outbox && self.needs.insert(author) {
            self.changed = true;
        }
    }

    /// Remove one live-wire owner of `author`'s `AuthorOutboxes` demand.
    ///
    /// `author` must already be counted -- a release with nothing to release
    /// is a bug in the caller (an atom's authors are a pure function of the
    /// atom, so every release corresponds to an earlier retain of the same
    /// atom), not a state to absorb silently.
    pub(super) fn release(&mut self, author: PublicKey) {
        let count = self.wire_owner_counts.get_mut(&author).unwrap_or_else(|| {
            panic!("a released author-outbox owner has a wire owner count for {author}")
        });
        *count = count
            .checked_sub(1)
            .expect("author-outbox wire owner count cannot underflow");
        if *count == 0 {
            self.wire_owner_counts.remove(&author);
            if self.needs.remove(&author) {
                self.changed = true;
            }
        }
    }

    /// Every author currently needing a neutral provider.
    pub(super) fn needs(&self) -> impl Iterator<Item = PublicKey> + '_ {
        self.needs.iter().copied()
    }

    /// Return whether the need set changed since the last check, and clear
    /// the flag as one transition. `#[must_use]`: a caller that clears this
    /// without acting on the result has reintroduced the "maintained by
    /// consensus" shape this owner exists to remove.
    #[must_use]
    pub(super) fn take_pending_change(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// Begin a wholesale rebuild: reset to empty and return the need set
    /// held immediately before the reset, for [`Self::finish_rebuild`] to
    /// compare against once the caller has replayed every current
    /// contribution through [`Self::retain`].
    pub(super) fn reset_for_rebuild(&mut self) -> BTreeSet<PublicKey> {
        std::mem::replace(self, Self::default()).needs
    }

    /// End a wholesale rebuild: set the pending-change flag from the exact
    /// difference between the need set now and the one
    /// [`Self::reset_for_rebuild`] returned, discarding whatever `retain`
    /// set the flag to along the way. See the module doc for why a rebuild
    /// cannot trust replay-time flag writes on their own.
    pub(super) fn finish_rebuild(&mut self, needs_before_rebuild: BTreeSet<PublicKey>) {
        self.changed = self.needs != needs_before_rebuild;
    }

    /// Exact structural consistency.
    ///
    /// `wire_owner_counts` is the canonical relation: no entry may be zero
    /// (release always removes a count that reaches zero rather than leaving
    /// it behind), and `needs` -- which also depends on routing facts this
    /// owner never stores -- must still be an exact subset of it by
    /// identity. A cardinality-preserving corruption that swaps which author
    /// is recorded as needing a provider (same `needs.len()`, wrong member)
    /// fails the second assertion even though every count and every set size
    /// stays right.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn assert_consistent(&self, at: &str) {
        for (author, count) in &self.wire_owner_counts {
            assert!(
                *count > 0,
                "{at}: author-outbox wire owner count for {author} is present but zero"
            );
        }
        for author in &self.needs {
            assert!(
                self.wire_owner_counts.contains_key(author),
                "{at}: {author} is recorded as needing a provider route with no live \
                 author-outbox wire owner"
            );
        }
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn counts(&self) -> AuthorRouteNeedsCounts {
        AuthorRouteNeedsCounts {
            wire_owner_keys: self.wire_owner_counts.len(),
            wire_owner_refs: self.wire_owner_counts.values().sum(),
            needs: self.needs.len(),
        }
    }
}

/// The reads the admission proofs need, as questions rather than maps.
#[cfg(test)]
impl AuthorRouteNeeds {
    pub(super) fn wire_owner_count(&self, author: &PublicKey) -> usize {
        self.wire_owner_counts
            .get(author)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn needs_set(&self) -> BTreeSet<PublicKey> {
        self.needs.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    #[test]
    fn a_continuing_owner_does_not_reconsider_route_status() {
        let mut owner = AuthorRouteNeeds::default();
        let alice = author();
        owner.retain(alice, false);
        assert!(owner.needs_set().contains(&alice));
        // A second retain with a positive route must not remove the need:
        // only the last release does.
        owner.retain(alice, true);
        assert!(owner.needs_set().contains(&alice));
        assert_eq!(owner.wire_owner_count(&alice), 2);
    }

    #[test]
    fn the_last_release_clears_the_need_and_the_count() {
        let mut owner = AuthorRouteNeeds::default();
        let alice = author();
        owner.retain(alice, false);
        owner.retain(alice, false);
        let _ = owner.take_pending_change();
        owner.release(alice);
        assert!(owner.needs_set().contains(&alice), "one owner remains");
        assert!(!owner.take_pending_change());
        owner.release(alice);
        assert!(!owner.needs_set().contains(&alice));
        assert_eq!(owner.wire_owner_count(&alice), 0);
        assert!(owner.take_pending_change());
    }

    #[test]
    fn take_pending_change_clears_as_one_transition() {
        let mut owner = AuthorRouteNeeds::default();
        owner.retain(author(), false);
        assert!(owner.take_pending_change());
        assert!(!owner.take_pending_change());
    }

    /// The reachable over-release panic: once the last owner has departed,
    /// the entry is gone (see `release`'s "never holds a zero entry"
    /// invariant), so a further release hits the presence check, not the
    /// `checked_sub` underflow beneath it. Given the two mutating doors here
    /// (`retain`/`release`) alone, `checked_sub` underflowing is not
    /// reachable -- the presence check always fires first, exactly as
    /// `wire_ownership.rs`'s analogous `release_ref` is structured. The
    /// `checked_sub` stays as the same defense-in-depth that file uses, not
    /// because a test can reach it today.
    #[test]
    #[should_panic(expected = "has a wire owner count for")]
    fn releasing_more_than_retained_panics() {
        let mut owner = AuthorRouteNeeds::default();
        let alice = author();
        owner.retain(alice, false);
        owner.release(alice);
        owner.release(alice);
    }

    #[test]
    #[should_panic(expected = "has a wire owner count for")]
    fn releasing_an_author_never_retained_panics() {
        let mut owner = AuthorRouteNeeds::default();
        owner.release(author());
    }

    /// The exact bug this design has to avoid: a rebuild must notice an
    /// author *leaving* the need set even though no `retain`/`release` call
    /// during replay ever mentions them for that reason (a positive route
    /// simply stops `retain` from re-inserting them). A first version of
    /// this owner derived the flag from replay-time `retain` calls alone and
    /// missed exactly this case -- caught by
    /// `core::query_tests::author_outbox_queries_need_a_provider_until_a_positive_route_or_withdrawal`
    /// at the `CoreState` level, reproduced here at the owner level.
    #[test]
    fn a_rebuild_notices_a_route_learned_between_rebuilds() {
        let mut owner = AuthorRouteNeeds::default();
        let alice = author();
        owner.retain(alice, false);
        assert!(owner.take_pending_change());

        // Rebuild with the SAME live owner, but the route is positive now.
        let previous = owner.reset_for_rebuild();
        owner.retain(alice, true);
        owner.finish_rebuild(previous);

        assert!(
            !owner.needs_set().contains(&alice),
            "a rebuild must drop an author whose route turned positive"
        );
        assert!(
            owner.take_pending_change(),
            "the rebuild must report the need set changed, even though no \
             retain/release call during replay targeted `needs` for it"
        );
    }

    /// The mirror image: an idempotent rebuild (nothing learned, same
    /// owners) must NOT report a change, even though every author passes
    /// through a synthetic zero-to-one `retain` during replay.
    #[test]
    fn an_idempotent_rebuild_reports_no_change() {
        let mut owner = AuthorRouteNeeds::default();
        let alice = author();
        owner.retain(alice, false);
        let _ = owner.take_pending_change();

        let previous = owner.reset_for_rebuild();
        owner.retain(alice, false);
        owner.finish_rebuild(previous);

        assert!(owner.needs_set().contains(&alice));
        assert!(
            !owner.take_pending_change(),
            "an idempotent rebuild must not report a change"
        );
    }

    #[test]
    #[should_panic(expected = "is recorded as needing a provider route with no live")]
    fn assert_consistent_catches_a_cardinality_preserving_swap() {
        let mut owner = AuthorRouteNeeds::default();
        let alice = author();
        let bob = author();
        owner.retain(alice, false);
        owner.retain(bob, true);
        owner.assert_consistent("before corruption");
        // Corrupt `needs` directly: swap alice out for an author with no
        // live wire owner at all. `needs.len()` is unchanged (still one),
        // so a count-only check would miss this.
        owner.needs.remove(&alice);
        owner.needs.insert(author());
        owner.assert_consistent("after corruption");
    }
}
