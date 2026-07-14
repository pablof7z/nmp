//! The small shared vocabulary the graph (`graph.rs`) and the pure
//! evaluation functions (`eval.rs`) both need: node identifiers, the
//! resolved-value element shape, field slots, and parent links (M1 plan
//! §3.1/§3.2).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{IndexedTagName, RoutingEvidence};

/// Opaque node identifier. A plain incrementing counter (not the `slotmap`
/// crate) — M1's graphs are small and bounded (depth ≤ 3), so a `HashMap`
/// keyed by a counter gives everything the plan's "SlotMap<NodeId, Node>"
/// sketch needs without an extra dependency.
pub(crate) type NodeId = u64;

/// Which grammar-level field slot a `BindingNode` is attached to. Distinct
/// from [`nmp_grammar::Selector`] (which describes a `Derived`'s
/// *projection*, i.e. what its inner rows are turned into) — `FieldSlot`
/// describes *where* a resolved value set is written back into a
/// `ConcreteFilter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum FieldSlot {
    Authors,
    Ids,
    Tag(IndexedTagName),
}

/// A single resolved value produced by a `BindingNode`. Two shapes:
///
/// - `Scalar` — a pubkey-hex / id-hex / tag-value (opaque string to the
///   resolver).
/// - `Coord` — a co-pinned `(kind, author, d)` address coordinate produced
///   by a `Selector::AddressCoord` projection (M1 plan §3.5).
///
/// Which shape a resolved set holds is a *type-level* fact about the leaf
/// `Selector` that produced it. Dispatching on this enum's variant is
/// structural dispatch over the grammar's own closed projection vocabulary
/// — not a kind-literal branch (there is no comparison against any event
/// `kind` value anywhere in this dispatch).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Element {
    Scalar(String),
    Coord {
        kind: u16,
        author: String,
        d: String,
    },
}

/// A `BindingNode`'s resolved values together with every routing fact that
/// reached each value. Set algebra operates on the map keys and retains or
/// unions the corresponding evidence; evidence never changes value equality.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedSet(BTreeMap<Element, BTreeSet<RoutingEvidence>>);

impl ResolvedSet {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn insert(&mut self, element: Element) {
        self.0.entry(element).or_default();
    }

    pub(crate) fn insert_with(
        &mut self,
        element: Element,
        evidence: impl IntoIterator<Item = RoutingEvidence>,
    ) {
        self.0.entry(element).or_default().extend(evidence);
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Element, &BTreeSet<RoutingEvidence>)> {
        self.0.iter()
    }

    pub(crate) fn contains(&self, element: &Element) -> bool {
        self.0.contains_key(element)
    }

    pub(crate) fn merge_from(&mut self, other: &Self) {
        for (element, evidence) in other.iter() {
            self.insert_with(element.clone(), evidence.iter().cloned());
        }
    }

    pub(crate) fn remove(&mut self, element: &Element) {
        self.0.remove(element);
    }
}

impl<const N: usize> From<[Element; N]> for ResolvedSet {
    fn from(elements: [Element; N]) -> Self {
        elements.into_iter().collect()
    }
}

impl FromIterator<Element> for ResolvedSet {
    fn from_iter<T: IntoIterator<Item = Element>>(iter: T) -> Self {
        let mut set = Self::new();
        for element in iter {
            set.insert(element);
        }
        set
    }
}

impl<'a> IntoIterator for &'a ResolvedSet {
    type Item = (&'a Element, &'a BTreeSet<RoutingEvidence>);
    type IntoIter = std::collections::btree_map::Iter<'a, Element, BTreeSet<RoutingEvidence>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Where a node's changed value must propagate to next. Every node in a
/// graph has exactly one parent (the grammar's dependency graph is a tree)
/// except a graph's root `FilterNode`, whose "parent" is the global
/// demand-atom table itself (`Root`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParentLink {
    /// This is a graph's root `FilterNode`. Its atom diff feeds the global
    /// demand-atom table directly; there is no further ancestor to notify.
    Root,
    /// Parent is a `SetOp` BindingNode; this node is one of its operands.
    SetOpOperand(NodeId),
    /// Parent is a `FilterNode`; this BindingNode is one of its bound
    /// fields.
    FilterField(NodeId),
    /// Parent is a `Derived` BindingNode; this `FilterNode` is its `inner`.
    DerivedInner(NodeId),
}
