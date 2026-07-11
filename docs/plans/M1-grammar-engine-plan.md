# M1 — Grammar-engine spike: implementation plan

- **Date:** 2026-07-11
- **Status:** Provisional-until-v2 (no self-compat obligation). Builder-facing plan for M1 per `docs/VISION.md` §6.
- **Milestone:** M1 — headless binding resolver over an in-memory store with a scripted fake-relay harness. NO persistence, NO real transport, NO FFI, NO crypto, NO wire/relay planning.
- **Gate:** settled by its own contract tests, running headless (§5 below). These tests ARE the M1 pass criteria.
- **Folds in:** M0 completeness-audit amendments (1–3) + M0-gate refuter amendments (1–5, message of 2026-07-11).

M1 proves the crown jewel (VISION §2 P2) on the REAL path — event ingested → replaceable supersede → binding re-eval → surgical demand delta — at two different depths, an identity re-root, and a set-algebra composition. It emits **abstract demand-set deltas** (sets of concrete resolved filters to open/close), NOT per-relay wire plans. Replace-not-rebuild and recompile-not-reopen must hold at every node; at most one compile-invalidation per ingest batch.

The pre-committed **kill** (VISION §6 M1): surgical deltas require per-shape special-casing (the code grows "the kind:3 case" and "the 39002 case"), OR replace-not-rebuild needs O(rebuild) work. §6 below is the instrumentation that makes either kill *visible* rather than hidden.

---

## 1. Crate layout

A four-member Cargo workspace. Few crates, YAGNI, clear one-directional dependency. Nightly toolchain is already pinned (`rust-toolchain.toml`). The only external protocol dependency is the **`nostr`** crate (Event/Filter/Keys/Tag/EventBuilder) — **not** `nostr-sdk`.

```
nostr (external crate)
  ├── nmp-grammar     value types only (Filter, Binding, Selector, ConcreteFilter, DemandOp/Delta, hashing)
  ├── nmp-store       EventStore trait + MemoryStore (the one insert door + query)
  └── nmp-resolver    the graph engine, atom refcounting, identity register, metrics   → deps: nmp-grammar, nmp-store
        └── src/testkit.rs   the fake-relay / ingest harness + event builders (compiled always; small)
        └── tests/           the M1 contract tests (integration)  ← the pass criteria
```

**Dependency direction.** `nmp-grammar` and `nmp-store` are siblings, each depending only on `nostr`; `nmp-resolver` depends on both. Nothing depends on `nmp-resolver`. No cycles.

**Why this split (not one crate):** it buys real builder parallelism (grammar and store share zero files → two Sonnet builders in parallel) and gives the resolver a clean seam to test the store in isolation. It is still "few crates." The harness lives *inside* `nmp-resolver` as `pub mod testkit` (not its own crate) because it needs the private `Engine` surface and nothing else consumes it in M1.

**Wrap `nostr::Filter` or define our own?** Define our own `Filter`/`Binding`/`ConcreteFilter`, and **lower to `nostr::Filter` only when concrete**. Justification:
1. `nostr::Filter` field values are literal sets; they structurally *cannot* hold a `Binding` (the whole grammar). So the live-query `Filter` must be ours.
2. The *resolved* form (`ConcreteFilter`) is the unit of the demand set and of the refcount/dedup key. We need a **canonical, `Ord`+`Hash`-stable** representation (sorted `BTreeSet`s, single-char tag keys) so that descriptor hashing, atom diffing, and refcounting are byte-deterministic. `nostr::Filter`'s internal tag/`Hash` representation is not guaranteed canonical for that purpose.
3. We still get rust-nostr's value: `ConcreteFilter::to_nostr()` lowers to `nostr::Filter`, and **event↔filter matching reuses `nostr::Filter::match_event`** (do not hand-roll matching — memory rule "use rust-nostr, not scratch logic"). The store also queries by lowered `nostr::Filter`, so the store stays ignorant of the grammar.

---

## 2. Core types (sketches — fields + key signatures, not bodies)

### 2.1 `nmp-grammar`

> **Superseded by [#64](https://github.com/pablof7z/nmp/issues/64):** the
> `TagName`/whitelist sketch below is the M1-era shape this plan originally
> shipped. #64 split it in two: `IndexedTagName` (any of the 52 ASCII
> letters, no whitelist) for `Filter.tags`/`ConcreteFilter.tags` — the
> wire/local indexed-filter alphabet only — and a plain `String` for
> `Selector::Tag`, an arbitrary already-acquired event-tag key that never
> inherits the wire filter's single-letter restriction. The sketch is left
> as-is below as a historical record of the M1 milestone's original design,
> not a currently-authoritative type.

```rust
/// A single-letter Nostr tag name, PARAMETERIZED — never per-tag enum variants.
/// Valid single-letter set: p, e, a, d, E, t, q, h (validated at construction).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagName(char);
impl TagName { pub fn new(c: char) -> Option<Self>; pub fn as_char(&self) -> char; }

/// CLOSED, introspectable projection vocabulary. Never an app closure.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Selector {
    Authors,          // project each event's author pubkey
    Ids,              // project each event's id
    Tag(TagName),     // project each value of a single-letter tag (parameterized)
    AddressCoord,     // project the a-coordinate(s): (kind, author, d) — CO-PINNED, see §3.5
}

/// The reactive identity root. App sets it; engine reacts. Extensible.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityField { ActivePubkey /* future: ActiveRelayList, ... — do not forbid */ }

/// Every bindable filter-field value.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Binding {
    Literal(BTreeSet<String>),          // fixed hex/tag-value set
    Reactive(IdentityField),            // e.g. $currentPubkey — legal in authors AND in any tag field
    Derived(Box<Derived>),              // result of an inner Filter projected through a Selector
    SetOp(Box<SetOp>),                  // set algebra over child bindings (M0-refuter amendment #1)
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Derived { pub inner: Filter, pub project: Selector }

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SetOp { pub op: SetAlgebra, pub operands: Vec<Binding> }

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SetAlgebra { Union, Intersect, Diff }   // Diff is non-negotiable: "follows MINUS mutes"

/// A live-query filter whose field values may be Bindings.
/// kinds are LITERAL in M1 (not bindable) — simplest, matches every falsifier; extensible later.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Filter {
    pub kinds:   Option<BTreeSet<u16>>,
    pub authors: Option<Binding>,
    pub ids:     Option<Binding>,
    pub tags:    BTreeMap<TagName, Binding>,   // any Binding here, incl. Reactive(ActivePubkey) — amendment #2
    pub since:   Option<u64>,
    pub until:   Option<u64>,
    pub limit:   Option<usize>,
}

/// A fully-resolved filter — NO bindings. The unit of the demand set + refcount/dedup key.
/// Every field co-pinned: for a coordinate-derived atom, kinds/authors/#d are singletons TOGETHER.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConcreteFilter {
    pub kinds:   Option<BTreeSet<u16>>,
    pub authors: Option<BTreeSet<String>>,
    pub ids:     Option<BTreeSet<String>>,
    pub tags:    BTreeMap<TagName, BTreeSet<String>>,
    pub since:   Option<u64>,
    pub until:   Option<u64>,
    pub limit:   Option<usize>,
}
impl ConcreteFilter {
    pub fn to_nostr(&self) -> nostr::Filter;          // lowering at the boundary
    pub fn hash(&self) -> DescriptorHash;             // canonical, stable — the demand/refcount key
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DescriptorHash(u64);

/// A demand-set delta. INVARIANT: all Close ops precede all Open ops;
/// Close ops are emitted in reverse-of-open order (teardown-before-activate at every node).
pub enum DemandOp { Close(ConcreteFilter), Open(ConcreteFilter) }
pub struct DemandDelta { pub ops: Vec<DemandOp> }
impl DemandDelta {
    pub fn is_empty(&self) -> bool;
    pub fn opened(&self)  -> Vec<&ConcreteFilter>;    // convenience for assertions
    pub fn closed(&self)  -> Vec<&ConcreteFilter>;
}
```

### 2.2 `nmp-store`

```rust
pub enum InsertOutcome {
    Inserted,                              // brand-new id
    Duplicate,                             // id already present (provenance merge is a NO-OP stub in M1)
    Superseded { replaced: nostr::EventId },// replaceable/addressable winner changed
    Stale,                                 // older than current winner for this address — REJECTED
}

/// The single mutating door.
pub trait EventStore {
    fn insert(&mut self, event: nostr::Event) -> InsertOutcome;   // dedup-first, THEN supersede
    fn query(&self, filter: &nostr::Filter) -> Vec<nostr::Event>; // current winners only
}

pub struct MemoryStore { /* by_id: HashMap<EventId, Event>; addr_index: HashMap<Address, EventId>; ... */ }
impl EventStore for MemoryStore { /* ... */ }
```

Supersession rule (ledger #1, harvested semantics): id-dedup first; then for replaceable kinds `{0,3,10000..=19999}` keyed `(pubkey,kind)` and addressable `{30000..=39999}` keyed `(pubkey,kind,d)`, winner = **newest `created_at`, tie-break lexicographically-smallest id**; an older event for an existing address returns `Stale` and is dropped. M1 does **not** verify signatures (harness builds trusted events) and does **not** populate a provenance field (stub).

### 2.3 `nmp-resolver` (engine + harness surface)

```rust
pub struct LiveQuery(pub grammar::Filter);   // the descriptor value (Hashable)

pub struct HandleId(u64);
pub struct QueryHandle { id: HandleId /* + Weak back-ref for Drop-withdraw, see §4 */ }

#[derive(Default, Clone)]
pub struct Metrics {
    pub recompute_passes:   u64,   // MUST advance ≤ once per ingest batch
    pub nodes_recomputed:   u64,   // cascade depth witness
    pub sets_reevaluated:   u64,
    pub atoms_opened:       u64,   // replace-not-rebuild witness: == |symmetric diff|, NOT |set|
    pub atoms_closed:       u64,
}

pub struct Engine<S: EventStore> { store: S, /* graph, registry, atoms, identity, metrics */ }

impl<S: EventStore> Engine<S> {
    pub fn new(store: S) -> Self;
    pub fn set_active_pubkey(&mut self, pk: Option<Pubkey>) -> DemandDelta; // identity re-root
    pub fn subscribe(&mut self, q: LiveQuery) -> (QueryHandle, DemandDelta);
    pub fn unsubscribe(&mut self, id: HandleId) -> DemandDelta;
    pub fn ingest(&mut self, events: Vec<nostr::Event>) -> DemandDelta;     // THE real path (batch)
    pub fn active_demand(&self) -> BTreeSet<ConcreteFilter>;                // for assertions
    pub fn metrics(&self) -> &Metrics;
    pub fn graph_snapshot(&self) -> GraphSnapshot;                          // node/edge introspection for §6
}
```

The **harness** (`testkit`) wraps `Engine<MemoryStore>` plus event builders:

```rust
pub struct Harness { /* Engine<MemoryStore> */ }
impl Harness {
    pub fn new() -> Self;
    pub fn set_active(&mut self, pk: Option<Pubkey>) -> DemandDelta;
    pub fn subscribe(&mut self, q: LiveQuery) -> (QueryHandle, DemandDelta);
    pub fn deliver(&mut self, events: Vec<nostr::Event>) -> DemandDelta;    // scripted "relay push" → ingest
    pub fn demand(&self)  -> BTreeSet<ConcreteFilter>;
    pub fn metrics(&self) -> Metrics;
}
// Event builders (throwaway Keys are test fixtures, NOT a crypto feature; store doesn't verify sigs):
pub fn kind3(author: &Keys, follows: &[Pubkey], created_at: u64) -> nostr::Event;
pub fn kind39002(author: &Keys, group_d: &str, members: &[Pubkey], created_at: u64) -> nostr::Event;
pub fn kind10000_mutes(author: &Keys, muted: &[Pubkey], created_at: u64) -> nostr::Event;
pub fn addressable(author: &Keys, kind: u16, d: &str, created_at: u64) -> nostr::Event;
```

---

## 3. The resolver algorithm

### 3.1 Graph = a tree of BindingNodes feeding FilterNodes

Expanding a `LiveQuery`'s `Filter` produces a small dependency graph (bounded ≤3 deep; no cycles, no unbounded dataflow). Two node kinds:

- **`FilterNode`** — one `Filter` instance. Holds its literal fields (kinds, since/until/limit) plus, per bound field, a handle to a `BindingNode`. Produces **demand atoms** (§3.4).
- **`BindingNode`** — resolves a `Binding` to a `ResolvedSet` (a `BTreeSet<Element>`, element type = pubkey-hex | id-hex | tag-value | coordinate-triple):
  - `Literal(set)` → the fixed set.
  - `Reactive(ActivePubkey)` → `{active_pk}` or `{}` when unset. Depends on the identity register. **Position-agnostic:** a `Reactive` in `#p` resolves identically to one in `authors` (amendment #2) — the resolver never branches on which field a binding sits in.
  - `Derived{inner, project}` → owns a child `FilterNode` for `inner`; resolves to `project(Selector)` over `store.query(inner.concrete)` (current winners). Records its last `ResolvedSet`.
  - `SetOp{op, operands}` → owns a child `BindingNode` per operand; resolves to `fold(op)` over operand sets (`Union`/`Intersect`/`Diff`). Records its last `ResolvedSet`.

**Roots** of the dependency graph are `Literal` sets and the identity register (`Reactive`). Every node **caches its last `ResolvedSet`** (and each `FilterNode` its last atom set) — this cache *is* the "replace-not-rebuild" retained state.

### 3.2 Data structures (keyed by what)

- `nodes: SlotMap<NodeId, Node>` — the graph, one shared instance per LiveQuery descriptor.
- `registry: BTreeMap<DescriptorHash, (RootNodeId, u32 refcount)>` — identical whole-descriptor `LiveQuery`s share ONE graph (graph-level dedup + refcount).
- `atoms: BTreeMap<DescriptorHash, (ConcreteFilter, u32 refcount)>` — the **demand truth**. Union of every `FilterNode`'s atoms across all active graphs. Open fires on 0→1, close on 1→0. This is what produces surgical open/close across overlapping queries.
- `reactive_dependents: Set<NodeId>` — nodes transitively depending on `ActivePubkey`, for O(root) re-root.
- `identity: Option<Pubkey>` — the whole identity contract (VISION P3).
- `metrics: Metrics`.

### 3.3 Incremental re-eval on ingest (the real path)

`ingest(events)` — exactly one compile-invalidation per batch:

1. **Insert phase.** For each event, `store.insert(e)`. Collect only events whose outcome is `Inserted` or `Superseded` into `changed`. `Duplicate`/`Stale` contribute nothing → this is how *stale older kind:3* and *duplicate delivery* produce an empty delta with no re-eval firing.
2. **Dirty-mark phase (GENERIC — the kill guard).** For each `changed` event and each `Derived` `BindingNode`, mark it dirty iff `inner.concrete.to_nostr().match_event(event)` is true. **The only thing that decides "does this event affect this node" is `match_event` against the node's own concrete filter.** No `event.kind == 3`, no `== 39002`, anywhere. (Enforced structurally + by test 10.)
3. **Recompute phase (single pass, bottom-up).** Topologically recompute dirty nodes and their transitive parents:
   - `Derived` node: recompute `ResolvedSet = project(store.query(inner.concrete))`; diff vs cached set.
   - `SetOp` node: if any operand changed, recompute `fold(op, operands)`; diff vs cached set.
   - If a node's `ResolvedSet` is unchanged, **propagation stops here** (`sets_reevaluated`++ but no parent dirtying). This is how an *unchanged-set ingest* (a newer kind:3 listing the same members) yields an empty delta: event changed, but the projected set did not.
   - When a `ResolvedSet` changes, the consuming `FilterNode`'s bound field is marked dirty → recompute its atoms (§3.4) → diff atoms.
4. **Delta phase.** Apply every atom add/remove to the `atoms` refcount table; collect ops where refcount crossed 0→1 (`Open`) or 1→0 (`Close`). Emit ONE `DemandDelta` ordered **closes-before-opens, closes in reverse-of-open order**. `recompute_passes` advances exactly once for the whole batch.

`ingest` of a single event = a batch of one. Concurrent changes in one batch → still one pass, one delta (test 7).

### 3.4 Demand atoms — the granularity that makes deltas surgical

A `FilterNode` has a **base** (literal kinds + since/until/limit + any single-valued resolved fields) and **at most one fan-out binding** (the field bound to a set-producing `BindingNode`). It produces one **atom** (a `ConcreteFilter`) **per element** of the fan-out set:

- `authors := Derived(...→Tag(p))` resolving `{A,B,C}` → atoms `{kinds:[X],authors:{A}}`, `{…authors:{B}}`, `{…authors:{C}}`.
- `#d := Derived(...→Tag(d))` resolving `{g1,g2}` → atoms `{kinds:[…],#d:{g1}}`, `{…#d:{g2}}`.

**Why per-element and not one multi-value filter:** the falsifier requires `{A,B,C}→{A,B,D}` to yield *exactly* close-C/open-D with **zero churn on A,B**. A single `authors:{A,B,C}` filter cannot express that — changing it closes the whole old filter and opens a whole new one (rebuild). Per-element atoms make set-diff = demand-diff. This fine-grained per-element set is the **demand TRUTH**; M2's widen-only wire coalescing later re-merges `{A},{B},{D}` into one REQ `authors:[A,B,D]`. So per-element demand at M1 is *consistent with*, not contradictory to, the "don't shard authors on the wire" doctrine — sharding is a wire concern M1 doesn't reach.

**Invariant — empty set ≠ wildcard.** A fan-out binding resolving to `{}` produces **zero atoms**, never an unconstrained filter. (When active pubkey is set but the new account's kind:3 hasn't arrived, the outer author set is empty → outer atoms all close, and nothing opens until follows arrive. An empty `Derived` set must never widen to "all events.")

### 3.5 AddressCoord — one Derived does NOT always feed one field (amendment #3)

An `a`-coordinate is a `(kind, author, d)` triple that does **not** factor into independent kinds/authors/#d field-sets — the cartesian product over-matches. **M1's escape: fan out.** A `Derived{project: AddressCoord}` resolving to N coordinates produces **N distinct co-pinned atoms**, each a `ConcreteFilter` with `kinds:{k}`, `authors:{author}`, `tags[d]:{d}` pinned *together* for that one coordinate. Consequences, all naturally handled by the per-element atom model (§3.4):

- An inner change that adds/removes a coordinate changes the **number** of outer atoms (a shape change), surgically = open/close one co-pinned atom. No cartesian blow-up, no over-fetch.
- We deliberately do **not** take the "over-fetch a safe superset + local re-filter" escape in M1 — local re-filter is a delivery/wire concern (M2). Fan-out keeps M1's demand set exact.
- This is tested (test 11), because the depth-2 falsifier uses `Tag(d)` (single field) and sidesteps coordinate co-pinning.

**Scope note:** M1 supports at most **one** fan-out binding per `FilterNode`. Multiple independent `Derived` dimensions on a single node (true cartesian) is a deferred non-goal (§8) — `AddressCoord` is the co-pinned multi-field case and is handled; independent multi-dimension is not needed by any falsifier.

### 3.6 Identity re-root — teardown-before-activate, in order

`set_active_pubkey(new)`:
1. Snapshot the current atom set of every graph in `reactive_dependents` (the "old graph").
2. Update `identity`; invalidate every `Reactive(ActivePubkey)` `BindingNode`; recompute affected graphs bottom-up (inner authors/#p swap `old_pk→new_pk`; `Derived` sets recompute against the store — which for a just-switched account is typically empty until that account's kind:3/39002 arrives).
3. Emit ONE ordered `DemandDelta`: **all Close ops (reverse-of-open order) precede all Open ops.** Because the new account's derived sets are usually empty at switch time, the entire old graph's atoms close and only the new inner `Reactive` atoms (e.g. `kinds:3, authors:{new_pk}`) open — the rest open later when the new account's events land. No atom keyed to the old pubkey survives (ledger #10: cross-account leak has no surviving subscription to deliver into).

### 3.7 Replace-not-rebuild / recompile-not-reopen, restated per node

- **Replace-not-rebuild:** each node caches its `ResolvedSet`/atom set; a change recomputes and **diffs**, touching only symmetric-difference elements. `metrics.atoms_opened + atoms_closed == |symmetric diff|`, never `2·|set|`.
- **Recompile-not-reopen:** the outer `LiveQuery` handle and its graph-level refcount are **never** touched by an inner change — only the underlying atoms churn. The handle stays open across re-routes; there is no teardown/reopen of the subscription the app holds.

---

## 4. Refcounting & handle lifecycle

- **Graph-level:** `subscribe(q)` hashes the whole `LiveQuery` descriptor. If present in `registry`, bump refcount, return a new `QueryHandle`, and an **empty** `DemandDelta` (demand already open). If new, build the graph, evaluate, return the **open** delta.
- **Atom-level:** the `atoms` table refcounts each `ConcreteFilter` across *all* graphs; open/close fire only on 0→1 / 1→0. Two different descriptors sharing an atom keep it open until both drop.
- **Withdrawal:** `unsubscribe(id)` decrements the graph refcount; at 0 the graph tears down and its atoms decrement → a **close** delta in reverse-of-open order.
- **Drop:** `QueryHandle` holds a `Weak` back-ref; its `Drop` calls `unsubscribe`. In M1 the contract tests use **explicit `unsubscribe`** (deterministic, headless); `Drop` is a thin wrapper over the same path.
- **NOTE (defer, not M1):** teardown-with-grace / debounce (Room `WhileSubscribed(5s)`) — the drop→withdraw edge exists in M1, but the grace timer is an M4 platform-SDK concern.

---

## 5. The contract tests (M1 pass criteria)

Integration tests in `nmp-resolver/tests/`, driving the `testkit` harness. Each is the REAL path (build event → `deliver` → insert/supersede → re-eval → assert delta + metrics).

**1. `depth1_myfollows_surgical_delta`**
- *Arrange:* `set_active(A_pk)`; subscribe outer `kinds:[1], authors := Derived(inner=kinds:[3], authors:[Reactive(ActivePubkey)], project=Tag(p))`; `deliver(kind3(A,[A,B,C],t=100))`. Assert initial demand = inner `{kinds:3,authors:A_pk}` + outer `{1,A},{1,B},{1,C}`.
- *Act:* `deliver(kind3(A,[A,B,D],t=101))`.
- *Assert:* `delta.ops == [Close({1,C}), Open({1,D})]`; `metrics.atoms_closed==1 && atoms_opened==1`; atoms for A,B and the inner kind:3 atom untouched; `recompute_passes` +1.

**2. `depth2_nip29_groups_cascade_one_level`**
- *Arrange:* `set_active(A_pk)`; subscribe outer `kinds:[39000,39001,39002], #d := Derived(inner=(kinds:[39002], #p:[Reactive(ActivePubkey)]), project=Tag(d))`; `deliver([kind39002(_,"g1",[A]..), kind39002(_,"g2",[A]..)])`. Assert inner atom `{39002,#p:A_pk}` + outer `{…,#d:g1},{…,#d:g2}` open.
- *Act:* `deliver(kind39002(_,"g3",[A]..))`.
- *Assert:* `delta.ops == [Open({…,#d:g3})]`; inner atom unchanged (zero churn); outer handle & graph refcount unchanged (recompile-not-reopen); `metrics.nodes_recomputed` counts only inner+outer (cascade depth == 1).

**3. `identity_reroot_closes_old_before_new`**
- *Arrange:* `set_active(A_pk)`; subscribe `$myFollows`; `deliver(kind3(A,[A,B],t=100))`. Snapshot demand.
- *Act:* `set_active(B_pk)`.
- *Assert (on the re-root delta):* every `Close` index < every `Open` index; closes in reverse-of-open order; all old atoms (`{3,A_pk}`,`{1,A}`,`{1,B}`) closed; only `{3,B_pk}` opened; `active_demand()` contains no atom mentioning `A_pk` (no leak).
- *Then:* `deliver(kind3(B,[E,F],t=100))` → asserts `{1,E},{1,F}` open.

**4. `stale_older_kind3_rejected_without_firing`**
- *Arrange:* `deliver(kind3(A,[A,B,C],t=100))`.
- *Act:* `deliver(kind3(A,[X,Y],t=50))`.
- *Assert:* insert outcome `Stale`; `delta.is_empty()`; `recompute_passes` unchanged.

**5. `duplicate_delivery_no_fire`** — deliver the same event id twice; second → `Duplicate`; empty delta; no recompute.

**6. `unchanged_set_ingest_empty_delta`** — after `{A,B,C}`, `deliver(kind3(A,[A,B,C],t=101))` (same members, newer). Assert `Superseded` but `delta.is_empty()`; `atoms_opened==0 && atoms_closed==0` (proves *set*-diff, not *event*-diff, gates downstream).

**7. `concurrent_depth2_changes_batch_one_delta`** — one `deliver([add g3, remove-by-supersede g1])`. Assert single delta `[Close({…,#d:g1}), Open({…,#d:g3})]`; `recompute_passes` +1 exactly (one compile-invalidation for the batch).

**8. `identical_descriptors_share_graph`** — subscribe `$myFollows` twice. Second subscribe → empty delta, atom refcounts == 2. `unsubscribe` once → empty delta (refcount 1). `unsubscribe` twice → close delta (reverse-of-open).

**9. `follows_minus_mutes_surgical`** (amendment #1 — SetOp/Diff)
- *Arrange:* `set_active(A_pk)`; subscribe outer `kinds:[1], authors := SetOp(Diff, [ Derived(kinds:[3], authors:[Reactive(ActivePubkey)], Tag(p)), Derived(kinds:[10000], authors:[Reactive(ActivePubkey)], Tag(p)) ])`; `deliver(kind3(A,[A,B,C],t=100))`, no mutes yet. Assert outer atoms `{1,A},{1,B},{1,C}`.
- *Act:* `deliver(kind10000_mutes(A,[A],t=100))` (mute A, which is in follows).
- *Assert:* `delta.ops == [Close({1,A})]` and nothing else; `atoms_opened==0 && atoms_closed==1`; B,C untouched; `recompute_passes` +1. (Without `Diff` in the grammar this can only be expressed by the app hand-maintaining the expansion — ledger #11 violation. This test *is* the proof the grammar doesn't contradict its own ledger.)

**10. `resolver_has_no_kind_specific_branches`** (kill guard, structural) — a test that greps `nmp-resolver/src/**` (excluding `testkit`/tests) for kind literals / `event.kind()` comparisons (`== 3`, `== 39002`, `kind() ==`, `match … kind`). Any hit fails. Makes "the resolver grew a kind:3 case" a red build, not a hidden branch.

**11. `address_coord_fans_out_per_coordinate`** (amendment #3) — outer `kinds:[31923], authors := Derived(inner=(kinds:[3], authors:[Reactive(ActivePubkey)]), project=Authors)` is the trivial case; the coordinate case: outer with a field bound to `Derived(inner=(kinds:[30003] authors:[Reactive]), project=AddressCoord)`. Deliver two addressable events → assert two **co-pinned** atoms (each `kinds:{k},authors:{pk},#d:{d}` together, NOT a cartesian of separate field-sets). Add a third coordinate → assert exactly one `Open` of one co-pinned atom (number-of-filters shape change handled surgically).

**12. `arbitrary_depth1_shape_needs_no_engine_change`** (kill guard, generality) — a THIRD unrelated depth-1 shape (e.g. `kinds:[1], #e := Derived(kinds:[10003] bookmarks, project=Tag(e))`). It must compile and pass with **zero** modification to `nmp-resolver` beyond what tests 1–2 needed. If it forces engine edits, generality has failed (the M1 kill).

---

## 6. Kill-condition instrumentation (honest falsification)

The M1 kill is "the resolver grew a kind:3 case and a 39002 case" OR "replace-not-rebuild needs O(rebuild)." Three mechanisms make either kill *visible*:

1. **No-kind-branch structural guard (test 10)** + the design rule that event→node routing is *only* `match_event` against a node's own concrete filter (§3.3 step 2). A builder cannot special-case a kind without either a kind literal in resolver src (→ test 10 red) or a descriptor-content branch inside `recompute_node` (→ caught by test 12 + code review). Both depth-1 and depth-2 provably traverse the *same* `recompute` code — only the descriptor differs.
2. **Generality witness (test 12):** a third, unrelated shape passing with zero engine change is positive evidence the primitive is general, not two hardcoded reads.
3. **Rebuild witness (metrics):** `atoms_opened + atoms_closed` MUST equal `|symmetric difference|` on every surgical test (2 for `{A,B,C}→{A,B,D}`, 1 for follows-minus-mutes, 1 for a group add). If replace-not-rebuild silently degraded to rebuild, these counters would equal `2·|set|` and the exact-count asserts go red. `nodes_recomputed` bounds cascade depth (== 1 for one-level cascades). `recompute_passes` proves at-most-one-compile-invalidation-per-batch (tests 1,7).

`graph_snapshot()` exposes nodes/edges/cached-set-sizes so a reviewer (and these tests) can inspect the graph shape directly rather than trusting prose.

---

## 7. Build order for Sonnet builders

Each step is independently committable; the test(s) it turns green are named. `‖` marks steps that can run in parallel without file conflict.

- **Step 0 — scaffold.** Workspace `Cargo.toml`; three crate skeletons; add `nostr` dep; wire `testkit` module stub. *Green:* `cargo build` + empty test harness compiles.
- **Step 1 ‖ — `nmp-grammar`.** All value types (§2.1), `to_nostr()`, canonical `hash()`, `DemandOp/Delta`. *Green:* grammar unit tests (hash stability/canonicality; lowering; `TagName` validation; `SetOp`/`AddressCoord` variants present).
- **Step 2 ‖ — `nmp-store`.** `EventStore` + `MemoryStore` insert door (dedup→supersede→stale) + `query`. *Green:* store unit tests (newest-wins + lexical tiebreak; stale rejected; duplicate; replaceable vs addressable keying). *Independent of Step 1 → parallel.*
- **Step 3 — resolver: static graph + subscribe.** Graph expansion (FilterNode/BindingNode incl. `Derived`, `SetOp`, `Reactive`, `Literal`), atom computation for a static graph, atom + graph refcount tables, initial demand delta. *Green:* initial-open assertions of tests 1,2,9; `identical_descriptors_share_graph` (8).
- **Step 4 — resolver: incremental re-eval + metrics + batching.** Dirty-mark via `match_event`, bottom-up recompute, set-diff at `Derived`/`SetOp`, atom-diff, ordered `DemandDelta`, `Metrics`, one-pass-per-batch. *Green:* 1, 2, 6, 7, 9.
- **Step 5 — resolver: identity re-root.** `set_active_pubkey`, ordered teardown-before-activate. *Green:* 3.
- **Step 6 — resolver: outcome gating + handle lifecycle.** Stale/duplicate no-fire, `unsubscribe`/`Drop` close deltas. *Green:* 4, 5, 8.
- **Step 7 — resolver: AddressCoord fan-out + generality guards.** Co-pinned coordinate atoms; structural no-kind-branch guard. *Green:* 10, 11, 12.

**Parallelism:** Steps 1 and 2 are fully parallel (disjoint crates). While a builder lands resolver Steps 3–7, a second builder can write the `testkit` harness + event builders + the test *skeletons* against the `Engine`/`Harness` signatures in §2.3 (they compile against the API before internals exist). Steps 3→7 are serial within the resolver (one owner).

---

## 8. Explicit non-goals for M1 (defer list — do not gold-plate)

- **No persistence** — `MemoryStore` only.
- **No real transport / relays / sockets / async** — the harness *scripts* delivery; there is no network.
- **No per-relay wire plans, no coalescing / merge-lattice, no outbox / lane routing, no 2-relay-min or fan-out cap** — that is M2. M1 emits abstract `ConcreteFilter` demand only. (Per-element demand atoms are the exact truth M2 coalesces; do not pre-build coalescing.)
- **No coverage watermarks, negentropy, or NIP-77** — M3.
- **No NIP-45 relay-COUNT query mode** — deliberately excluded (count locally over a coverage window); not a query result mode to build (amendment #5).
- **No FFI / SDK / Swift / Kotlin** — M4.
- **No crypto** — harness builds trusted events with throwaway `Keys` (a test fixture, not a feature); the store does **not** verify signatures. **No encrypt/decrypt** — but do not design demand-side types that would forbid inserting an engine-internal decrypt step before projection later (amendment: keep event content opaque, never assume plaintext-only in a blocking way).
- **No write outbox / intents / receipts, and no write-intent durability class** (`durable | ephemeral | at-most-once`) — M3+ (amendment #4). Don't let the demand-side types obstruct it.
- **No provenance merge** — `Duplicate` is a no-op stub; no provenance field.
- **No GC.**
- **No teardown-with-grace / debounce timer** — the drop→withdraw edge exists; the grace window is M4.
- **No kinds-as-binding** — `kinds` is literal in M1.
- **No multiple independent `Derived` dimensions on a single FilterNode** (true cartesian) — `AddressCoord` co-pinning is handled; independent multi-dimension is unneeded and deferred.
- **Depth > 3 / cycles** — grammar is nestable but M1 only builds/proves bounded depth ≤ 3; no cycle detection.
```
