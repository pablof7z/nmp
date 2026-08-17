//! The mirrored-index mechanism `nip77_sessions.rs` introduced, generalized
//! over its owner key type (#1606).
//!
//! `PlanIndexed` mirrored a map of children keyed by their own wire id against
//! a reverse index from the router plan (a [`nmp_router::SubId`]) that owned
//! them. Request replacements need the identical shape — a forward map of
//! successors keyed by wire id, a reverse index from the thing that owns the
//! transition — except the owner is a session (a `RelaySessionKey`), not a
//! plan. Insert, take, and owner-scoped teardown all follow the same rules in
//! both places: a duplicate child id is refused, and once the forward map
//! says a child existed, its reverse edge is REQUIRED to exist. Two owners,
//! one mechanism, disagreeing only on what type names the owner.
//!
//! What stays out of here on purpose: every name in this file is about the
//! index, not about NIP-77 or about replacements. `Nip77Sessions` and
//! `RequestReplacements` each keep their own vocabulary in their own
//! lifecycle methods; this module supplies the plumbing underneath both, and
//! must never grow a "plan" or a "successor" of its own.

use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;

/// A value that is owned by exactly one key of type `Owner`.
pub(super) trait IndexedChild<Owner> {
    fn owner_key(&self) -> &Owner;
}

/// Children keyed by their own wire id, with the reverse index from the
/// owner that holds them maintained as a consequence rather than by every
/// caller.
///
/// Both maps are private. There is no spelling of "insert into one and forget
/// the other", in either removal direction. `what` names the owning cluster
/// for panic text, so a broken mirror in a replacement session and a broken
/// mirror in a NIP-77 plan fail with different words despite sharing this
/// code.
pub(super) struct OwnerIndexed<Owner, Child, V> {
    what: &'static str,
    by_child: HashMap<Child, V>,
    by_owner: HashMap<Owner, BTreeSet<Child>>,
}

impl<Owner, Child, V> OwnerIndexed<Owner, Child, V> {
    pub(super) fn new(what: &'static str) -> Self {
        Self {
            what,
            by_child: HashMap::new(),
            by_owner: HashMap::new(),
        }
    }
}

impl<Owner, Child, V> OwnerIndexed<Owner, Child, V>
where
    Owner: Clone + Eq + Hash,
    Child: Clone + Eq + Hash + Ord,
    V: IndexedChild<Owner>,
{
    /// Index one new child under the owner that holds it.
    ///
    /// A duplicate child id is refused rather than replaced. Every id here is
    /// freshly minted, so re-inserting one is not a supported transition —
    /// and the permissive spelling was actively unsafe: it added the new
    /// reverse edge, overwrote the forward value, and left the OLD owner
    /// still naming the child. One child in two reverse sets, silently.
    pub(super) fn insert(&mut self, child: Child, value: V) {
        assert!(
            !self.by_child.contains_key(&child),
            "{}: a child id was reused while still live",
            self.what
        );
        self.by_owner
            .entry(value.owner_key().clone())
            .or_default()
            .insert(child.clone());
        self.by_child.insert(child, value);
    }

    /// Remove one child and prune its owner's set.
    ///
    /// Absent is a valid answer for the CHILD -- callers legitimately probe
    /// for one that has already gone. It is not a valid answer for its
    /// owner's set: once the forward map says the child existed, the reverse
    /// edge is required to exist too, and used to be allowed to be missing.
    pub(super) fn take(&mut self, child: &Child) -> Option<V> {
        let value = self.by_child.remove(child)?;
        let owner = value.owner_key().clone();
        let children = self.by_owner.get_mut(&owner).unwrap_or_else(|| {
            panic!(
                "{}: a live child's owner has no reverse-index set",
                self.what
            )
        });
        assert!(
            children.remove(child),
            "{}: a live child's owner did not name it in the reverse index",
            self.what
        );
        if children.is_empty() {
            self.by_owner.remove(&owner);
        }
        Some(value)
    }

    /// Remove every child of one owner. The returned order is the reverse
    /// index's own, which is stable because `by_owner`'s sets are ordered.
    pub(super) fn take_owner(&mut self, owner: &Owner) -> Vec<(Child, V)> {
        let children = self.by_owner.remove(owner).unwrap_or_default();
        children
            .into_iter()
            .map(|child| {
                // A reverse edge naming an absent child used to disappear
                // quietly here (`filter_map`). That is the mirror being
                // broken, and it should say so where it broke.
                let value = self.by_child.remove(&child).unwrap_or_else(|| {
                    panic!(
                        "{}: an owner's reverse index names a child that is not live",
                        self.what
                    )
                });
                (child, value)
            })
            .collect()
    }

    /// Remove every child matching `drop`, pruning the reverse index for
    /// each, and hand back what was removed.
    pub(super) fn take_where<F: Fn(&Child, &V) -> bool>(&mut self, drop: F) -> Vec<(Child, V)> {
        let departing: Vec<_> = self
            .by_child
            .iter()
            .filter(|(child, value)| drop(child, value))
            .map(|(child, _)| child.clone())
            .collect();
        departing
            .into_iter()
            .filter_map(|child| self.take(&child).map(|value| (child, value)))
            .collect()
    }

    pub(super) fn get(&self, child: &Child) -> Option<&V> {
        self.by_child.get(child)
    }

    pub(super) fn get_mut(&mut self, child: &Child) -> Option<&mut V> {
        self.by_child.get_mut(child)
    }

    pub(super) fn contains(&self, child: &Child) -> bool {
        self.by_child.contains_key(child)
    }

    pub(super) fn children_of(&self, owner: &Owner) -> BTreeSet<Child> {
        self.by_owner.get(owner).cloned().unwrap_or_default()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&Child, &V)> {
        self.by_child.iter()
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn len(&self) -> usize {
        self.by_child.len()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.by_child.is_empty() && self.by_owner.is_empty()
    }

    /// Test-only: swap which owner's reverse set names which owner's
    /// children, touching `by_owner` alone -- the forward map, `len`,
    /// `owner_keys`, and `owner_edges` are all unchanged by this call. This
    /// is the exact cardinality-preserving corruption `assert_consistent`
    /// exists to catch, reachable only from inside this module (both maps
    /// are private everywhere else) or through this door, which exists
    /// solely so a falsifier elsewhere in `core` can drive it without
    /// duplicating this module's own field access.
    #[cfg(test)]
    pub(super) fn swap_owners_for_test(&mut self, a: &Owner, b: &Owner) {
        let a_children = self
            .by_owner
            .remove(a)
            .expect("swap_owners_for_test: owner `a` has no live children");
        let b_children = self
            .by_owner
            .remove(b)
            .expect("swap_owners_for_test: owner `b` has no live children");
        self.by_owner.insert(a.clone(), b_children);
        self.by_owner.insert(b.clone(), a_children);
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn owner_keys(&self) -> usize {
        self.by_owner.len()
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn owner_edges(&self) -> usize {
        self.by_owner.values().map(BTreeSet::len).sum()
    }
}

/// Exact structural consistency for one mirrored index.
///
/// Both directions, by identity rather than by count. `owner_edges == len` is
/// necessary and nowhere near sufficient: one child indexed under the wrong
/// owner preserves both numbers exactly.
#[cfg(any(test, feature = "bench-instrumentation"))]
impl<Owner, Child, V> OwnerIndexed<Owner, Child, V>
where
    Owner: Clone + Eq + Hash + std::fmt::Debug,
    Child: Clone + Eq + Hash + Ord + std::fmt::Debug,
    V: IndexedChild<Owner>,
{
    pub(super) fn assert_consistent(&self, at: &str) {
        for (child, value) in &self.by_child {
            let owner = value.owner_key();
            let children = self.by_owner.get(owner).unwrap_or_else(|| {
                panic!(
                    "{at}: {} child {child:?} has no reverse set for its own owner",
                    self.what
                )
            });
            assert!(
                children.contains(child),
                "{at}: {} child {child:?} is not named by the owner it reports",
                self.what
            );
        }
        for (owner, children) in &self.by_owner {
            assert!(
                !children.is_empty(),
                "{at}: {} kept an empty reverse set for owner {owner:?}",
                self.what
            );
            for child in children {
                let value = self.by_child.get(child).unwrap_or_else(|| {
                    panic!(
                        "{at}: {} owner {owner:?} names child {child:?}, which is not live",
                        self.what
                    )
                });
                assert_eq!(
                    value.owner_key(),
                    owner,
                    "{at}: {} child {child:?} is indexed under an owner it does not report",
                    self.what
                );
            }
        }
    }
}

/// `take_owner`'s falsifier.
///
/// Both `Nip77Sessions` and `RequestReplacements` only ever reach `by_child`
/// and `by_owner` through `insert`/`take`/`take_owner`, which keep the two
/// maps in lockstep by construction -- production code cannot produce a
/// reverse edge naming a child the forward map has already lost. That is
/// exactly why the old bug (`take_plan`'s `filter_map`, and the equivalent
/// hand-written loop `abandon_request_replacements_for_session` used to have)
/// was never caught: no integration test could reach the state it mishandled
/// either. This test reaches past every public door -- something only a test
/// inside this module can do -- to build that state directly, asserts it is
/// real before calling the falsified method, and only then calls
/// `take_owner`.
#[cfg(test)]
mod tests {
    use super::*;

    struct Widget {
        owner: u32,
    }

    impl IndexedChild<u32> for Widget {
        fn owner_key(&self) -> &u32 {
            &self.owner
        }
    }

    #[test]
    #[should_panic(expected = "widget: an owner's reverse index names a child that is not live")]
    fn take_owner_panics_on_a_reverse_edge_the_forward_map_already_lost() {
        let mut index: OwnerIndexed<u32, u32, Widget> = OwnerIndexed::new("widget");
        index.insert(1, Widget { owner: 7 });

        // Precondition: the scenario genuinely exists before the break can
        // become observable. The mirror is intact -- one owner, one child,
        // both sides agreeing -- so a panic below can only come from what
        // happens next, not from state this test never actually built.
        assert_eq!(index.owner_keys(), 1);
        assert_eq!(index.owner_edges(), 1);
        assert!(index.contains(&1));

        // Corrupt the forward map only, bypassing `take` entirely. `by_owner`
        // still names child 1 under owner 7; `by_child` does not. This is the
        // exact disagreement a hand-written forward-only removal used to
        // produce.
        index.by_child.remove(&1);
        assert!(!index.contains(&1));
        assert_eq!(
            index.owner_edges(),
            1,
            "the reverse edge must still be there, or this proves nothing about take_owner"
        );

        let _ = index.take_owner(&7);
    }

    /// `assert_consistent`'s falsifier for the corruption a count can never
    /// see: two owners, one child each, with the reverse sets SWAPPED
    /// between them. `len`, `owner_keys`, and `owner_edges` are all
    /// unchanged -- one child moved, zero created or destroyed -- so any
    /// check built from those three numbers alone would read this as
    /// healthy. `assert_consistent` compares by identity (does the child
    /// insist it belongs to the owner naming it?), which is exactly what
    /// this corruption breaks.
    #[test]
    #[should_panic(expected = "is not named by the owner it reports")]
    fn assert_consistent_panics_on_a_cardinality_preserving_swap_between_owners() {
        let mut index: OwnerIndexed<u32, u32, Widget> = OwnerIndexed::new("widget");
        index.insert(1, Widget { owner: 7 });
        index.insert(2, Widget { owner: 9 });

        // Precondition: two owners, one child each, mirror intact.
        assert_eq!(index.len(), 2);
        assert_eq!(index.owner_keys(), 2);
        assert_eq!(index.owner_edges(), 2);
        index.assert_consistent("before swap");

        index.swap_owners_for_test(&7, &9);

        // Every count a size-only check could compare is identical to the
        // precondition -- this corruption is invisible to `len`,
        // `owner_keys`, and `owner_edges` alike.
        assert_eq!(index.len(), 2);
        assert_eq!(index.owner_keys(), 2);
        assert_eq!(index.owner_edges(), 2);

        index.assert_consistent("after swap");
    }
}
