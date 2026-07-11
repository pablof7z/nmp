//! The graph itself: a tree of `FilterNode`s and `BindingNode`s (M1 plan
//! §3.1/§3.2), unified under one [`Node`] enum keyed by [`NodeId`] so that
//! parent-links, depth, and traversal are uniform regardless of node kind.
//!
//! This module holds data + the *pure*, store-independent parts of the
//! algorithm (reading already-cached values, diffing atom sets, walking the
//! tree in construction order). Everything that needs the event store
//! (initial evaluation, re-querying on recompute) lives in `engine.rs`,
//! which owns both a `Graph` and an `EventStore`.

use std::collections::{BTreeSet, HashMap};

use nmp_grammar::ConcreteFilter;

use crate::eval::merge_element_into;
use crate::types::{FieldSlot, NodeId, ParentLink, ResolvedSet};

/// A `BindingNode::Literal` — a fixed value set, immutable for the graph's
/// lifetime.
#[derive(Debug, Clone)]
pub(crate) struct LiteralNode {
    pub(crate) resolved: ResolvedSet,
}

/// A `BindingNode::Reactive` — resolves from the identity register.
#[derive(Debug, Clone)]
pub(crate) struct ReactiveNode {
    pub(crate) field: nmp_grammar::IdentityField,
    pub(crate) cached: ResolvedSet,
}

/// A `BindingNode::Derived` — an inner `FilterNode` plus the selector that
/// projects its queried rows into a value set.
#[derive(Debug, Clone)]
pub(crate) struct DerivedNode {
    pub(crate) inner: NodeId,
    pub(crate) project: nmp_grammar::Selector,
    pub(crate) cached: ResolvedSet,
}

/// A `BindingNode::SetOp` — a set-algebra fold over operand BindingNodes.
#[derive(Debug, Clone)]
pub(crate) struct SetOpNode {
    pub(crate) op: nmp_grammar::SetAlgebra,
    pub(crate) operands: Vec<NodeId>,
    pub(crate) cached: ResolvedSet,
}

/// A `FilterNode` — one live `Filter` instance: a literal base (kinds /
/// since / until / limit) plus zero or more bound fields, each a
/// `(FieldSlot, BindingNode NodeId)` pair. M1's tests exercise at most one
/// bound field per node (M1 plan §3.5 scope note); the cartesian mechanism
/// below generalizes to N without any per-shape branching, so supporting N
/// costs nothing extra in code paths and isn't itself special-cased.
#[derive(Debug, Clone)]
pub(crate) struct FilterNodeData {
    pub(crate) kinds: Option<BTreeSet<u16>>,
    pub(crate) since: Option<u64>,
    pub(crate) until: Option<u64>,
    pub(crate) limit: Option<usize>,
    pub(crate) bound: Vec<(FieldSlot, NodeId)>,
    pub(crate) cached_atoms: BTreeSet<ConcreteFilter>,
}

/// One graph node: a `BindingNode` variant or a `FilterNode`.
#[derive(Debug, Clone)]
pub(crate) enum Node {
    Literal(LiteralNode),
    Reactive(ReactiveNode),
    Derived(DerivedNode),
    SetOp(SetOpNode),
    Filter(FilterNodeData),
}

/// Per-node bookkeeping: its parent link (for propagation) and its depth
/// (distance from its graph's root FilterNode, root = 0), used to schedule
/// recompute rounds deepest-first so every child is resolved before its
/// parent is recomputed (M1 plan §3.3: exactly one recompute pass, never a
/// partially-updated intermediate value observed by a parent).
#[derive(Debug, Clone, Copy)]
pub(crate) struct NodeMeta {
    pub(crate) parent: ParentLink,
    pub(crate) depth: u32,
}

/// The graph: every node from every currently-subscribed `LiveQuery`,
/// keyed by a flat `NodeId` space (a plain counter, not the `slotmap`
/// crate — M1's graphs are small and bounded).
#[derive(Debug, Default)]
pub(crate) struct Graph {
    next_id: NodeId,
    nodes: HashMap<NodeId, Node>,
    meta: HashMap<NodeId, NodeMeta>,
}

impl Graph {
    pub(crate) fn alloc_id(&mut self) -> NodeId {
        self.next_id += 1;
        self.next_id
    }

    pub(crate) fn insert(&mut self, id: NodeId, node: Node, parent: ParentLink, depth: u32) {
        self.nodes.insert(id, node);
        self.meta.insert(id, NodeMeta { parent, depth });
    }

    pub(crate) fn remove_node(&mut self, id: NodeId) {
        self.nodes.remove(&id);
        self.meta.remove(&id);
    }

    pub(crate) fn node(&self, id: NodeId) -> &Node {
        self.nodes.get(&id).expect("node id must exist in graph")
    }

    pub(crate) fn parent_of(&self, id: NodeId) -> ParentLink {
        self.meta.get(&id).expect("node id must have meta").parent
    }

    pub(crate) fn depth_of(&self, id: NodeId) -> u32 {
        self.meta.get(&id).expect("node id must have meta").depth
    }

    fn filter_data(&self, id: NodeId) -> &FilterNodeData {
        match self.node(id) {
            Node::Filter(f) => f,
            _ => panic!("node {id} is not a FilterNode"),
        }
    }

    /// The current resolved set of any BindingNode variant (Literal's fixed
    /// set, Reactive/Derived/SetOp's cached set).
    pub(crate) fn resolved_set_of(&self, id: NodeId) -> &ResolvedSet {
        match self.node(id) {
            Node::Literal(n) => &n.resolved,
            Node::Reactive(n) => &n.cached,
            Node::Derived(n) => &n.cached,
            Node::SetOp(n) => &n.cached,
            Node::Filter(_) => panic!("node {id} is a FilterNode, not a BindingNode"),
        }
    }

    /// Every `Derived` BindingNode id currently in the graph, across all
    /// live LiveQuery subscriptions. Used by ingest's dirty-mark phase (M1
    /// plan §3.3 step 2) to find which nodes might be affected by a batch
    /// of changed events.
    pub(crate) fn derived_node_ids(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter_map(|(id, n)| matches!(n, Node::Derived(_)).then_some(*id))
            .collect()
    }

    /// A FilterNode's own current atoms — no subtree walk, unlike
    /// [`Self::atoms_in_structural_order`] (which also collects every
    /// descendant `Derived`'s inner FilterNode atoms, for demand-set
    /// refcounting). Used by `Engine::root_atoms` to report exactly what a
    /// subscription's OWN descriptor resolves to.
    pub(crate) fn cached_atoms_of(&self, filter_id: NodeId) -> &BTreeSet<ConcreteFilter> {
        &self.filter_data(filter_id).cached_atoms
    }

    /// The wide query filter for a FilterNode: base (kinds/since/until/limit)
    /// merged with the FULL resolved set of every bound field. This is the
    /// single `nostr::Filter` a `Derived` parent queries the store with, and
    /// the exact filter §3.3 step 2 matches incoming events against for
    /// dirty-marking. `None` iff any bound field's resolved set is empty —
    /// per the empty-set-never-widens-to-wildcard invariant (M1 plan §3.4),
    /// an empty resolved field makes the whole filter match nothing, so we
    /// skip querying entirely rather than asking the store with an
    /// under-constrained filter.
    pub(crate) fn wide_concrete(&self, filter_id: NodeId) -> Option<ConcreteFilter> {
        let f = self.filter_data(filter_id);
        let mut cf = ConcreteFilter {
            kinds: f.kinds.clone(),
            since: f.since,
            until: f.until,
            limit: f.limit,
            ..ConcreteFilter::default()
        };
        for (slot, binding_id) in &f.bound {
            let set = self.resolved_set_of(*binding_id);
            if set.is_empty() {
                return None;
            }
            for el in set {
                merge_element_into(&mut cf, slot, el);
            }
        }
        Some(cf)
    }

    /// The fanned-out per-element demand atoms for a FilterNode (M1 plan
    /// §3.4): the cartesian product of the base filter across each bound
    /// field's resolved elements. Zero bound fields => exactly one atom
    /// (the base). Any bound field resolving to the empty set => zero atoms
    /// (never a wildcard).
    pub(crate) fn compute_atoms(&self, filter_id: NodeId) -> BTreeSet<ConcreteFilter> {
        let f = self.filter_data(filter_id);
        let base = ConcreteFilter {
            kinds: f.kinds.clone(),
            since: f.since,
            until: f.until,
            limit: f.limit,
            ..ConcreteFilter::default()
        };
        if f.bound.is_empty() {
            return BTreeSet::from([base]);
        }
        let mut atoms = vec![base];
        for (slot, binding_id) in &f.bound {
            let set = self.resolved_set_of(*binding_id);
            if set.is_empty() {
                return BTreeSet::new();
            }
            let mut next = Vec::with_capacity(atoms.len() * set.len());
            for existing in &atoms {
                for el in set {
                    let mut cf = existing.clone();
                    merge_element_into(&mut cf, slot, el);
                    next.push(cf);
                }
            }
            atoms = next;
        }
        atoms.into_iter().collect()
    }

    pub(crate) fn set_reactive_cached(&mut self, id: NodeId, new: ResolvedSet) -> bool {
        match self.nodes.get_mut(&id).expect("reactive node must exist") {
            Node::Reactive(n) => {
                let changed = n.cached != new;
                n.cached = new;
                changed
            }
            _ => panic!("node {id} is not a Reactive BindingNode"),
        }
    }

    pub(crate) fn set_derived_cached(&mut self, id: NodeId, new: ResolvedSet) -> bool {
        match self.nodes.get_mut(&id).expect("derived node must exist") {
            Node::Derived(n) => {
                let changed = n.cached != new;
                n.cached = new;
                changed
            }
            _ => panic!("node {id} is not a Derived BindingNode"),
        }
    }

    pub(crate) fn set_setop_cached(&mut self, id: NodeId, new: ResolvedSet) -> bool {
        match self.nodes.get_mut(&id).expect("setop node must exist") {
            Node::SetOp(n) => {
                let changed = n.cached != new;
                n.cached = new;
                changed
            }
            _ => panic!("node {id} is not a SetOp BindingNode"),
        }
    }

    /// Replace a FilterNode's cached atom set, returning the OLD set for the
    /// caller to diff against.
    pub(crate) fn set_filter_cached_atoms(
        &mut self,
        id: NodeId,
        new: BTreeSet<ConcreteFilter>,
    ) -> BTreeSet<ConcreteFilter> {
        match self.nodes.get_mut(&id).expect("filter node must exist") {
            Node::Filter(f) => std::mem::replace(&mut f.cached_atoms, new),
            _ => panic!("node {id} is not a FilterNode"),
        }
    }

    /// Walk this graph in the same bottom-up order construction uses
    /// (innermost FilterNodes first: a `Derived`'s `inner` before the
    /// FilterNode that binds it, a `SetOp`'s operands before the FilterNode
    /// that binds it), collecting every FilterNode's *current* atoms in
    /// that order. Used both to build the initial-open sequence (subscribe)
    /// and — reversed — the teardown/re-root close sequence (M1 plan
    /// §3.6/§4: "closes in reverse-of-open order").
    pub(crate) fn atoms_in_structural_order(&self, root: NodeId) -> Vec<ConcreteFilter> {
        let mut out = Vec::new();
        self.walk_filter_postorder(root, &mut out);
        out
    }

    fn walk_filter_postorder(&self, filter_id: NodeId, out: &mut Vec<ConcreteFilter>) {
        let f = self.filter_data(filter_id);
        for (_, binding_id) in &f.bound {
            self.walk_binding_postorder(*binding_id, out);
        }
        out.extend(f.cached_atoms.iter().cloned());
    }

    fn walk_binding_postorder(&self, binding_id: NodeId, out: &mut Vec<ConcreteFilter>) {
        match self.node(binding_id) {
            Node::Derived(d) => self.walk_filter_postorder(d.inner, out),
            Node::SetOp(s) => {
                for op in &s.operands {
                    self.walk_binding_postorder(*op, out);
                }
            }
            Node::Literal(_) | Node::Reactive(_) => {}
            Node::Filter(_) => unreachable!("a BindingNode id must never reference a FilterNode"),
        }
    }

    /// A minimal introspectable snapshot of every node currently in the
    /// graph: its id, a kind label, and the size of its cached value (M1
    /// plan §6 — `graph_snapshot()` lets a reviewer inspect the graph shape
    /// directly rather than trusting prose).
    pub(crate) fn snapshot_entries(&self) -> Vec<(u64, &'static str, usize)> {
        self.nodes
            .iter()
            .map(|(id, node)| {
                let (kind, size) = match node {
                    Node::Literal(n) => ("literal", n.resolved.len()),
                    Node::Reactive(n) => ("reactive", n.cached.len()),
                    Node::Derived(n) => ("derived", n.cached.len()),
                    Node::SetOp(n) => ("setop", n.cached.len()),
                    Node::Filter(f) => ("filter", f.cached_atoms.len()),
                };
                (*id, kind, size)
            })
            .collect()
    }

    /// Every node id (FilterNode and BindingNode alike) reachable from
    /// `root`, in the same postorder as [`Self::atoms_in_structural_order`].
    /// Used at full graph teardown to know exactly which node ids to purge.
    pub(crate) fn collect_node_ids(&self, root: NodeId, out: &mut Vec<NodeId>) {
        self.collect_filter_ids(root, out);
    }

    fn collect_filter_ids(&self, filter_id: NodeId, out: &mut Vec<NodeId>) {
        let f = self.filter_data(filter_id);
        for (_, binding_id) in &f.bound {
            self.collect_binding_ids(*binding_id, out);
        }
        out.push(filter_id);
    }

    fn collect_binding_ids(&self, binding_id: NodeId, out: &mut Vec<NodeId>) {
        match self.node(binding_id) {
            Node::Derived(d) => self.collect_filter_ids(d.inner, out),
            Node::SetOp(s) => {
                for op in &s.operands {
                    self.collect_binding_ids(*op, out);
                }
            }
            Node::Literal(_) | Node::Reactive(_) => {}
            Node::Filter(_) => unreachable!("a BindingNode id must never reference a FilterNode"),
        }
        out.push(binding_id);
    }
}
