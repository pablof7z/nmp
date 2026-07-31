//! [`GroupPredicate`] -- a composable NIP-29 discovery predicate (#1033).
//!
//! # Why this is not just a `Binding`
//!
//! A NIP-29 discovery predicate is an unfinished thing: it says "groups whose
//! member-list evidence names me" WITHOUT yet saying at which relay that
//! evidence was observed. It cannot say so, because the same predicate is
//! lowered once per host in the scope, each time against that host and no
//! other. A finished `Binding` has already answered that question and answered
//! it once; using one as the currency would force either a flattened
//! `Pinned({A, B})` -- which cross-products evidence observed at A onto a
//! listing at B, a confidently wrong answer -- or a recursive repin of a
//! caller's own binding, which is the silent inheritance `nmp_grammar::Derived`
//! exists to forbid.
//!
//! So the predicate stays open until a host is known, and the scope closes it
//! once per branch. That is the whole justification for the type; it owns no
//! other property and adds no grammar.
//!
//! # Evidence, never exact state
//!
//! kind:39002 is an optional, possibly partial member-list snapshot;
//! kind:39001 an optional informative admin list. Inclusion is evidence;
//! ABSENCE IS NOT evidence of non-membership or of not being an admin. Hence
//! `member_list_includes` / `admin_list_includes` and no spelling that claims
//! exact current state. Reconstructing exact state from the canonical
//! kind:9000/9001 sequence is a different problem, deliberately not smuggled
//! in here.

use nmp_grammar::{Binding, SetAlgebra, SetOp};
use nostr::RelayUrl;

/// A composable NIP-29 discovery predicate.
///
/// Predicates COMPOSE, they do not terminate: [`Self::union`],
/// [`Self::intersect`] and [`Self::minus`] fold them with the grammar's own
/// [`SetAlgebra`], so "a member here AND an admin there", or "…minus muted",
/// costs nothing extra. Lowering a composite lowers every leaf at the same
/// host and folds the results with the same operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupPredicate {
    /// Groups whose observed kind:39002 member-list evidence names these
    /// subjects.
    MemberListIncludes(Binding),
    /// Groups whose observed kind:39001 admin-list evidence names these
    /// subjects.
    AdminListIncludes(Binding),
    /// Set algebra over child predicates, folded left to right.
    Combined {
        /// The algebra to fold with.
        op: SetAlgebra,
        /// The operand predicates.
        operands: Vec<GroupPredicate>,
    },
}

/// Groups whose observed member-list evidence names `subjects`.
///
/// `subjects` is an ordinary [`Binding`], so
/// `Binding::Reactive(IdentityField::ActivePubkey)` stays REACTIVE and the
/// query follows the logged-in user across an account switch -- nothing here
/// flattens it to a literal. A `subjects` binding that is itself derived
/// (say, a kind:3 follows lookup resolving from the author's own outboxes)
/// keeps its OWN complete authority; lowering never rewrites it.
#[must_use]
pub fn member_list_includes(subjects: Binding) -> GroupPredicate {
    GroupPredicate::MemberListIncludes(subjects)
}

/// Groups whose observed admin-list evidence names `subjects`.
///
/// Evidence-scoped exactly like [`member_list_includes`].
#[must_use]
pub fn admin_list_includes(subjects: Binding) -> GroupPredicate {
    GroupPredicate::AdminListIncludes(subjects)
}

impl GroupPredicate {
    /// Groups matching this predicate OR any of `others`.
    #[must_use]
    pub fn union(self, others: impl IntoIterator<Item = GroupPredicate>) -> Self {
        self.combine(SetAlgebra::Union, others)
    }

    /// Groups matching this predicate AND all of `others`.
    #[must_use]
    pub fn intersect(self, others: impl IntoIterator<Item = GroupPredicate>) -> Self {
        self.combine(SetAlgebra::Intersect, others)
    }

    /// Groups matching this predicate and none of `others`.
    #[must_use]
    pub fn minus(self, others: impl IntoIterator<Item = GroupPredicate>) -> Self {
        self.combine(SetAlgebra::Diff, others)
    }

    fn combine(self, op: SetAlgebra, others: impl IntoIterator<Item = GroupPredicate>) -> Self {
        let mut operands = vec![self];
        operands.extend(others);
        Self::Combined { op, operands }
    }

    /// Close the predicate against ONE host: every NIP-29-owned level in the
    /// result is pinned to exactly this host, and every caller-supplied
    /// binding inside it survives untouched.
    pub(crate) fn lower_at(&self, host: &RelayUrl) -> Binding {
        match self {
            Self::MemberListIncludes(subjects) => {
                nmp_nip29::member_list_includes_at(host, subjects.clone())
            }
            Self::AdminListIncludes(subjects) => {
                nmp_nip29::admin_list_includes_at(host, subjects.clone())
            }
            Self::Combined { op, operands } => Binding::SetOp(Box::new(SetOp {
                op: *op,
                operands: operands.iter().map(|each| each.lower_at(host)).collect(),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, BTreeMap};

    use super::*;
    use nmp_grammar::{
        AccessContext, Demand, Derived, Filter, IdentityField, IndexedTagName, Selector,
        SourceAuthority,
    };

    fn host(n: u16) -> RelayUrl {
        RelayUrl::parse(&format!("wss://host-{n}.example.com")).expect("a well-formed host")
    }

    fn pinned(host: RelayUrl) -> SourceAuthority {
        SourceAuthority::Pinned(BTreeSet::from([host]))
    }

    fn me() -> Binding {
        Binding::Reactive(IdentityField::ActivePubkey)
    }

    fn derived(binding: &Binding) -> &Derived {
        match binding {
            Binding::Derived(inner) => inner,
            other => panic!("expected Derived, got {other:?}"),
        }
    }

    /// A follows lookup an APP built: its own kind:3 selection resolving from
    /// the author's own outboxes, which no NIP-29 layer may repin.
    fn follows_of_me() -> Binding {
        Binding::Derived(Box::new(Derived {
            inner: Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([3u16])),
                    authors: Some(me()),
                    ..Filter::default()
                },
                SourceAuthority::AuthorOutboxes,
                AccessContext::Public,
            )
            .expect("an author-bound outbox demand is constructible"),
            project: Selector::Tag("p".to_string()),
        }))
    }

    #[test]
    fn a_lowered_member_predicate_pins_its_own_level_to_the_exact_host() {
        let lowered = member_list_includes(me()).lower_at(&host(1));
        assert_eq!(derived(&lowered).inner.source, pinned(host(1)));
        assert_eq!(
            derived(&lowered).inner.selection.kinds,
            Some(BTreeSet::from([39002u16]))
        );
    }

    /// The identity reference survives lowering as a REACTIVE binding, so an
    /// account switch re-resolves the query instead of pinning yesterday's
    /// user into it.
    #[test]
    fn the_active_pubkey_stays_reactive_through_lowering() {
        let lowered = member_list_includes(me()).lower_at(&host(1));
        let p = IndexedTagName::new('p').unwrap();
        assert_eq!(derived(&lowered).inner.selection.tags.get(&p), Some(&me()));
    }

    /// Composition is the grammar's own set algebra over ordinary bindings --
    /// no conversion, no second combinator vocabulary.
    #[test]
    fn set_algebra_composes_predicates_into_ordinary_bindings() {
        let composed = member_list_includes(me())
            .intersect([admin_list_includes(follows_of_me())])
            .lower_at(&host(1));
        match composed {
            Binding::SetOp(set) => {
                assert_eq!(set.op, SetAlgebra::Intersect);
                assert_eq!(set.operands.len(), 2);
                for operand in &set.operands {
                    assert_eq!(
                        derived(operand).inner.source,
                        pinned(host(1)),
                        "every NIP-29-owned level is pinned to the branch host"
                    );
                }
            }
            other => panic!("expected SetOp, got {other:?}"),
        }
    }

    /// THE hazard this whole design exists to prevent, stated as an
    /// assertion: a NIP-29-owned level is pinned to the branch host, and a
    /// caller-owned level nested inside it keeps its own authority. Inherit
    /// or blanket-repin, and the innermost kind:3 lookup would ask the
    /// group's hosts for a contact list they have no reason to hold -- it
    /// would not error, it would silently under-resolve.
    #[test]
    fn a_caller_owned_inner_demand_keeps_its_own_authority_at_depth_two() {
        let lowered = admin_list_includes(follows_of_me()).lower_at(&host(1));

        // Depth 1: NIP-29's own admin-record lookup, pinned to the host.
        let admin_records = &derived(&lowered).inner;
        assert_eq!(admin_records.source, pinned(host(1)));

        // Depth 2: the app's follows lookup, still on the author's outboxes.
        let p = IndexedTagName::new('p').unwrap();
        let subjects = admin_records
            .selection
            .tags
            .get(&p)
            .expect("the admin lookup binds #p to the caller's subjects");
        assert_eq!(
            derived(subjects).inner.source,
            SourceAuthority::AuthorOutboxes,
            "NIP-29 never recursively overwrites app-owned authority"
        );
    }

    /// Lowering the same predicate at two hosts yields two independent
    /// values, each closed against its own host and neither mentioning the
    /// other.
    #[test]
    fn two_hosts_lower_to_two_independently_pinned_values() {
        let predicate = member_list_includes(me());
        let at_one = predicate.lower_at(&host(1));
        let at_two = predicate.lower_at(&host(2));
        assert_ne!(at_one, at_two);
        assert_eq!(derived(&at_one).inner.source, pinned(host(1)));
        assert_eq!(derived(&at_two).inner.source, pinned(host(2)));
    }

    #[test]
    fn union_and_diff_fold_with_the_grammars_own_algebra() {
        let _ = BTreeMap::<u8, u8>::new();
        for (predicate, expected) in [
            (
                member_list_includes(me()).union([admin_list_includes(me())]),
                SetAlgebra::Union,
            ),
            (
                member_list_includes(me()).minus([admin_list_includes(me())]),
                SetAlgebra::Diff,
            ),
        ] {
            match predicate.lower_at(&host(1)) {
                Binding::SetOp(set) => assert_eq!(set.op, expected),
                other => panic!("expected SetOp, got {other:?}"),
            }
        }
    }
}
