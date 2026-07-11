//! [`Engine`] — the graph engine, atom refcounting, identity register, and
//! metrics (M1 plan §2.3, §3, §4). This is the only module that touches the
//! `EventStore`; `graph.rs` holds the pure graph data + read-only algorithm,
//! `eval.rs` holds pure leaf computations.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::{Rc, Weak};

use nmp_grammar::{Binding, ConcreteFilter, DemandDelta, DemandOp, DescriptorHash, Filter};
use nmp_store::{EventStore, InsertOutcome, RelayObserved};
use nostr::filter::MatchEventOptions;
use nostr::RelayUrl;

use crate::eval::{project_events, resolve_reactive, resolve_setop};
use crate::graph::{
    DerivedNode, FilterNodeData, Graph, LiteralNode, Node, ReactiveNode, SetOpNode,
};
use crate::types::{Element, FieldSlot, NodeId, ParentLink, ResolvedSet};

/// The descriptor value of a live query: a `nmp_grammar::Filter` whose
/// `Binding`s may reference the identity register and/or other filters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiveQuery(pub Filter);

/// Opaque identifier for a live subscription handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HandleId(u64);

/// A live subscription handle. Explicit [`Engine::unsubscribe`] is the path
/// the M1 contract tests use (deterministic, headless); `Drop` is a thin
/// wrapper over the same path — dropping a handle enqueues its id for
/// withdrawal, and the engine drains that queue at the start of its next
/// mutating call (M1 plan §4: "Drop... a thin wrapper over the same path";
/// the grace-window debounce a real Drop would eventually want is an
/// explicitly deferred M4 concern, not built here).
pub struct QueryHandle {
    id: HandleId,
    pending_drops: Weak<RefCell<Vec<HandleId>>>,
}

impl QueryHandle {
    pub fn id(&self) -> HandleId {
        self.id
    }
}

impl Drop for QueryHandle {
    fn drop(&mut self) {
        if let Some(cell) = self.pending_drops.upgrade() {
            cell.borrow_mut().push(self.id);
        }
    }
}

/// Cascade/rebuild-witness counters (M1 plan §3.7/§6). `atoms_opened +
/// atoms_closed` must equal `|symmetric diff|` on every surgical test, never
/// `2 * |set|` — the replace-not-rebuild witness.
#[derive(Debug, Default, Clone)]
pub struct Metrics {
    /// Advances exactly once per `ingest`/`set_active_pubkey` batch that
    /// actually had something to recompute.
    pub recompute_passes: u64,
    /// Incremented once per node whose recomputed value actually changed
    /// (cascade-depth witness).
    pub nodes_recomputed: u64,
    /// Incremented once per node actually re-evaluated, whether or not its
    /// value changed.
    pub sets_reevaluated: u64,
    /// Incremented once per demand atom crossing refcount 0 -> 1.
    pub atoms_opened: u64,
    /// Incremented once per demand atom crossing refcount 1 -> 0.
    pub atoms_closed: u64,
}

/// A minimal, introspectable view of the graph's current shape (M1 plan
/// §6): one entry per node, its kind, and the size of its cached value.
/// Lets a reviewer (or a test) inspect the graph directly rather than
/// trusting prose.
#[derive(Debug, Clone)]
pub struct GraphNodeInfo {
    pub id: u64,
    pub kind: &'static str,
    pub cached_size: usize,
}

#[derive(Debug, Clone, Default)]
pub struct GraphSnapshot {
    pub nodes: Vec<GraphNodeInfo>,
}

/// Accumulates a batch's demand-set changes. Closes are appended in
/// natural bottom-up discovery order and reversed exactly once when the
/// delta is finalized, giving "closes in reverse-of-open order" (M1 plan
/// §3.6/§4) regardless of which code path fed them in.
#[derive(Default)]
struct DeltaAcc {
    closes: Vec<ConcreteFilter>,
    opens: Vec<ConcreteFilter>,
}

impl DeltaAcc {
    fn push_close(&mut self, cf: ConcreteFilter) {
        self.closes.push(cf);
    }

    fn push_open(&mut self, cf: ConcreteFilter) {
        self.opens.push(cf);
    }

    fn into_delta(mut self) -> DemandDelta {
        self.closes.reverse();
        let mut ops: Vec<DemandOp> = self.closes.into_iter().map(DemandOp::Close).collect();
        ops.extend(self.opens.into_iter().map(DemandOp::Open));
        DemandDelta { ops }
    }
}

/// Concatenate two already-internally-ordered `DemandDelta`s: `first`'s ops
/// then `second`'s ops. Used to merge a drained drop-delta (all `Close`,
/// M1 nit #2) ahead of a call's own delta (already closes-then-opens),
/// preserving "all closes precede all opens" overall.
fn merge_deltas(mut first: DemandDelta, second: DemandDelta) -> DemandDelta {
    first.ops.extend(second.ops);
    first
}

struct GraphEntry {
    descriptor: LiveQuery,
    refcount: u32,
}

/// The graph engine (M1 plan §2.3): owns the store, the graph, the
/// descriptor/atom refcount tables, the identity register, and metrics.
pub struct Engine<S: EventStore> {
    store: S,
    graph: Graph,
    descriptor_to_root: BTreeMap<LiveQuery, NodeId>,
    graph_entries: HashMap<NodeId, GraphEntry>,
    handle_to_root: HashMap<HandleId, NodeId>,
    next_handle: u64,
    /// The demand truth (M1 plan §3.2): every `ConcreteFilter` any live
    /// FilterNode currently contributes, refcounted. Open fires on 0->1,
    /// close on 1->0.
    atoms: BTreeMap<DescriptorHash, (ConcreteFilter, u32)>,
    /// Every `Reactive` BindingNode id currently in the graph, across all
    /// live subscriptions — the re-root seed set (M1 plan §3.6).
    reactive_nodes: BTreeSet<NodeId>,
    identity: Option<nostr::PublicKey>,
    metrics: Metrics,
    pending_drops: Rc<RefCell<Vec<HandleId>>>,
}

impl<S: EventStore> Engine<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            graph: Graph::default(),
            descriptor_to_root: BTreeMap::new(),
            graph_entries: HashMap::new(),
            handle_to_root: HashMap::new(),
            next_handle: 0,
            atoms: BTreeMap::new(),
            reactive_nodes: BTreeSet::new(),
            identity: None,
            metrics: Metrics::default(),
            pending_drops: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn active_demand(&self) -> BTreeSet<ConcreteFilter> {
        self.atoms.values().map(|(cf, _)| cf.clone()).collect()
    }

    /// Read access to the underlying store. `EngineCore` (M3 step B) needs
    /// this for coverage watermark reads (`get_coverage`) that have nothing
    /// to do with graph evaluation — the resolver's own methods only ever
    /// touch the store via `insert`/`query` internally, so there was no
    /// existing seam for a caller that needs the store's OTHER door.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Mutable access to the underlying store. `EngineCore` needs this to
    /// call `record_coverage` (the coverage-attribution ruling,
    /// `docs/consults/2026-07-11-fable-coverage-attribution.md`, is engine-
    /// owned logic; the resolver has no notion of relays or wire REQs at
    /// all, so it cannot and must not decide what to record here itself).
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// The ROOT FilterNode's own current atoms for `id`'s subscription —
    /// i.e. exactly the (possibly fanned-out) `ConcreteFilter`s the query's
    /// OWN descriptor resolves to, never an inner `Derived`'s bookkeeping
    /// atoms (contrast with `Graph::atoms_in_structural_order`, which walks
    /// the WHOLE subtree and exists purely for demand-set refcounting).
    /// `EngineCore` uses this to know which store rows/coverage a handle's
    /// `EmitRows` should be computed over. Empty for an unknown handle.
    pub fn root_atoms(&self, id: HandleId) -> BTreeSet<ConcreteFilter> {
        let Some(&root) = self.handle_to_root.get(&id) else {
            return BTreeSet::new();
        };
        self.graph.cached_atoms_of(root).clone()
    }

    pub fn graph_snapshot(&self) -> GraphSnapshot {
        let nodes = self
            .graph
            .snapshot_entries()
            .into_iter()
            .map(|(id, kind, size)| GraphNodeInfo {
                id,
                kind,
                cached_size: size,
            })
            .collect();
        GraphSnapshot { nodes }
    }

    // ---- identity re-root (M1 plan §3.6) --------------------------------

    pub fn set_active_pubkey(&mut self, pk: Option<nostr::PublicKey>) -> DemandDelta {
        let drop_delta = self.drain_pending_drops();
        self.identity = pk;
        let seed: BTreeSet<NodeId> = self.reactive_nodes.iter().copied().collect();
        if seed.is_empty() {
            return drop_delta;
        }
        self.metrics.recompute_passes += 1;
        merge_deltas(drop_delta, self.run_recompute(seed))
    }

    // ---- subscribe / unsubscribe (M1 plan §4) ---------------------------

    pub fn subscribe(&mut self, q: LiveQuery) -> (QueryHandle, DemandDelta) {
        let drop_delta = self.drain_pending_drops();
        let handle_id = self.alloc_handle();

        if let Some(&root) = self.descriptor_to_root.get(&q) {
            // Identical whole-descriptor LiveQuery already has a graph:
            // graph-level dedup (M1 plan §3.2/§4). Bump the graph refcount
            // and this handle's own claim on every atom the (shared) graph
            // currently owns; none of these can cross 0->1 (they're already
            // open), so this always yields an empty delta.
            self.graph_entries
                .get_mut(&root)
                .expect("registry entry must exist")
                .refcount += 1;
            self.handle_to_root.insert(handle_id, root);
            let mut acc = DeltaAcc::default();
            for atom in self.graph.atoms_in_structural_order(root) {
                self.ref_atom(&atom, &mut acc);
            }
            let handle = QueryHandle {
                id: handle_id,
                pending_drops: Rc::downgrade(&self.pending_drops),
            };
            return (handle, merge_deltas(drop_delta, acc.into_delta()));
        }

        let root = self.build_filter_node(&q.0, ParentLink::Root, 0);
        self.descriptor_to_root.insert(q.clone(), root);
        self.graph_entries.insert(
            root,
            GraphEntry {
                descriptor: q,
                refcount: 1,
            },
        );
        self.handle_to_root.insert(handle_id, root);

        let mut acc = DeltaAcc::default();
        for atom in self.graph.atoms_in_structural_order(root) {
            self.ref_atom(&atom, &mut acc);
        }
        let handle = QueryHandle {
            id: handle_id,
            pending_drops: Rc::downgrade(&self.pending_drops),
        };
        (handle, merge_deltas(drop_delta, acc.into_delta()))
    }

    pub fn unsubscribe(&mut self, id: HandleId) -> DemandDelta {
        let drop_delta = self.drain_pending_drops();
        merge_deltas(drop_delta, self.unsubscribe_inner(id))
    }

    /// Flush any handles dropped since the last mutating call, surfacing
    /// their withdrawal as a `DemandDelta` even with no other activity (M1
    /// nit #2, M2 plan §8.2). Every other mutating call already drains and
    /// MERGES pending drops into its own returned delta; this method exists
    /// so a driver can force that flush on a bare `Drop` with nothing else
    /// to piggyback on.
    pub fn poll_pending_drops(&mut self) -> DemandDelta {
        self.drain_pending_drops()
    }

    fn unsubscribe_inner(&mut self, id: HandleId) -> DemandDelta {
        let Some(root) = self.handle_to_root.remove(&id) else {
            return DemandDelta::empty();
        };
        let refcount_after = {
            let entry = self
                .graph_entries
                .get_mut(&root)
                .expect("root must have a registry entry");
            entry.refcount -= 1;
            entry.refcount
        };

        let mut acc = DeltaAcc::default();
        if refcount_after > 0 {
            // Other handles still hold this (shared) graph open: only this
            // handle's own claim on each atom is withdrawn.
            for atom in self.graph.atoms_in_structural_order(root) {
                self.unref_atom(&atom, &mut acc);
            }
            return acc.into_delta();
        }

        // Refcount hit zero: full teardown.
        let descriptor = self.graph_entries.remove(&root).unwrap().descriptor;
        self.descriptor_to_root.remove(&descriptor);
        for atom in self.graph.atoms_in_structural_order(root) {
            self.unref_atom(&atom, &mut acc);
        }
        let mut ids = Vec::new();
        self.graph.collect_node_ids(root, &mut ids);
        for nid in ids {
            self.reactive_nodes.remove(&nid);
            self.graph.remove_node(nid);
        }
        acc.into_delta()
    }

    /// Drain every handle dropped since the last call and MERGE their
    /// withdrawal into a `DemandDelta` (M1 nit #2: previously this
    /// discarded `unsubscribe_inner`'s return with `let _ = ...`, so a
    /// dropped handle's CLOSE never reached the wire). `unsubscribe_inner`
    /// only ever produces `Close` ops, so simple concatenation across
    /// multiple drained drops preserves the "all closes precede all opens"
    /// invariant trivially.
    fn drain_pending_drops(&mut self) -> DemandDelta {
        let ids: Vec<HandleId> = std::mem::take(&mut *self.pending_drops.borrow_mut());
        let mut ops = Vec::new();
        for id in ids {
            let delta = self.unsubscribe_inner(id);
            debug_assert!(
                delta.opened().is_empty(),
                "an unsubscribe delta must never contain Open ops"
            );
            ops.extend(delta.ops);
        }
        DemandDelta { ops }
    }

    fn alloc_handle(&mut self) -> HandleId {
        self.next_handle += 1;
        HandleId(self.next_handle)
    }

    // ---- ingest: the real path (M1 plan §3.3) ---------------------------

    /// `Engine::ingest` predates per-relay provenance (M1) and has no relay
    /// identity of its own to attribute an insert to. This resolver-level
    /// path is exercised by the M1 contract-test harness
    /// (`testkit::Harness::deliver`, which has no relay concept either), so
    /// it attributes every ingested event to a single fixture relay identity
    /// at the event's own `created_at` — sufficient to satisfy the new
    /// `EventStore::insert` door without inventing resolver-owned routing.
    /// `EngineCore` (M3 step B), which DOES know the real relay a frame
    /// arrived on, calls [`Self::ingest_observed`] directly instead.
    fn ingest_fixture_observation(at: nostr::Timestamp) -> RelayObserved {
        RelayObserved::new(
            RelayUrl::parse("wss://resolver-ingest.fixture.invalid")
                .expect("fixture relay URL must parse"),
            at,
        )
    }

    pub fn ingest(&mut self, events: Vec<nostr::Event>) -> DemandDelta {
        let observed = events
            .into_iter()
            .map(|e| {
                let at = e.created_at;
                (e, Self::ingest_fixture_observation(at))
            })
            .collect();
        self.ingest_observed(observed)
    }

    /// The real ingest path (M1 plan §3.3), parameterized over each event's
    /// ACTUAL relay attribution rather than a resolver-invented fixture.
    /// `EngineCore` is the intended caller: it knows exactly which relay a
    /// `RelayFrame` arrived on (`nmp-store`'s `RelayObserved`) and must not
    /// launder that real provenance through `ingest`'s fixture. `ingest`
    /// above is a thin wrapper over this for the resolver-only contract
    /// tests, which have no relay concept of their own.
    pub fn ingest_observed(&mut self, events: Vec<(nostr::Event, RelayObserved)>) -> DemandDelta {
        let drop_delta = self.drain_pending_drops();

        let mut changed: Vec<nostr::Event> = Vec::new();
        for (event, from) in events {
            match self.store.insert(event.clone(), from) {
                InsertOutcome::Inserted | InsertOutcome::Superseded { .. } => changed.push(event),
                // Never stored -- neither is "changed": a duplicate/stale
                // event was already reflected in the store, and a refused
                // event (already-expired, or future tombstoned) never
                // entered it at all.
                InsertOutcome::Duplicate { .. }
                | InsertOutcome::Stale
                | InsertOutcome::Refused(_) => {}
            }
        }
        if changed.is_empty() {
            return drop_delta;
        }
        self.metrics.recompute_passes += 1;

        // Dirty-mark phase (GENERIC — the kill guard, M1 plan §3.3 step 2 /
        // test 10). The ONLY thing that decides whether a changed event
        // affects a Derived BindingNode is `match_event` against that
        // node's own inner FilterNode's concrete filter. No kind literal,
        // no per-shape branch, anywhere in this decision.
        let mut seed: BTreeSet<NodeId> = BTreeSet::new();
        for derived_id in self.graph.derived_node_ids() {
            let Node::Derived(d) = self.graph.node(derived_id) else {
                unreachable!("derived_node_ids only returns Derived node ids")
            };
            if let Some(cf) = self.graph.wide_concrete(d.inner) {
                let nf = cf.to_nostr();
                if changed
                    .iter()
                    .any(|e| nf.match_event(e, MatchEventOptions::new()))
                {
                    seed.insert(derived_id);
                }
            }
        }
        merge_deltas(drop_delta, self.run_recompute(seed))
    }

    // ---- recompute rounds (shared by ingest + re-root) ------------------

    /// Process `pending` in rounds by strictly descending depth, so every
    /// child is fully resolved before its parent is recomputed — the
    /// invariant that makes a single pass correct even when a node (e.g. a
    /// `SetOp`) has multiple simultaneously-dirty children (M1 plan §3.3).
    fn run_recompute(&mut self, mut pending: BTreeSet<NodeId>) -> DemandDelta {
        let mut acc = DeltaAcc::default();
        while !pending.is_empty() {
            let max_depth = pending
                .iter()
                .map(|id| self.graph.depth_of(*id))
                .max()
                .expect("pending is non-empty");
            let this_round: Vec<NodeId> = pending
                .iter()
                .copied()
                .filter(|id| self.graph.depth_of(*id) == max_depth)
                .collect();
            for id in &this_round {
                pending.remove(id);
            }
            for id in this_round {
                self.metrics.sets_reevaluated += 1;
                let changed = self.recompute_node(id, &mut acc);
                if changed {
                    self.metrics.nodes_recomputed += 1;
                    match self.graph.parent_of(id) {
                        ParentLink::Root => {}
                        ParentLink::SetOpOperand(p)
                        | ParentLink::FilterField(p)
                        | ParentLink::DerivedInner(p) => {
                            pending.insert(p);
                        }
                    }
                }
            }
        }
        acc.into_delta()
    }

    /// Recompute node `id`'s value from its (already-current) children.
    /// Returns whether the value changed. For a FilterNode, also diffs +
    /// applies its atom-set change into the global demand table via `acc`.
    fn recompute_node(&mut self, id: NodeId, acc: &mut DeltaAcc) -> bool {
        // Snapshot the node (cheap: M1 graphs are tiny) so we can mutate
        // `self.graph`/`self.store` in the match arms without fighting the
        // borrow checker over a live reference into `self.graph.nodes`.
        let snapshot = self.graph.node(id).clone();
        match snapshot {
            Node::Literal(_) => false,
            Node::Reactive(n) => {
                let new = resolve_reactive(n.field, self.identity);
                self.graph.set_reactive_cached(id, new)
            }
            Node::Derived(n) => {
                let new = match self.graph.wide_concrete(n.inner) {
                    Some(cf) => {
                        let events: Vec<nostr::Event> = self
                            .store
                            .query(&cf.to_nostr())
                            .into_iter()
                            .map(|se| se.event)
                            .collect();
                        project_events(&events, &n.project)
                    }
                    None => ResolvedSet::new(),
                };
                self.graph.set_derived_cached(id, new)
            }
            Node::SetOp(n) => {
                let new = {
                    let sets: Vec<&ResolvedSet> = n
                        .operands
                        .iter()
                        .map(|o| self.graph.resolved_set_of(*o))
                        .collect();
                    resolve_setop(n.op, &sets)
                };
                self.graph.set_setop_cached(id, new)
            }
            Node::Filter(_) => {
                let new_atoms = self.graph.compute_atoms(id);
                let old_atoms = self.graph.set_filter_cached_atoms(id, new_atoms.clone());
                if old_atoms == new_atoms {
                    return false;
                }
                for a in old_atoms.difference(&new_atoms) {
                    self.unref_atom(a, acc);
                }
                for a in new_atoms.difference(&old_atoms) {
                    self.ref_atom(a, acc);
                }
                true
            }
        }
    }

    // ---- atom refcounting (M1 plan §3.2/§4) -----------------------------

    fn ref_atom(&mut self, atom: &ConcreteFilter, acc: &mut DeltaAcc) {
        let hash = atom.hash();
        let entry = self.atoms.entry(hash).or_insert_with(|| (atom.clone(), 0));
        entry.1 += 1;
        if entry.1 == 1 {
            self.metrics.atoms_opened += 1;
            acc.push_open(atom.clone());
        }
    }

    fn unref_atom(&mut self, atom: &ConcreteFilter, acc: &mut DeltaAcc) {
        let hash = atom.hash();
        if let Some(entry) = self.atoms.get_mut(&hash) {
            entry.1 -= 1;
            if entry.1 == 0 {
                self.atoms.remove(&hash);
                self.metrics.atoms_closed += 1;
                acc.push_close(atom.clone());
            }
        }
    }

    // ---- graph construction (M1 plan §3.1) ------------------------------

    fn build_filter_node(&mut self, filter: &Filter, parent: ParentLink, depth: u32) -> NodeId {
        let id = self.graph.alloc_id();

        let mut bound = Vec::new();
        if let Some(b) = &filter.authors {
            let bid = self.build_binding_node(b, ParentLink::FilterField(id), depth + 1);
            bound.push((FieldSlot::Authors, bid));
        }
        if let Some(b) = &filter.ids {
            let bid = self.build_binding_node(b, ParentLink::FilterField(id), depth + 1);
            bound.push((FieldSlot::Ids, bid));
        }
        for (tag, b) in &filter.tags {
            let bid = self.build_binding_node(b, ParentLink::FilterField(id), depth + 1);
            bound.push((FieldSlot::Tag(*tag), bid));
        }

        let data = FilterNodeData {
            kinds: filter.kinds.clone(),
            since: filter.since,
            until: filter.until,
            limit: filter.limit,
            bound,
            cached_atoms: BTreeSet::new(),
        };
        self.graph.insert(id, Node::Filter(data), parent, depth);
        let atoms = self.graph.compute_atoms(id);
        self.graph.set_filter_cached_atoms(id, atoms);
        id
    }

    fn build_binding_node(&mut self, binding: &Binding, parent: ParentLink, depth: u32) -> NodeId {
        match binding {
            Binding::Literal(set) => {
                let id = self.graph.alloc_id();
                let resolved: ResolvedSet = set.iter().cloned().map(Element::Scalar).collect();
                self.graph
                    .insert(id, Node::Literal(LiteralNode { resolved }), parent, depth);
                id
            }
            Binding::Reactive(field) => {
                let id = self.graph.alloc_id();
                let cached = resolve_reactive(*field, self.identity);
                self.graph.insert(
                    id,
                    Node::Reactive(ReactiveNode {
                        field: *field,
                        cached,
                    }),
                    parent,
                    depth,
                );
                self.reactive_nodes.insert(id);
                id
            }
            Binding::Derived(d) => {
                let id = self.graph.alloc_id();
                let inner =
                    self.build_filter_node(&d.inner, ParentLink::DerivedInner(id), depth + 1);
                let cached = match self.graph.wide_concrete(inner) {
                    Some(cf) => {
                        let events: Vec<nostr::Event> = self
                            .store
                            .query(&cf.to_nostr())
                            .into_iter()
                            .map(|se| se.event)
                            .collect();
                        project_events(&events, &d.project)
                    }
                    None => ResolvedSet::new(),
                };
                self.graph.insert(
                    id,
                    Node::Derived(DerivedNode {
                        inner,
                        project: d.project.clone(),
                        cached,
                    }),
                    parent,
                    depth,
                );
                id
            }
            Binding::SetOp(s) => {
                let id = self.graph.alloc_id();
                let operands: Vec<NodeId> = s
                    .operands
                    .iter()
                    .map(|op| self.build_binding_node(op, ParentLink::SetOpOperand(id), depth + 1))
                    .collect();
                let cached = {
                    let sets: Vec<&ResolvedSet> = operands
                        .iter()
                        .map(|o| self.graph.resolved_set_of(*o))
                        .collect();
                    resolve_setop(s.op, &sets)
                };
                self.graph.insert(
                    id,
                    Node::SetOp(SetOpNode {
                        op: s.op,
                        operands,
                        cached,
                    }),
                    parent,
                    depth,
                );
                id
            }
        }
    }
}
