//! [`Engine`] — the graph engine, atom refcounting, identity register, and
//! metrics (M1 plan §2.3, §3, §4). This is the only module that touches the
//! `EventStore`; `graph.rs` holds the pure graph data + read-only algorithm,
//! `eval.rs` holds pure leaf computations.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::{Rc, Weak};

use nmp_grammar::{
    AccessContext, Binding, ConcreteFilter, ContextualAtom, Demand, DemandDelta, DemandOp,
    DescriptorHash, Filter, SourceAuthority,
};
use nmp_store::{
    AcceptOutcome, AcceptWrite, CompensateOutcome, EventStore, InsertOutcome, PersistenceError,
    RelayObserved, StoredEvent,
};
use nostr::filter::MatchEventOptions;
use nostr::RelayUrl;

/// Full result of relay ingest when a verified relay copy also satisfies
/// locally-pending write owners of the same canonical event.
pub struct RelayIngestResult {
    pub delta: DemandDelta,
    pub satisfied_intents: Vec<(nmp_store::IntentId, nostr::Event)>,
    /// App/query handles whose row projection can have changed as a direct
    /// consequence of this committed batch. This is the post-commit
    /// subscription-notification set: callers need not re-query unrelated
    /// handles merely to prove they stayed unchanged.
    pub affected_handles: BTreeSet<HandleId>,
    /// Exact canonical row facts produced by the committed writer batch.
    /// Simple app projections can apply these facts directly instead of
    /// re-reading and re-materializing their entire prior result set merely
    /// to discover one `Added`, `SourcesGrew`, or `Removed` delta.
    pub row_changes: CommittedRowChanges,
}

/// Exact live-query consequences of one already-committed canonical store
/// mutation. The store door remains the sole authority for what became
/// current; this value only carries those durable facts across the
/// resolver/engine boundary so simple projections need not rediscover them
/// by replaying their full history.
pub struct CommittedMutationResult {
    pub delta: DemandDelta,
    pub affected_handles: BTreeSet<HandleId>,
    pub row_changes: CommittedRowChanges,
}

/// Full result of durable local acceptance: the unchanged governed store
/// outcome plus its exact post-commit live-query consequences.
pub struct LocalAcceptResult {
    pub outcome: AcceptOutcome,
    pub committed: CommittedMutationResult,
}

/// One canonical current row together with every relay in this writer batch
/// that observed it. For a new row this is its complete initial source set;
/// for provenance growth callers union these sources into remembered state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedCurrentRow {
    pub event: nostr::Event,
    pub observed_relays: BTreeSet<RelayUrl>,
}

/// Net canonical event changes after consolidating transient rows inside one
/// governed batch. An event inserted and then superseded/deleted in the same
/// transaction appears in neither `inserted` nor `removed`, because it was
/// never visible before or after the commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommittedRowChanges {
    pub inserted: Vec<CommittedCurrentRow>,
    pub removed: Vec<nostr::Event>,
    pub provenance_grew: Vec<CommittedCurrentRow>,
}

fn committed_current_row(row: &StoredEvent) -> CommittedCurrentRow {
    CommittedCurrentRow {
        event: row.event.clone(),
        observed_relays: row.provenance.seen.keys().cloned().collect(),
    }
}

/// Consolidate exact current rows and removals to the durable before/after
/// visibility delta. The overlap rule mirrors relay-batch consolidation: an
/// id present on both sides was transient inside the mutation and therefore
/// was never a distinct app-visible row transition.
fn committed_row_changes(
    inserted: impl IntoIterator<Item = StoredEvent>,
    removed: impl IntoIterator<Item = nostr::Event>,
) -> CommittedRowChanges {
    let mut inserted_by_id = BTreeMap::<nostr::EventId, CommittedCurrentRow>::new();
    for row in inserted {
        let current = committed_current_row(&row);
        inserted_by_id
            .entry(current.event.id)
            .and_modify(|prior| {
                prior
                    .observed_relays
                    .extend(current.observed_relays.iter().cloned());
            })
            .or_insert(current);
    }
    let removed_by_id: BTreeMap<_, _> =
        removed.into_iter().map(|event| (event.id, event)).collect();
    let transient: BTreeSet<_> = inserted_by_id
        .keys()
        .filter(|event_id| removed_by_id.contains_key(event_id))
        .copied()
        .collect();
    CommittedRowChanges {
        inserted: inserted_by_id
            .into_iter()
            .filter_map(|(event_id, row)| (!transient.contains(&event_id)).then_some(row))
            .collect(),
        removed: removed_by_id
            .into_iter()
            .filter_map(|(event_id, event)| (!transient.contains(&event_id)).then_some(event))
            .collect(),
        provenance_grew: Vec::new(),
    }
}

use crate::eval::{project_events, resolve_reactive, resolve_setop};
use crate::graph::{
    DerivedNode, FilterNodeData, Graph, LiteralNode, Node, ReactiveNode, SetOpNode,
};
use crate::types::{Element, FieldSlot, NodeId, ParentLink, ResolvedSet};

/// The descriptor value of a live query: a full [`Demand`] (#106) --
/// `selection + source + access + cache`, not a bare `Filter`. Two `Demand`s
/// with the same `Filter` but different `source`/`access` are DIFFERENT
/// subscriptions with distinct atom/wire/coverage identity (bug-class ledger
/// #18); `cache` does NOT participate in that identity (see
/// [`AcquisitionKey`]) -- it is a per-handle row-projection flag only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiveQuery(pub Demand);

impl LiveQuery {
    /// Convenience constructor applying `Demand`'s static default
    /// (`Demand::from_filter`) to a bare `Filter` -- the common case,
    /// unchanged in outward behavior from M1's `LiveQuery(Filter)`.
    pub fn from_filter(selection: Filter) -> Self {
        Self(Demand::from_filter(selection))
    }
}

/// The cache-FREE portion of a [`Demand`] that determines graph/atom/wire/
/// coverage sharing (#106, atlas's resolver-threading forward-note): two
/// `Demand`s differing ONLY in `cache` dedup onto the SAME graph node, the
/// SAME atoms, and the SAME wire/coverage history -- `cache` never widens
/// what's shared, it only selects which cached rows a given HANDLE's own
/// projection later serves (`nmp-engine`'s `rows_and_evidence_for`, #107).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct AcquisitionKey {
    selection: Filter,
    source: SourceAuthority,
    access: AccessContext,
}

impl From<&Demand> for AcquisitionKey {
    fn from(d: &Demand) -> Self {
        Self {
            selection: d.selection.clone(),
            source: d.source.clone(),
            access: d.access,
        }
    }
}

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
    /// This handle's OWN `CacheMode` (#106) -- never shared with sibling
    /// handles on the same (cache-free-deduped) graph node; read by
    /// `nmp-engine`'s row-projection layer (#107), not consumed inside the
    /// resolver itself.
    cache: nmp_grammar::CacheMode,
    pending_drops: Weak<RefCell<Vec<HandleId>>>,
}

impl QueryHandle {
    pub fn id(&self) -> HandleId {
        self.id
    }

    pub fn cache(&self) -> nmp_grammar::CacheMode {
        self.cache
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
    closes: Vec<ContextualAtom>,
    opens: Vec<ContextualAtom>,
}

impl DeltaAcc {
    fn push_close(&mut self, atom: ContextualAtom) {
        self.closes.push(atom);
    }

    fn push_open(&mut self, atom: ContextualAtom) {
        self.opens.push(atom);
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
    descriptor: AcquisitionKey,
    refcount: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectionShape {
    root_atoms: BTreeSet<ConcreteFilter>,
    subtree_atoms: BTreeSet<ContextualAtom>,
}

/// The graph engine (M1 plan §2.3): owns the store, the graph, the
/// descriptor/atom refcount tables, the identity register, and metrics.
pub struct Engine<S: EventStore> {
    store: S,
    graph: Graph,
    descriptor_to_root: BTreeMap<AcquisitionKey, NodeId>,
    graph_entries: HashMap<NodeId, GraphEntry>,
    handle_to_root: HashMap<HandleId, NodeId>,
    next_handle: u64,
    /// The demand truth (M1 plan §3.2, re-keyed on [`ContextualAtom`] per
    /// #106): every atom -- selection + source + access -- any live
    /// FilterNode currently contributes, refcounted. Open fires on 0->1,
    /// close on 1->0. Two atoms with identical `ConcreteFilter` but
    /// different context refcount INDEPENDENTLY (Fable D: coalescing is
    /// equal-context-only).
    atoms: BTreeMap<DescriptorHash, (ContextualAtom, u32)>,
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

    pub fn active_demand(&self) -> BTreeSet<ContextualAtom> {
        self.atoms.values().map(|(atom, _)| atom.clone()).collect()
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
    /// atoms (contrast with [`Self::subtree_atoms`]/`Graph::
    /// atoms_in_structural_order`, which walk the WHOLE subtree — the
    /// former for coverage, the latter purely for demand-set refcounting).
    /// `EngineCore` uses this to know which store ROWS a handle's
    /// `EmitRows` should be computed over — delivery shape is root-only and
    /// unchanged by #12/#49; only coverage/evidence widens to the subtree.
    /// Empty for an unknown handle.
    pub fn root_atoms(&self, id: HandleId) -> BTreeSet<ConcreteFilter> {
        let Some(&root) = self.handle_to_root.get(&id) else {
            return BTreeSet::new();
        };
        self.graph.cached_filters_of(root)
    }

    /// Every atom in `id`'s subscription's FULL subtree — interior
    /// `Derived` inner-filter atoms INCLUDED, not just the root FilterNode's
    /// own atoms (contrast with [`Self::root_atoms`], which deliberately
    /// excludes the subtree and is used for row computation only). Built on
    /// [`Graph::atoms_in_structural_order`], the exact same walk
    /// `subscribe`/`unsubscribe` already use for refcounting — this is a
    /// coverage-facing READ over that walk, never a second source of truth,
    /// and changes nothing about refcounting or wire Open/Close.
    ///
    /// This is the #12 fix's input: a query's coverage/acquisition-evidence
    /// must be computed over every atom a `Derived` binding depends on, not
    /// only the query's own root atoms — otherwise a query can report
    /// itself settled while an inner expansion (e.g. the kind:3 follow-list
    /// atom under a `$myFollows`-shaped query) is still entirely unproven.
    /// Empty for an unknown handle.
    pub fn subtree_atoms(&self, id: HandleId) -> BTreeSet<ContextualAtom> {
        let Some(&root) = self.handle_to_root.get(&id) else {
            return BTreeSet::new();
        };
        self.graph
            .atoms_in_structural_order(root)
            .into_iter()
            .collect()
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

    pub fn set_active_pubkey(
        &mut self,
        pk: Option<nostr::PublicKey>,
    ) -> Result<DemandDelta, PersistenceError> {
        let drop_delta = self.drain_pending_drops();
        self.identity = pk;
        let seed: BTreeSet<NodeId> = self.reactive_nodes.iter().copied().collect();
        if seed.is_empty() {
            return Ok(drop_delta);
        }
        self.metrics.recompute_passes += 1;
        Ok(merge_deltas(drop_delta, self.run_recompute(seed)?))
    }

    // ---- subscribe / unsubscribe (M1 plan §4) ---------------------------

    pub fn subscribe(
        &mut self,
        q: LiveQuery,
    ) -> Result<(QueryHandle, DemandDelta), PersistenceError> {
        let drop_delta = self.drain_pending_drops();
        let handle_id = self.alloc_handle();
        let key = AcquisitionKey::from(&q.0);
        let cache = q.0.cache;

        if let Some(&root) = self.descriptor_to_root.get(&key) {
            // Identical cache-free acquisition identity already has a
            // graph: graph-level dedup (M1 plan §3.2/§4; #106 widens the
            // key to selection+source+access, still cache-free per atlas's
            // forward-note -- two Demands differing only in `cache` share
            // this same graph/atoms/wire/coverage). Bump the graph refcount
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
                cache,
                pending_drops: Rc::downgrade(&self.pending_drops),
            };
            return Ok((handle, merge_deltas(drop_delta, acc.into_delta())));
        }

        let (source, access) = q.0.atom_context();
        let root = self.build_filter_node(&q.0.selection, source, access, ParentLink::Root, 0)?;
        self.descriptor_to_root.insert(key.clone(), root);
        self.graph_entries.insert(
            root,
            GraphEntry {
                descriptor: key,
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
            cache,
            pending_drops: Rc::downgrade(&self.pending_drops),
        };
        Ok((handle, merge_deltas(drop_delta, acc.into_delta())))
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

    pub fn ingest(&mut self, events: Vec<nostr::Event>) -> Result<DemandDelta, PersistenceError> {
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
    ///
    /// Sorts each event's store outcome into `inserted` (what now matches
    /// queries that didn't before) and `removed` (what no longer matches
    /// anything, as full events — retraction-and-negative-deltas.md §1.1/
    /// §1.4's "ingest commit" feeders): `Superseded { replaced }` and
    /// `Kind5Processed { deleted }` BOTH contribute their evicted rows to
    /// `removed` alongside pushing the arriving event itself to `inserted`
    /// — a superseding kind:39002 that drops my `#p` tag no longer matches
    /// the SAME inner filter its predecessor did, so without `replaced`
    /// feeding `removed` too, the dirty-seed would miss it exactly the way
    /// a kind:5/expiry retraction would (§0's "by luck of shape overlap"
    /// finding).
    pub fn ingest_observed(
        &mut self,
        events: Vec<(nostr::Event, RelayObserved)>,
    ) -> Result<DemandDelta, PersistenceError> {
        Ok(self.ingest_observed_detailed(events)?.delta)
    }

    pub fn ingest_observed_detailed(
        &mut self,
        events: Vec<(nostr::Event, RelayObserved)>,
    ) -> Result<RelayIngestResult, PersistenceError> {
        // Snapshot only graph/filter shapes, never store rows. This is the
        // cheap side of targeted invalidation: after recompute we can tell
        // exactly which shared roots changed without re-querying every
        // handle's current result set.
        let before_shapes = self.projection_shapes();
        let mut inserted: Vec<nostr::Event> = Vec::new();
        let mut removed: Vec<nostr::Event> = Vec::new();
        let mut provenance_grew: Vec<nostr::Event> = Vec::new();
        let mut satisfied_intents = Vec::new();
        let mut observed_by_id =
            BTreeMap::<nostr::EventId, (nostr::Event, BTreeSet<RelayUrl>)>::new();
        for (event, observed) in &events {
            let entry = observed_by_id
                .entry(event.id)
                .or_insert_with(|| (event.clone(), BTreeSet::new()));
            entry.1.insert(observed.relay.clone());
        }
        let input_events: Vec<_> = events.iter().map(|(event, _from)| event.clone()).collect();
        let outcomes = self.store.insert_batch(events)?;
        for (event, outcome) in input_events.into_iter().zip(outcomes) {
            match outcome {
                InsertOutcome::Inserted => inserted.push(event),
                InsertOutcome::Superseded { replaced } => {
                    inserted.push(event);
                    removed.push(replaced.event);
                }
                InsertOutcome::Kind5Processed { deleted } => {
                    inserted.push(event);
                    removed.extend(deleted.into_iter().map(|se| se.event));
                }
                // Never stored -- neither inserted nor removed: a
                // duplicate/stale event was already reflected in the store,
                // and a refused event (already-expired, or tombstoned)
                // never entered it at all.
                InsertOutcome::Duplicate {
                    provenance_grew: grew,
                    satisfied_intents: owners,
                } => {
                    if grew {
                        provenance_grew.push(event.clone());
                    }
                    satisfied_intents.extend(
                        owners
                            .into_iter()
                            .map(|intent_id| (intent_id, event.clone())),
                    );
                }
                InsertOutcome::Stale | InsertOutcome::Refused(_) => {}
            }
        }
        let inserted_ids: BTreeSet<_> = inserted.iter().map(|event| event.id).collect();
        let removed_ids: BTreeSet<_> = removed.iter().map(|event| event.id).collect();
        let provenance_grew_ids: BTreeSet<_> =
            provenance_grew.iter().map(|event| event.id).collect();
        let row_changes = CommittedRowChanges {
            inserted: inserted
                .iter()
                .filter(|event| !removed_ids.contains(&event.id))
                .map(|event| {
                    let (_, observed_relays) = observed_by_id
                        .get(&event.id)
                        .expect("inserted input event has observed relays");
                    CommittedCurrentRow {
                        event: event.clone(),
                        observed_relays: observed_relays.clone(),
                    }
                })
                .collect(),
            removed: removed
                .iter()
                .filter(|event| !inserted_ids.contains(&event.id))
                .cloned()
                .collect(),
            provenance_grew: provenance_grew_ids
                .into_iter()
                .filter(|event_id| {
                    !inserted_ids.contains(event_id) && !removed_ids.contains(event_id)
                })
                .map(|event_id| {
                    let (event, observed_relays) = observed_by_id
                        .get(&event_id)
                        .expect("duplicate input event has observed relays");
                    CommittedCurrentRow {
                        event: event.clone(),
                        observed_relays: observed_relays.clone(),
                    }
                })
                .collect(),
        };
        let changed_events: Vec<_> = inserted
            .iter()
            .chain(removed.iter())
            .chain(provenance_grew.iter())
            .cloned()
            .collect();
        // A duplicate whose provenance grew can change selector routing
        // evidence even though its projected VALUE is unchanged. Seed the
        // same generic Derived recompute lane so the old atom is replaced
        // by one carrying the enlarged source-relay set.
        let mut inserted_or_provenance_changed = inserted;
        inserted_or_provenance_changed.extend(provenance_grew);
        let delta = self.react(inserted_or_provenance_changed, removed)?;
        let affected_handles = self.affected_handles(&before_shapes, &changed_events);
        Ok(RelayIngestResult {
            delta,
            satisfied_intents,
            affected_handles,
            row_changes,
        })
    }

    /// One shape per shared acquisition root. Multiple handles that dedup to
    /// the same root share this snapshot and therefore share the same
    /// invalidation decision; their per-handle cache projection is applied
    /// later by `nmp-engine`.
    fn projection_shapes(&self) -> HashMap<NodeId, ProjectionShape> {
        self.handle_to_root
            .values()
            .copied()
            .map(|root| {
                let shape = ProjectionShape {
                    root_atoms: self.graph.cached_filters_of(root),
                    subtree_atoms: self
                        .graph
                        .atoms_in_structural_order(root)
                        .into_iter()
                        .collect(),
                };
                (root, shape)
            })
            .collect()
    }

    /// Resolve committed row/graph changes to the exact live handles that
    /// may project different rows or evidence. Matching is intentionally the
    /// same generic Nostr filter predicate used by resolver dirty-marking;
    /// there are no kind/tag special cases here.
    fn affected_handles(
        &self,
        before_shapes: &HashMap<NodeId, ProjectionShape>,
        changed_events: &[nostr::Event],
    ) -> BTreeSet<HandleId> {
        let after_shapes = self.projection_shapes();
        let affected_roots: BTreeSet<NodeId> = after_shapes
            .iter()
            .filter_map(|(root, after)| {
                let shape_changed = before_shapes.get(root) != Some(after);
                let rows_changed = !changed_events.is_empty()
                    && after.root_atoms.iter().any(|atom| {
                        let filter = atom.to_nostr();
                        changed_events
                            .iter()
                            .any(|event| filter.match_event(event, MatchEventOptions::new()))
                    });
                (shape_changed || rows_changed).then_some(*root)
            })
            .collect();

        self.handle_to_root
            .iter()
            .filter_map(|(handle, root)| affected_roots.contains(root).then_some(*handle))
            .collect()
    }

    /// The local-authorship mirror of [`Self::ingest_observed`]
    /// (`crashsafe-accepted-2-3-plan.md` §1.2, #2/#3 under epic #23): a
    /// locally-composed write enters the ONE store through the
    /// [`EventStore::accept_write`] door (local provenance +
    /// `SigState::Pending` instead of a `RelayObserved`) and its
    /// [`AcceptOutcome`] is sorted into `react`'s `inserted`/`removed`
    /// EXACTLY as a relay insert's [`InsertOutcome`] is — so the pending row
    /// is query-visible immediately, participates in `Derived` re-resolution
    /// (an optimistic kind:3 edit re-resolves follows), replaceable/
    /// addressable supersession, and the §1 negative-delta lane, with **no
    /// app optimistic mirror** and no new visibility mechanism.
    ///
    /// This method OWNS the single `accept_write` call (the store allocates
    /// the `intent_id`/`receipt_id` inside its own transaction — the one
    /// place a caller learns either), so it returns the outcome UNCHANGED
    /// alongside the `DemandDelta`: `EngineCore` (U3) reads the ids off it
    /// via [`AcceptOutcome::journaled_intent_id`]/
    /// [`AcceptOutcome::journaled_receipt_id`] to journal its `PendingWrite`
    /// and emit `Accepted`, without a second (unsound, two-transaction) door
    /// call. The outcome is matched by reference to derive the react inputs,
    /// never consumed/reconstructed. Sorting mirrors `ingest_observed`:
    /// `Inserted`/`Superseded`/`Kind5Processed` push the new row to
    /// `inserted` (and `Superseded`'s evicted predecessor / `Kind5Processed`'s
    /// hidden rows to `removed`); `Duplicate` (row already reflected) and
    /// `Stale` (no pending row produced) yield an empty delta; `Refused`
    /// yields an empty delta and carries no journal ids. A door-level
    /// persistence failure is surfaced as `Err` — the resolver graph is
    /// untouched (`accept_write` is atomic: on `Err` nothing committed).
    pub fn accept_local(
        &mut self,
        accept: AcceptWrite,
    ) -> Result<LocalAcceptResult, PersistenceError> {
        let before_shapes = self.projection_shapes();
        let outcome = self.store.accept_write(accept)?;
        let mut inserted_rows: Vec<StoredEvent> = Vec::new();
        let mut removed_rows: Vec<nostr::Event> = Vec::new();
        match &outcome {
            AcceptOutcome::Inserted { row, .. } => inserted_rows.push(row.clone()),
            AcceptOutcome::Superseded { row, replaced, .. } => {
                inserted_rows.push(row.clone());
                removed_rows.push(replaced.event.clone());
            }
            AcceptOutcome::Kind5Processed { row, hidden, .. } => {
                inserted_rows.push(row.clone());
                removed_rows.extend(hidden.iter().map(|se| se.event.clone()));
            }
            // Never a new query fact -- empty delta: a `Duplicate` row was
            // already reflected in the store (relay echo / co-owner join),
            // a `Stale` intent produced no pending row (lost its address
            // race), and a `Refused` intent never entered the store at all.
            AcceptOutcome::Duplicate { .. }
            | AcceptOutcome::Stale { .. }
            | AcceptOutcome::Refused(_) => {}
        }
        let inserted_events: Vec<_> = inserted_rows.iter().map(|row| row.event.clone()).collect();
        let changed_events: Vec<_> = inserted_events
            .iter()
            .chain(removed_rows.iter())
            .cloned()
            .collect();
        let row_changes = committed_row_changes(inserted_rows, removed_rows.clone());
        let delta = self.react(inserted_events, removed_rows)?;
        let affected_handles = self.affected_handles(&before_shapes, &changed_events);
        Ok(LocalAcceptResult {
            outcome,
            committed: CommittedMutationResult {
                delta,
                affected_handles,
                row_changes,
            },
        })
    }

    /// Apply the graph invalidation produced by the store's atomic
    /// pre-signature compensation door. The store mutation has already
    /// happened when this is called; this method only feeds its exact
    /// removed/inserted facts through the same symmetric `react` lane used
    /// by relay ingest and expiry.
    /// Carries the exact restored/revealed rows and removed pending row that
    /// the store committed atomically.
    pub fn react_to_compensation(
        &mut self,
        removed_pending: nostr::Event,
        outcome: &CompensateOutcome,
    ) -> Result<CommittedMutationResult, PersistenceError> {
        let before_shapes = self.projection_shapes();
        match outcome {
            CompensateOutcome::Compensated { restored, revealed } => {
                let mut inserted_rows: Vec<StoredEvent> = revealed.clone();
                if let Some(restored) = restored {
                    inserted_rows.push((**restored).clone());
                }
                let inserted_events: Vec<_> =
                    inserted_rows.iter().map(|row| row.event.clone()).collect();
                let mut changed_events = inserted_events.clone();
                changed_events.push(removed_pending.clone());
                let row_changes = committed_row_changes(inserted_rows, [removed_pending.clone()]);
                let delta = self.react(inserted_events, vec![removed_pending])?;
                let affected_handles = self.affected_handles(&before_shapes, &changed_events);
                Ok(CommittedMutationResult {
                    delta,
                    affected_handles,
                    row_changes,
                })
            }
            CompensateOutcome::NotFound => Ok(CommittedMutationResult {
                delta: DemandDelta::default(),
                affected_handles: BTreeSet::new(),
                row_changes: CommittedRowChanges::default(),
            }),
        }
    }

    /// Seed dirty-marks from removals that arrive with NO inbound event at
    /// all (retraction-and-negative-deltas.md §1.2/§1.4: NIP-40 expiry,
    /// optimistic-write rejection) — feeds `removed` into the SAME `react`
    /// `ingest_observed` uses, on the removed side only. The caller (M3's
    /// `EngineCore`) is responsible for having already removed these rows
    /// from the store itself (`EventStore::expire_due`/`remove`) before
    /// calling this: `retract` only re-evaluates the graph, it never
    /// touches the store door.
    pub fn retract(
        &mut self,
        removed: Vec<nostr::Event>,
    ) -> Result<CommittedMutationResult, PersistenceError> {
        let before_shapes = self.projection_shapes();
        let changed_events = removed.clone();
        let row_changes = committed_row_changes(Vec::<StoredEvent>::new(), removed.clone());
        let delta = self.react(Vec::new(), removed)?;
        let affected_handles = self.affected_handles(&before_shapes, &changed_events);
        Ok(CommittedMutationResult {
            delta,
            affected_handles,
            row_changes,
        })
    }

    /// The one recompute engine (retraction-and-negative-deltas.md §1.2):
    /// seeds dirty-marks from events that newly match (`inserted`) OR
    /// events that no longer match anything because they left the store
    /// (`removed`), running the IDENTICAL `match_event` test over both —
    /// symmetric by construction, not by luck of shape overlap. A `Derived`
    /// node is seeded if its inner filter matches an inserted OR a removed
    /// event; `recompute_node` already re-queries the store fresh (the
    /// store no longer holds a removed row by the time this runs), so the
    /// recomputed `ResolvedSet` shrinks by exactly the retracted members
    /// and the parent `FilterNode`'s atom diff closes exactly their atoms —
    /// replace-not-rebuild extends unchanged to retraction, and the
    /// `Metrics` witness (`atoms_opened + atoms_closed ==
    /// |symmetric diff|`) holds with zero new bookkeeping.
    /// `ingest_observed`/`retract` are both thin callers of this; the four
    /// §1.4 feeders differ only in who populates `removed`.
    fn react(
        &mut self,
        inserted: Vec<nostr::Event>,
        removed: Vec<nostr::Event>,
    ) -> Result<DemandDelta, PersistenceError> {
        let drop_delta = self.drain_pending_drops();
        if inserted.is_empty() && removed.is_empty() {
            return Ok(drop_delta);
        }
        self.metrics.recompute_passes += 1;

        // Dirty-mark phase (GENERIC — the kill guard, M1 plan §3.3 step 2 /
        // test 10). The ONLY thing that decides whether an event affects a
        // Derived BindingNode is `match_event` against that node's own
        // inner FilterNode's concrete filter. No kind literal, no
        // per-shape branch, anywhere in this decision.
        let mut seed: BTreeSet<NodeId> = BTreeSet::new();
        for derived_id in self.graph.derived_node_ids() {
            let Node::Derived(d) = self.graph.node(derived_id) else {
                unreachable!("derived_node_ids only returns Derived node ids")
            };
            if let Some(cf) = self.graph.wide_concrete(d.inner) {
                let nf = cf.to_nostr();
                if inserted
                    .iter()
                    .chain(removed.iter())
                    .any(|e| nf.match_event(e, MatchEventOptions::new()))
                {
                    seed.insert(derived_id);
                }
            }
        }
        Ok(merge_deltas(drop_delta, self.run_recompute(seed)?))
    }

    // ---- recompute rounds (shared by ingest + re-root) ------------------

    /// Process `pending` in rounds by strictly descending depth, so every
    /// child is fully resolved before its parent is recomputed — the
    /// invariant that makes a single pass correct even when a node (e.g. a
    /// `SetOp`) has multiple simultaneously-dirty children (M1 plan §3.3).
    fn run_recompute(
        &mut self,
        mut pending: BTreeSet<NodeId>,
    ) -> Result<DemandDelta, PersistenceError> {
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
                let changed = self.recompute_node(id, &mut acc)?;
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
        Ok(acc.into_delta())
    }

    /// Read the event set an interior projection is actually defined over.
    /// An explicit NIP-01 limit selects the newest `N` events before the
    /// closed [`nmp_grammar::Selector`] is applied; an unlimited filter still
    /// needs the complete set. Keeping this distinction here prevents a bounded
    /// `Derived` node from silently turning into an unbounded local scan.
    fn projection_input_events(
        &self,
        filter: &ConcreteFilter,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        let nostr_filter = filter.to_nostr();
        let rows = match filter.limit {
            Some(limit) => self.store.query_newest(&nostr_filter, limit)?,
            None => self.store.query(&nostr_filter)?,
        };
        Ok(rows)
    }

    /// Recompute node `id`'s value from its (already-current) children.
    /// Returns whether the value changed. For a FilterNode, also diffs +
    /// applies its atom-set change into the global demand table via `acc`.
    fn recompute_node(&mut self, id: NodeId, acc: &mut DeltaAcc) -> Result<bool, PersistenceError> {
        // Snapshot the node (cheap: M1 graphs are tiny) so we can mutate
        // `self.graph`/`self.store` in the match arms without fighting the
        // borrow checker over a live reference into `self.graph.nodes`.
        let snapshot = self.graph.node(id).clone();
        let changed = match snapshot {
            Node::Literal(_) => false,
            Node::Reactive(n) => {
                let new = resolve_reactive(n.field, self.identity);
                self.graph.set_reactive_cached(id, new)
            }
            Node::Derived(n) => {
                let new = match self.graph.wide_concrete(n.inner) {
                    Some(cf) => project_events(&self.projection_input_events(&cf)?, &n.project),
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
                    return Ok(false);
                }
                for a in old_atoms.difference(&new_atoms) {
                    self.unref_atom(a, acc);
                }
                for a in new_atoms.difference(&old_atoms) {
                    self.ref_atom(a, acc);
                }
                true
            }
        };
        Ok(changed)
    }

    // ---- atom refcounting (M1 plan §3.2/§4) -----------------------------

    /// Refcounts a [`ContextualAtom`] (identity domain) and pushes the SAME
    /// full atom into `DemandDelta` (#106, Fable's ratified shape --
    /// `DemandOp::Open/Close(ContextualAtom)`, not a bare `ConcreteFilter`):
    /// the delta reflects exactly what the refcount table keys on. Two
    /// atoms with the same `filter` but different `source`/`access`
    /// refcount in SEPARATE buckets, so BOTH can surface an `Open` here --
    /// downstream (the router's per-relay context partitioning + wire-
    /// domain `ConcreteFilter::hash`/`SubId::for_wire`) is what keeps their
    /// WIRE identity distinct without ever needing `ConcreteFilter` itself
    /// to widen (two-hash-domains).
    fn ref_atom(&mut self, atom: &ContextualAtom, acc: &mut DeltaAcc) {
        let hash = atom.hash();
        let entry = self.atoms.entry(hash).or_insert_with(|| (atom.clone(), 0));
        entry.1 += 1;
        if entry.1 == 1 {
            self.metrics.atoms_opened += 1;
            acc.push_open(atom.clone());
        }
    }

    fn unref_atom(&mut self, atom: &ContextualAtom, acc: &mut DeltaAcc) {
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

    /// `source`/`access` are the OWNING `Demand`'s context (#106), threaded
    /// in from the caller rather than re-derived here -- this FilterNode may
    /// be the root of a top-level `LiveQuery`'s `Demand` or the `inner` of a
    /// `Binding::Derived`'s OWN (independent) `Demand`; either way, by the
    /// time we're building the FilterNode the context has already been
    /// decided one level up.
    fn build_filter_node(
        &mut self,
        filter: &Filter,
        source: SourceAuthority,
        access: AccessContext,
        parent: ParentLink,
        depth: u32,
    ) -> Result<NodeId, PersistenceError> {
        let id = self.graph.alloc_id();

        let mut bound = Vec::new();
        if let Some(b) = &filter.authors {
            let bid = self.build_binding_node(b, ParentLink::FilterField(id), depth + 1)?;
            bound.push((FieldSlot::Authors, bid));
        }
        if let Some(b) = &filter.ids {
            let bid = self.build_binding_node(b, ParentLink::FilterField(id), depth + 1)?;
            bound.push((FieldSlot::Ids, bid));
        }
        for (tag, b) in &filter.tags {
            let bid = self.build_binding_node(b, ParentLink::FilterField(id), depth + 1)?;
            bound.push((FieldSlot::Tag(*tag), bid));
        }

        let data = FilterNodeData {
            kinds: filter.kinds.clone(),
            since: filter.since,
            until: filter.until,
            limit: filter.limit,
            bound,
            cached_atoms: BTreeSet::new(),
            source,
            access,
        };
        self.graph.insert(id, Node::Filter(data), parent, depth);
        let atoms = self.graph.compute_atoms(id);
        self.graph.set_filter_cached_atoms(id, atoms);
        Ok(id)
    }

    fn build_binding_node(
        &mut self,
        binding: &Binding,
        parent: ParentLink,
        depth: u32,
    ) -> Result<NodeId, PersistenceError> {
        match binding {
            Binding::Literal(set) => {
                let id = self.graph.alloc_id();
                let resolved: ResolvedSet = set.iter().cloned().map(Element::Scalar).collect();
                self.graph
                    .insert(id, Node::Literal(LiteralNode { resolved }), parent, depth);
                Ok(id)
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
                Ok(id)
            }
            Binding::Derived(d) => {
                let id = self.graph.alloc_id();
                // `d.inner` is its OWN `Demand` (#106): its `source`/
                // `access` come from ITSELF, never from the enclosing
                // FilterNode's context -- a Demand's context is never
                // inherited across a `Binding::Derived` boundary.
                let (inner_source, inner_access) = d.inner.atom_context();
                let inner = self.build_filter_node(
                    &d.inner.selection,
                    inner_source,
                    inner_access,
                    ParentLink::DerivedInner(id),
                    depth + 1,
                )?;
                let cached = match self.graph.wide_concrete(inner) {
                    Some(cf) => project_events(&self.projection_input_events(&cf)?, &d.project),
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
                Ok(id)
            }
            Binding::SetOp(s) => {
                let id = self.graph.alloc_id();
                let operands: Vec<NodeId> = s
                    .operands
                    .iter()
                    .map(|op| self.build_binding_node(op, ParentLink::SetOpOperand(id), depth + 1))
                    .collect::<Result<_, _>>()?;
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
                Ok(id)
            }
        }
    }
}
