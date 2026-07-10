//! [`Binding`], [`Derived`], [`SetOp`], [`SetAlgebra`], and the live-query
//! [`Filter`] — the reactive filter-binding grammar (VISION §2 P2).

use std::collections::{BTreeMap, BTreeSet};

use crate::selector::{IdentityField, Selector};
use crate::tag_name::TagName;

/// Every bindable filter-field value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Binding {
    /// A fixed hex/tag-value set.
    Literal(BTreeSet<String>),
    /// A reactive identity reference, e.g. `$currentPubkey` — legal in
    /// `authors` AND in any tag field (position-agnostic; the resolver
    /// never branches on which field a binding sits in).
    Reactive(IdentityField),
    /// The result of an inner [`Filter`] projected through a [`Selector`].
    Derived(Box<Derived>),
    /// Set algebra over child bindings (M0-refuter amendment #1 — e.g.
    /// "follows minus mutes").
    SetOp(Box<SetOp>),
}

/// A `Binding::Derived` payload: an inner live filter plus the selector that
/// projects its resolved rows into a value set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Derived {
    /// The inner live query.
    pub inner: Filter,
    /// How the inner query's resolved rows are projected into a value set.
    pub project: Selector,
}

/// A `Binding::SetOp` payload: a set-algebra operation folded over child
/// bindings.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SetOp {
    /// The algebra to fold with.
    pub op: SetAlgebra,
    /// The operand bindings, folded left-to-right.
    pub operands: Vec<Binding>,
}

/// Set algebra over resolved value sets. `Diff` is non-negotiable: it is
/// what makes "follows MINUS mutes" declarable (bug-class ledger #11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SetAlgebra {
    /// Union of all operands.
    Union,
    /// Intersection of all operands.
    Intersect,
    /// First operand minus the union of the rest.
    Diff,
}

/// A live-query filter whose field values may be [`Binding`]s.
///
/// `kinds` are LITERAL in M1 (not bindable) — the simplest shape that
/// matches every M1 falsifier; the grammar does not forbid making `kinds`
/// bindable later.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Filter {
    /// Literal kind set (not bindable in M1).
    pub kinds: Option<BTreeSet<u16>>,
    /// The `authors` field's binding, if constrained.
    pub authors: Option<Binding>,
    /// The `ids` field's binding, if constrained.
    pub ids: Option<Binding>,
    /// Per-tag bindings — any `Binding` may appear here, including
    /// `Reactive(ActivePubkey)` in e.g. `#p` (amendment #2).
    pub tags: BTreeMap<TagName, Binding>,
    /// Inclusive lower bound on `created_at`.
    pub since: Option<u64>,
    /// Inclusive upper bound on `created_at`.
    pub until: Option<u64>,
    /// Result-count cap.
    pub limit: Option<usize>,
}

impl Filter {
    /// A filter with no constraints at all.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk_hex() -> String {
        "a".repeat(64)
    }

    #[test]
    fn setop_and_addresscoord_variants_exist_and_derive_ord_hash() {
        use std::collections::BTreeSet as Set;

        let diff = SetOp {
            op: SetAlgebra::Diff,
            operands: vec![
                Binding::Literal(Set::from([pk_hex()])),
                Binding::Reactive(IdentityField::ActivePubkey),
            ],
        };
        let binding = Binding::SetOp(Box::new(diff.clone()));

        // Ord + Hash: usable as a BTreeSet element / map key.
        let mut set = Set::new();
        set.insert(binding.clone());
        set.insert(binding);
        assert_eq!(set.len(), 1, "identical SetOp bindings should dedup under Ord+Hash");

        // SetAlgebra variants are distinguishable and totally ordered.
        assert_ne!(SetAlgebra::Union, SetAlgebra::Diff);
        let mut algebras = vec![SetAlgebra::Diff, SetAlgebra::Union, SetAlgebra::Intersect];
        algebras.sort();
        assert_eq!(
            algebras,
            vec![SetAlgebra::Union, SetAlgebra::Intersect, SetAlgebra::Diff]
        );

        // AddressCoord selector round-trips through a Derived binding.
        let derived = Derived {
            inner: Filter {
                kinds: Some(Set::from([30003])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            project: Selector::AddressCoord,
        };
        assert_eq!(derived.project, Selector::AddressCoord);
    }

    #[test]
    fn reactive_is_position_agnostic_between_authors_and_tags() {
        let mut f = Filter::new();
        f.authors = Some(Binding::Reactive(IdentityField::ActivePubkey));
        f.tags.insert(
            TagName::new('p').unwrap(),
            Binding::Reactive(IdentityField::ActivePubkey),
        );
        assert_eq!(f.authors, f.tags.get(&TagName::new('p').unwrap()).cloned());
    }
}
