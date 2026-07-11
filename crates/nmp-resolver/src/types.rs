//! The small shared vocabulary the graph (`graph.rs`) and the pure
//! evaluation functions (`eval.rs`) both need: node identifiers, the
//! resolved-value element shape, field slots, and parent links (M1 plan
//! §3.1/§3.2).

use std::collections::BTreeSet;

use nmp_grammar::TagName;

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
    Tag(TagName),
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

/// A `BindingNode`'s resolved value set — the "ResolvedSet" of M1 plan §3.1.
pub(crate) type ResolvedSet = BTreeSet<Element>;

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
