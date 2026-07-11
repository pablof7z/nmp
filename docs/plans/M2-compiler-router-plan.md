# M2 — Compiler / router + coalescing derisk: implementation plan

- **Date:** 2026-07-11
- **Status:** Provisional-until-v2 (no self-compat obligation). Builder-facing plan for M2 per `docs/VISION.md` §6.
- **Milestone:** M2 — per-relay compilation over M1's abstract demand set. Still headless: NO real sockets/transport (M3), NO persistence, NO FFI. Relay/mailbox facts are INJECTED as fixtures.
- **Gate:** running (property + differential suites). No Tier A unless the widen-only invariant itself must change.
- **Builds on:** M1 (`nmp-grammar` / `nmp-store` / `nmp-resolver`), VERIFIED-WITH-NITS in `docs/reviews/2026-07-11-M1-verification.md`. Both nits are folded in here (§8).

M2 turns M1's per-element demand atoms (`BTreeSet<ConcreteFilter>`, each author-singleton) into **per-relay wire plans** and proves the milestone's real question (VISION §6 Q1): coalescing is demoted from load-bearing to a bandwidth optimization by the **widen-only invariant + mandatory local re-filter**, so a wrong merge rule costs bandwidth, never correctness. This is where per-element atoms **re-coalesce** into multi-author REQs — the "don't shard authors on the wire" doctrine lives HERE, not in M1.

The pre-committed **kill** (VISION §6 M2): with coalescing fully disabled (dedup-only floor), realistic falsifier demand exceeds relay REQ/filter limits *and the trivially-provable author-union rule cannot close the gap* → correctness would require the unproven lattice → per-relay compilation needs redesign. §5 test 16 makes this measurable and visible.

---

## 1. Crate layout delta

M1 is a three-crate workspace (`nmp-grammar`, `nmp-store`, `nmp-resolver`). M2 adds **exactly one** crate.

```
nostr (external crate)
  ├── nmp-grammar     (M1) value types: Filter, Binding, Selector, ConcreteFilter, DemandDelta, hashing
  ├── nmp-store       (M1) EventStore + MemoryStore
  ├── nmp-resolver    (M1) graph engine → abstract demand deltas   deps: nmp-grammar, nmp-store
  └── nmp-router      (M2) per-relay compiler + router + coalescing + diagnostics
                            deps: nmp-grammar, nostr
                            dev-deps: nmp-resolver (uses pub testkit), proptest
```

**Dependency direction.** `nmp-router` depends ONLY on `nmp-grammar` (for `ConcreteFilter`/`DescriptorHash`/`DemandDelta`) and `nostr` (for `RelayUrl` + `Filter::match_event`). It does **NOT** depend on `nmp-resolver` or `nmp-store` in its library — the compiler is a pure function of `(demand set, injected relay facts)`, testable in complete isolation with hand-built `ConcreteFilter` demand sets. `nmp-resolver` is a **dev-dependency only**, used by the integration tests (differential oracle, Drop-nit, kill measurement) that wire the real resolver → router.

**Why one crate, not two (compiler vs router):** they share the same per-relay plan value and the same lane vocabulary; splitting buys no builder parallelism (one owner writes the pipeline serially) and would invert nothing. Few crates, YAGNI. The compiler and router are modules inside `nmp-router`, not crates.

**Why the router consumes `ConcreteFilter`, not resolver internals:** M1 already lowered its output to the `nmp-grammar` value types (`active_demand() -> BTreeSet<ConcreteFilter>`, `DemandDelta{ops: Vec<DemandOp>}`). Everything the compiler needs is in `nmp-grammar`. Keeping `nmp-router` off `nmp-resolver` is what makes the differential oracle honest: the router can be driven by *generated* demand as easily as by the real graph.

Module layout inside `nmp-router/src/`:
```
lib.rs        public surface re-exports
facts.rs      Lane, LanedRelay, RelayDirectory trait, FixtureDirectory, RelayLimits
route.rs      atom classification + outbox resolution + pinned-route lookup
solver.rs     the 2-relay-min + cap coverage solver (greedy set-cover) + shortfall
coalesce.rs   exact-canonical dedup + MergeRule trait + widen-only rule registry
plan.rs       RelayPlan, WireReq, sub-id assignment, plan diffing → WireDelta
deliver.rs    local re-filter + the delivery model (headless "what a relay returns")
diag.rs       Diagnostics (four-lane records, reverse coverage, exact filters)
router.rs     Router: compile(demand, dir) -> WireDelta; owns previous_plan + diagnostics
```

---

## 2. Core types (sketches — fields + key signatures, not bodies)

### 2.1 Lane facts + injected mailbox surface (`facts.rs`)

```rust
pub type PubkeyHex = String;                 // matches ConcreteFilter.authors element type
pub use nostr::RelayUrl;

/// The lane every relay-bearing fact and every route carries (VISION P4,
/// ledger #3; §9 flags the non-NIP-65 lanes as an explicit M2 concern).
/// CLOSED vocabulary — extend the enum, never admit a free-form string.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Lane {
    Nip65Write,       // author's kind:10002 WRITE relay (the outbox default)
    Hint,             // relay hint from a tag / nevent / nprofile
    Provenance,       // where we've previously seen this author's events
    UserConfigured,   // operator policy (role-tagged config, not a route override)
    IndexerDiscovery, // operator indexer set — DISCOVERY KINDS ONLY, never content fallback
    GroupHost,        // NIP-29 host relay for a non-author group atom (pinned)
    DmInbox,          // kind:10050 DM inbox (pinned; full private-route provenance is M3)
}

/// A relay tagged with the lane that supplied it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LanedRelay { pub url: RelayUrl, pub lane: Lane }

/// The injected mailbox/relay-fact surface. In M2 every method is a fixture
/// lookup (no network). M3 backs this with live NIP-65 / probing behind the
/// SAME trait — that is why it is a trait now, with one fixture impl.
pub trait RelayDirectory {
    /// An author's write relays (NIP-65 kind:10002 write entries), lane-tagged.
    fn write_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay>;
    /// Hint / provenance / user-configured extras for an author (may be empty).
    fn extra_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay>;
    /// Operator indexer set. Eligible ONLY for discovery-kind atoms.
    fn indexers(&self) -> Vec<RelayUrl>;
    /// Pinned relays for a NON-author atom (NIP-29 group host, DM inbox, …),
    /// keyed by the atom's discriminating fields. Empty => unroutable.
    fn pinned_relays(&self, atom: &ConcreteFilter) -> Vec<LanedRelay>;
}

/// Configurable relay limits — the kill measurement (§5 test 16) asserts the
/// compiled plan stays within these. Defaults reflect v1 evidence (relays
/// accept large author arrays but cap concurrent subscriptions).
#[derive(Clone, Copy, Debug)]
pub struct RelayLimits {
    pub max_subs_per_relay: usize,      // e.g. 20
    pub max_filter_authors: usize,      // e.g. 1000
    pub max_filter_terms: usize,        // authors + ids + tag values in one filter
}

/// In-memory fixture with ergonomic builders (`with_write`, `with_indexer`,
/// `with_group_host`, adversarial-mailbox generators for the solver tests).
pub struct FixtureDirectory { /* ... */ }

/// The discovery-kind set (default {0, 3, 10002, 10050}). An atom whose
/// `kinds` ⊆ this set MAY use the IndexerDiscovery lane; a content atom never
/// may (ledger: "indexers are never a content fallback"). Injected as config.
pub struct DiscoveryKinds(pub BTreeSet<u16>);
```

### 2.2 Routes + typed provenance (`route.rs`)

```rust
/// Why one relay is in the plan for one atom — typed provenance (ledger #3/#4:
/// "every explicit route carries typed provenance"; "no connection outside a
/// solver-produced plan"). Every wire REQ traces back to one of these.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RouteProvenance {
    pub relay: RelayUrl,
    pub lane: Lane,
    /// Which authors of the atom this relay covers (outbox), or empty for a
    /// pinned non-author route.
    pub covers_authors: BTreeSet<PubkeyHex>,
    /// Solver-produced (outbox) vs pinned-fact (group host / dm inbox).
    pub kind: RouteKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RouteKind { OutboxSolved, Pinned }

/// The classification of a demand atom before routing.
enum AtomClass<'a> {
    /// Author-bearing: coverage-solve the author set.
    Outbox { skeleton: Skeleton, authors: BTreeSet<PubkeyHex>, atom: &'a ConcreteFilter },
    /// No authors: relays come directly from a lane fact (pinned).
    Pinned { atom: &'a ConcreteFilter },
}

/// A demand atom with its routable dimension (authors) projected OUT. Atoms
/// that share a skeleton are coverage-solved TOGETHER so their covering relay
/// set is shared (and their per-relay atoms re-coalesce, §4). The skeleton is
/// the coalescing/sub-id key.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Skeleton { /* ConcreteFilter with authors erased; canonical, Hash */ }
impl Skeleton { pub fn of(atom: &ConcreteFilter) -> (Skeleton, BTreeSet<PubkeyHex>); }
```

### 2.3 Coverage solver (`solver.rs`)

```rust
/// Input: authors, each with an ordered candidate relay list (lane-priority
/// order); the required coverage floor k (2); the global fan-out cap; whether
/// indexers are eligible (discovery-kind atom).
pub struct CoverageInput<'a> {
    pub candidates: BTreeMap<PubkeyHex, Vec<LanedRelay>>, // per-author, lane-ordered
    pub k: usize,                                          // 2 (the min)
    pub cap: usize,                                        // REQUIRED fan-out cap
    pub indexer_eligible_relays: Vec<RelayUrl>,            // [] for content atoms
}

/// Output: the covering relay set + per-author assignment + shortfall report.
pub struct Coverage {
    pub selected: BTreeSet<RelayUrl>,                      // never an accumulated union; |selected| ≤ cap
    pub assignment: BTreeMap<PubkeyHex, BTreeSet<RelayUrl>>, // each author -> the relays covering it
    pub shortfall: BTreeMap<PubkeyHex, Shortfall>,         // authors that did not reach k
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shortfall { pub requested_k: usize, pub achieved: usize, pub reason: ShortfallReason }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShortfallReason {
    NoCandidates,          // author has no NIP-65 / hint / provenance relays at all
    FewerCandidatesThanK,  // author lists < k relays — achieved == their ceiling, NOT a defect
    CapExhausted,          // the global cap was hit before this author reached k
}

pub fn solve(input: &CoverageInput) -> Coverage;   // greedy, deterministic (§3)
```

### 2.4 Coalescing (`coalesce.rs`)

```rust
/// A widen-only, INTROSPECTABLE merge rule. The correctness contract is a
/// single independently-checkable fact (VISION §6 Q1(a)):
///   matches(try_merge(a,b)) ⊇ matches(a) ∪ matches(b)   for all events.
/// A rule not shown to widen is DROPPED (graceful degradation): its filters
/// ship as separate REQs. Exact-canonical dedup alone is the trivially-correct
/// floor and is not expressed as a rule.
pub trait MergeRule {
    fn name(&self) -> &'static str;
    /// Some(merged) claims the widening contract for (a,b). None = "I don't
    /// apply here". The property test (§5 test 10) is what VERIFIES the claim.
    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter>;
}

/// The default registry. `AuthorUnion` is the load-bearing one — it re-merges
/// M1's per-element author shards ({X,A},{X,B},{X,D}) into one REQ
/// {X,authors:{A,B,D}} and is trivially widening (adding authors only matches
/// more). Additional rules (e.g. KindUnion for identical-except-kinds filters)
/// are optional and droppable.
pub struct RuleRegistry { rules: Vec<Box<dyn MergeRule>>, dropped: Vec<&'static str> }
impl RuleRegistry {
    pub fn default_widen_only() -> Self;   // [AuthorUnion, KindUnion?]
    pub fn coalesce(&self, filters: BTreeSet<ConcreteFilter>) -> Vec<ConcreteFilter>;
    pub fn dropped_rules(&self) -> &[&'static str];
}
```

### 2.5 Plan + wire delta + diffing (`plan.rs`)

```rust
/// A stable subscription id, keyed by (relay, skeleton) so that adding/removing
/// an author re-uses the same sub-id: on the wire that is ONE overwriting REQ,
/// not close+reopen of every author (NIP-01: a REQ with an existing sub-id
/// replaces that sub's filter).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SubId(pub RelayUrl, pub SkeletonHash);

pub struct WireReq {
    pub sub_id: SubId,
    pub filter: ConcreteFilter,          // the (possibly coalesced, widened) wire filter
    pub provenance: Vec<RouteProvenance>,// why this REQ exists (lanes, covered authors)
}

/// The full per-relay plan for the CURRENT demand set.
pub struct RelayPlan { pub reqs: BTreeMap<RelayUrl, Vec<WireReq>> }

/// A single wire operation. `Req` is open-or-replace (same sub-id overwrites);
/// `Close` withdraws a sub-id.
pub enum WireOp { Req(SubId, ConcreteFilter), Close(SubId) }

/// Surgical per-relay deltas — the M1 atom-diffing discipline lifted to the
/// wire layer. INVARIANT (mirrors DemandDelta): all Close ops precede all Req
/// ops (teardown-before-activate; hygiene now, load-bearing when private
/// routes arrive in M3).
pub struct WireDelta { pub ops: Vec<(RelayUrl, Vec<WireOp>)> }

/// Diff new plan against previous → WireDelta. Unchanged (relay, skeleton)
/// subs whose filter is byte-identical emit NOTHING; a changed filter on an
/// existing sub emits one Req(sub_id, new); a vanished sub emits Close(sub_id);
/// a new sub emits Req(sub_id, filter).
pub fn diff_plans(prev: &RelayPlan, next: &RelayPlan) -> WireDelta;
```

### 2.6 Diagnostics (`diag.rs`)

```rust
/// The acceptance-test-made-visible surface (VISION §5 diagnostic screen,
/// harvested four-lane design). Read-only projection of the plan + provenance.
pub struct RelayDiagnostics {
    pub relay: RelayUrl,
    pub wire_sub_count: usize,                 // subs on this relay
    pub by_lane: BTreeMap<Lane, usize>,        // routes on this relay per lane
    pub authors_served: usize,                 // reverse coverage: distinct authors this relay covers
    pub filters: Vec<ConcreteFilter>,          // the EXACT filters sent to this relay
}
pub struct Diagnostics {
    pub per_relay: BTreeMap<RelayUrl, RelayDiagnostics>,
    pub uncovered_authors: BTreeMap<PubkeyHex, Shortfall>,
    pub dropped_merge_rules: Vec<&'static str>, // graceful-degradation visibility
}
```

### 2.7 The Router (`router.rs`)

```rust
pub struct Router {
    limits: RelayLimits,
    discovery: DiscoveryKinds,
    rules: RuleRegistry,
    prev_plan: RelayPlan,        // previous, for diffing
    last_diag: Diagnostics,
}
impl Router {
    pub fn new(limits: RelayLimits, discovery: DiscoveryKinds, rules: RuleRegistry) -> Self;

    /// THE entry point. Recompile the whole per-relay plan from the engine's
    /// CURRENT demand set (VISION §6: "diffing against the previous plan into
    /// surgical CLOSE/REQ deltas"), diff vs `prev_plan`, store the new plan +
    /// diagnostics, return the surgical wire delta.
    pub fn compile(&mut self, demand: &BTreeSet<ConcreteFilter>, dir: &dyn RelayDirectory, cap: usize)
        -> WireDelta;

    pub fn diagnostics(&self) -> &Diagnostics;
    pub fn plan(&self) -> &RelayPlan;
}
```

**Full-recompile-then-diff, not delta-threading.** The router recomputes the plan from the full current demand set each call and derives the wire delta by diffing plans. This matches the VISION wording exactly, is far less bug-prone than threading `DemandDelta` ops through routing, and is O(demand) — acceptable at M2 scale for the same reason M1's O(graph)-per-change is (bounded, small). It also automatically fixes the Drop nit (§8.2): a withdrawn atom simply vanishes from `demand`, so the next `compile` emits its `Close`. `DemandDelta` is therefore NOT a router input; it is only how the engine keeps `active_demand()` current.

---

## 3. The coverage solver algorithm (2-relay-min + cap)

The heart of M2 and ledger #4. It is a **capped, greedy, k-cover** — set-cover is NP-hard, so greedy approximation with a deterministic tiebreak is the correct minimum-shape choice.

**Per-skeleton, over the author set.** Atoms are grouped by `Skeleton` (§2.2); the solver runs once per skeleton over that skeleton's author set, so all authors of one query share one covering relay set (and re-coalesce per relay, §4). Content vs discovery eligibility is a property of the skeleton's `kinds`.

**Setup.** For each author, build the candidate relay list = `write_relays` (Nip65Write) ++ `extra_relays` (Hint/Provenance/UserConfigured), lane-ordered (Nip65Write first). If the skeleton is discovery-kind, indexer relays are appended as candidates for *every* author; if content-kind, they are NOT candidates (structural: indexers never serve content). Each author needs `min(k, |candidates|)` coverage slots filled (clamps k for authors with fewer than k relays — that is their ceiling, not a shortfall of type CapExhausted).

**Greedy loop.**
1. Maintain `selected: BTreeSet<RelayUrl>` and per-author `need = min(k,|cand|) - |covered|`.
2. While some author has `need > 0` AND `|selected| < cap`:
   - Score each not-yet-selected relay = number of (author, open-slot) pairs it would fill (an author counts only while it still needs coverage).
   - Pick the highest score; ties broken by **lexicographic RelayUrl** (determinism → reproducible plans → stable diffs).
   - Add to `selected`; decrement `need` for every author it covers; record assignment.
3. On exit, any author with `need > 0` gets a `Shortfall`: `CapExhausted` if the cap was hit, `NoCandidates`/`FewerCandidatesThanK` if that is the ceiling.

**`|selected| ≤ cap` always** — the cap is a hard global bound; the plan is NEVER an accumulated union of per-author relay lists (ledger #4). When cap is too small for full k-coverage, the greedy loop maximizes covered slots under the cap and REPORTS the shortfall (never silently drops the floor).

**Adversarial cases (each a §5 solver test):**
- **One prolific author (50 write relays)** → filled with exactly `k=2` slots; the other 48 are never selected on its account. One author cannot blow the cap. (test 4)
- **Disjoint mailboxes** (N authors, 2 unique relays each, no overlap, cap < 2N) → needs 2N relays; greedy fills to the cap, reports `CapExhausted` for the remainder; `|selected| == cap`. (test 3)
- **Heavy overlap** (all authors share 3 relays) → greedy picks 2 popular relays, covers everyone, `|selected| == 2`. Minimal. (test 2)
- **Author with 1 write relay** → k clamps to 1; covered at 1; `FewerCandidatesThanK`, not a `CapExhausted` defect. (test 5)
- **Author with 0 relays (no NIP-65), content kind** → `NoCandidates`; NOT indexer-filled. (test 6)
- **Ties** → lexicographic; the same input always yields the same plan (so `diff_plans` is stable and test 14's "A,B untouched" holds).

---

## 4. Coalescing + widen-only property test + differential oracle

### 4.1 The pipeline (where re-coalescing happens)

1. **Group** demand by `Skeleton`; collect each skeleton's author set (outbox) or classify as pinned.
2. **Route**: outbox groups → `solve` (§3) → per-author relay assignment; pinned atoms → `dir.pinned_relays`.
3. **Materialize per relay**: for relay R and skeleton S, collect the authors assigned to R → **one filter `S + authors:{those authors}`**. This is the **AuthorUnion coalescing done by construction** — M1's shards `{X,A},{X,B},{X,D}` become one REQ `{X,authors:{A,B,D}}` on each relay serving them. Always widening (superset of each shard). This is the "don't shard authors on the wire" doctrine, realized.
4. **Dedup + coalesce**: within each relay, exact-canonical dedup (group by `ConcreteFilter::hash()` — reuse M1's `DescriptorHash` — identical filter → one REQ, the trivially-correct floor), then apply the `RuleRegistry` merge rules across the remaining distinct filters (e.g. `KindUnion` for filters identical except `kinds`). Every rule is widen-only; dropped rules degrade to separate REQs.
5. **Assign sub-ids** (skeleton-keyed) → `RelayPlan`.
6. **Diff** vs previous plan → `WireDelta`.

### 4.2 The widen-only property test (VISION §6 Q1(a))

`proptest`, per active `MergeRule`:
```
proptest! { fn merge_rule_widens(a: ConcreteFilter, b: ConcreteFilter, evs: Vec<Event>) {
    if let Some(m) = rule.try_merge(&a,&b) {
        for e in &evs {
            if a.matches(e) || b.matches(e) { prop_assert!(m.matches(e)); }  // ⊇
        }
    }
}}
```
`matches` = `ConcreteFilter::to_nostr().match_event(e, MatchEventOptions::new())` (reuse rust-nostr; never hand-roll). Generators: `ConcreteFilter` over a small alphabet of kinds/pubkeys/tag-values so collisions are frequent; `Event` likewise, plus targeted events built to match `a`/`b`. A rule that fails this property is a RED test AND is dropped from `default_widen_only()` (graceful degradation is verified by test 13).

### 4.3 Local re-filter — exactness (VISION §6 Q1(b))

`deliver.rs`: given events a relay returns for a coalesced wire filter, each event is re-matched against **each consuming atom's ORIGINAL `ConcreteFilter`** before delivery to that consumer. Widen-only guarantees no UNDER-delivery (the wire filter matched ≥ every atom's events); local re-filter guarantees no OVER-delivery (each consumer sees exactly its atom's matches). State both directions explicitly — they are the two halves that make coalescing non-load-bearing.

### 4.4 The differential oracle (integration test, `nmp-router/tests/`)

Wires the REAL resolver (dev-dep `nmp-resolver::testkit`) → router.
```
Arrange: generated demand (via the resolver graph OR hand-built atoms) +
         a per-relay model store (relay -> events it holds) + fixture facts.
Path A (dedup-only floor): route atoms; send each atom as its own REQ; per
         consumer atom collect events = { e in relay's store : atom.matches(e) }.
Path B (coalesced): router.compile → wire filters per relay; per relay collect
         events matching the WIRE filter; then LOCAL RE-FILTER to each consumer.
Assert: for every consumer atom, delivered row set (A) == delivered row set (B).
```
Run under generated/adversarial traffic (`proptest`). IDENTICAL per-consumer delivery is the derisk. (test 12)

---

## 5. Contract / property tests — M2 pass criteria

Integration + unit tests. Names + arrange/act/assert skeletons.

1. **`outbox_maps_authors_to_write_relays`** — inject NIP-65 write relays for A,B; demand `{kinds:[1],authors:{A}}`,`{…{B}}`; assert A's atom routes to A's write relays, B's to B's; each `WireReq.provenance` lane == `Nip65Write`.
2. **`coverage_gives_each_author_min_two_relays`** — authors each with ≥2 write relays, generous cap; assert every author's `assignment` size ≥ 2; no shortfall.
3. **`coverage_respects_cap_under_disjoint_mailboxes`** — N=10 authors, disjoint 2-relay mailboxes, cap=6; assert `|selected| ≤ 6` and the uncovered authors carry `CapExhausted`.
4. **`coverage_single_prolific_author_capped_at_k`** — author with 50 write relays; assert exactly 2 selected on its behalf.
5. **`coverage_author_with_one_relay_clamps_k`** — author with 1 write relay; assert covered at 1, shortfall reason `FewerCandidatesThanK` (not a defect surface).
6. **`content_atom_uncovered_author_never_uses_indexer`** — content-kind atom, author with no NIP-65; assert `NoCandidates` shortfall and NO indexer relay in the plan.
7. **`indexer_lane_only_for_discovery_kinds`** — same author, one kind:3 atom (discovery) and one kind:1 atom (content); assert indexers appear for the kind:3 route, never the kind:1 route.
8. **`exact_canonical_dedup_one_req_per_relay`** — two subscriptions producing the identical atom on the same relay; assert one `WireReq` (deduped by hash).
9. **`author_union_coalesces_shards_into_one_req`** — atoms `{X,A},{X,B},{X,D}` all assigned to relay R; assert R has ONE `WireReq` `{X,authors:{A,B,D}}`; assert it is a superset of each shard.
10. **`merge_rule_widens`** (proptest, §4.2) — for each rule in `default_widen_only()`, `matches(merged) ⊇ matches(a) ∪ matches(b)`.
11. **`local_refilter_is_exact`** (unit + proptest) — a widened wire filter delivering a superset; re-filter against each original atom yields exactly that atom's matches (no over-, no under-delivery).
12. **`differential_oracle_identical_delivery`** (proptest, §4.4) — coalesced vs dedup-only deliver identical per-consumer row sets over generated traffic.
13. **`non_widening_rule_is_dropped_and_ships_separately`** — register a deliberately non-widening rule; assert it is in `dropped_rules()`, its inputs ship as separate REQs, and delivery stays correct (oracle passes).
14. **`per_relay_diff_is_surgical`** — compile plan for `{X,A},{X,B},{X,C}`; recompile for `{X,A},{X,B},{X,D}`; assert the `WireDelta` touches only relays serving C/D (one overwriting `Req` on each, keyed by the stable skeleton sub-id), and relays serving ONLY A/B emit NO ops. (The wire-layer mirror of M1 test 1.)
15. **`dropped_handle_close_reaches_wire`** (M1 nit 2, §8.2) — subscribe (resolver) → `compile` opens a REQ; drop the handle; `engine.poll_pending_drops()`; `compile` again → assert `WireDelta` contains `Close(sub_id)` for the withdrawn atom's sub. (In M1 this delta was discarded; here it reaches the wire.)
16. **`kill_measurement_dedup_only_within_relay_limits`** (THE M2 kill, §6) — build realistic falsifier demand (~300 follows over a realistic write-relay distribution); compile **dedup-only** (rules empty); measure per-relay `wire_sub_count` and max `max_filter_terms`; PRINT the measurement (acceptance-visible); assert within `RelayLimits`. Then compile **with AuthorUnion**; assert strict improvement. The test FAILS (kill) only if even AuthorUnion coalescing leaves a relay over `max_subs_per_relay` or a filter over `max_filter_authors`.
17. **`no_kind_value_read_outside_annotated_site`** (M1 nit 1, §8.1) — hardened structural guard over `nmp-router/src` and `nmp-resolver/src`: assert `as_u16(`/`as_u64(`/`.kind` reads occur ONLY on lines bearing the `// KIND-VALUE-READ:` annotation; belt-and-suspenders to the clippy `disallowed-methods` lint.
18. **`every_wire_req_traces_to_a_route`** (ledger #4 structural) — assert every `WireReq` in the plan carries non-empty `provenance`; there is no relay in the plan not produced by routing (no connection outside a solver-produced/pinned plan).
19. **`diagnostics_reverse_coverage_and_lanes`** — assert `Diagnostics` reports, per relay, `authors_served` (reverse coverage), `by_lane` counts, and the exact filters; and `uncovered_authors` for the shortfall case.
20. **`nip29_non_author_atom_routes_via_group_host`** (discharges VISION §9) — a non-author NIP-29 outer atom (`kinds:[39000,39001,39002],#d:{g}`) routes to its `GroupHost` pinned relay; no coverage solving; provenance lane `GroupHost`.

---

## 6. The M2 kill — how the plan makes it visible

Dedup-only floor = exact-canonical dedup with the merge registry EMPTY. Because M1 emits per-element author atoms, dedup-only produces **one wire REQ per (author, relay)** — a relay serving 200 of your authors gets 200 subs. Test 16 measures exactly this:

- **`wire_sub_count` per relay** vs `max_subs_per_relay` — the sub-count limit is the one v1 evidence says relays enforce.
- **`max_filter_terms` / author-count** per REQ vs `max_filter_authors` — the filter-size limit.

Two tiers make the kill honest:
1. **Dedup-only** may exceed `max_subs_per_relay` (many per-author subs). That alone does NOT fire the kill — because AuthorUnion is a **trivially-provable widening rule** (test 10 proves it) that collapses N per-author subs into 1 multi-author sub.
2. **With AuthorUnion**, re-measure. The kill FIRES only if a relay STILL exceeds `max_subs_per_relay`, or a coalesced filter exceeds `max_filter_authors` (a single REQ with thousands of authors). That would mean the trivially-correct floor + the trivially-provable rule are insufficient and correctness would require MORE aggressive (unproven) merging → per-relay compilation needs redesign.

v1 evidence (relays accept large author arrays, cap concurrent subs) predicts tier 2 passes: AuthorUnion trades many small subs for few large ones, landing inside both limits. The test encodes that prediction as an assertion and prints the numbers so the verdict is visible, not asserted-by-prose. `RelayLimits` is configurable so the measurement can be re-run against real relay caps discovered in M3.

---

## 7. Build order for Sonnet builders

`‖` marks steps that can run in parallel without file conflict. Each step names the test(s) it turns green.

- **Step 0 — scaffold.** Add `nmp-router` to the workspace; deps (`nmp-grammar`, `nostr`); dev-deps (`nmp-resolver`, `proptest`); empty modules. *Green:* `cargo build`.
- **Step 1 ‖ — facts.** `Lane`, `LanedRelay`, `RelayDirectory`, `FixtureDirectory` (+ adversarial-mailbox builders), `RelayLimits`, `DiscoveryKinds`. *Green:* fixture unit tests.
- **Step 2 ‖ — plan value types.** `Skeleton`, `SubId`, `WireReq`, `RelayPlan`, `WireOp`, `WireDelta`, `RouteProvenance`. Pure; parallel with Step 1. *Green:* skeleton/hash unit tests.
- **Step 3 — routing.** Atom classification (outbox vs pinned), candidate assembly (lane order, discovery-kind indexer eligibility), pinned lookup. *Green:* 1, 7, 20.
- **Step 4 — coverage solver.** Greedy k-cover + cap + shortfall (§3). *Green:* 2, 3, 4, 5, 6.
- **Step 5 — materialize + dedup + author-union.** Per-relay atom collection (author-union by construction), exact-canonical dedup. *Green:* 8, 9.
- **Step 6 — merge rules.** `MergeRule` trait, `AuthorUnion`/`KindUnion`, `RuleRegistry`, widen property test + drop-on-fail. *Green:* 10, 13.
- **Step 7 — local re-filter + differential oracle.** `deliver.rs`, model store, oracle harness. *Green:* 11, 12.
- **Step 8 — plan assembly + diffing.** Sub-id assignment, `Router::compile`, `diff_plans`. *Green:* 14, 18.
- **Step 9 ‖ — engine Drop-delta wiring** (nmp-resolver change, §8.2). Stop discarding drop deltas; add `Engine::poll_pending_drops`. Router seam. Parallel once the seam is agreed (touches a different crate). *Green:* 15.
- **Step 10 ‖ — diagnostics.** `diag.rs` assembly from plan + provenance + shortfall. *Green:* 19.
- **Step 11 ‖ — kill measurement.** Realistic-demand generator + `RelayLimits` assertion + print. *Green:* 16.
- **Step 12 ‖ — harden no-kind guard** (§8.1). clippy `disallowed-methods` config + annotate the sole projection site + extend the scanner. *Green:* 17.

**Parallelism:** 1‖2 (disjoint files); 3→8 serial (router core, one owner); 9,10,11,12 parallel after Step 8 (9 is a different crate; 10/11/12 are additive modules/tests).

---

## 8. The two M1 nits (entry conditions)

### 8.1 Harden the no-kind-branch guard

M1's grep (`no_kind_branches.rs`) is defeatable — the reviewer demonstrated `let k = ev.kind.as_u16(); match k {3 => …}` passes it. The **structural** fix, minimum-shape and AST-level (not defeatable by reformatting):

- **clippy `disallowed-methods`** (in `clippy.toml`) banning `nostr::Kind::as_u16` and `nostr::Kind::as_u64` across `nmp-resolver` and `nmp-router`. You cannot obtain a `u16`/`u64` kind to compare without calling a disallowed method → a laundered `match k {3 => …}` is CI-red at clippy, upstream of any `match` shape the grep can miss. Numeric match-arms on kinds become effectively unrepresentable.
- **The single legit kind-value read** — the `AddressCoord` projection (`nmp-resolver/src/eval.rs`, building `Element::Coord{kind,…}`) — carries `#[allow(clippy::disallowed_methods)] // KIND-VALUE-READ: projection into Element::Coord, not a routing branch`. It is the one auditable site; laundering now requires ALSO forging that annotation (a visible, reviewable act).
- **Keep the existing grep test, extended** (test 17): assert `as_u16(`/`as_u64(`/`.kind` reads appear ONLY on `// KIND-VALUE-READ:`-annotated lines. Belt-and-suspenders; the clippy lint is the real gate, the scanner is the cheap fast-feedback tripwire.

CI must run `cargo clippy -p nmp-resolver -p nmp-router -- -D warnings` for the lint to bite.

### 8.2 Wire `QueryHandle::Drop`'s close-delta to the wire

In M1, `Engine::drain_pending_drops` calls `unsubscribe_inner` (which correctly decrements the atoms table) but **discards** the returned `DemandDelta` (`let _ = …`, `engine.rs:292`). Correct for headless M1 (no wire); a real hole at M2 (a dropped CLOSE never reaches the wire). Two-part fix, and the seam where it connects:

- **Engine change (`nmp-resolver`):** `drain_pending_drops` must **merge** each drop's close ops into the `DemandDelta` returned by the current mutating call (`ingest`/`subscribe`/`unsubscribe`/`set_active_pubkey`) instead of discarding, AND add `pub fn poll_pending_drops(&mut self) -> DemandDelta` so a driver can flush drops even with no other activity. This preserves the M1 close-before-open ordering.
- **The seam:** the router recompiles from `active_demand()`. Because `drain_pending_drops` already decremented the atoms, a dropped atom is ALREADY absent from `active_demand()`; the next `Router::compile` therefore diffs it out and emits `WireOp::Close(sub_id)` — the withdrawal reaches the wire. `poll_pending_drops` gives the M2 driver a way to trigger that recompile after a bare drop. Test 15 asserts the CLOSE actually appears.

Note the resolver change is small and additive (merge instead of discard; one new public method); it does not alter M1's contract-test behavior (explicit `unsubscribe` already surfaces its delta).

---

## 9. Explicit non-goals for M2 (defer list — do not gold-plate)

- **No real transport / sockets / async / reconnection.** The router emits `WireDelta` values; nothing sends them. Delivery is MODELED (`deliver.rs`) only for the oracle. (M3)
- **No negentropy / NIP-77 / capability probing.** (M3)
- **No persistence / coverage watermarks.** Coverage state here is solver shortfall, not (filter,relay) completeness watermarks. (M3)
- **No write outbox / intents / receipts / private-route provenance narrowing.** `DmInbox`/`GroupHost` are thin PINNED lanes sufficient to route the falsifier's non-author reads; full private-route classes and fail-closed narrowing (ledger #6) are M3.
- **No live NIP-65 / relay-list fetching.** All relay facts are INJECTED via `RelayDirectory` fixtures.
- **No FFI / SDK / Swift / Kotlin.** (M4)
- **No Collection observation mode / ordering / pagination / cursors.** (M4+, VISION §10)
- **No general filter lattice.** Deliberately (VISION §6 Q1): only the widen-only invariant + per-rule property tests + differential oracle + graceful degradation. Do NOT attempt to prove or build a lattice.
- **No incremental plan-diffing / delta-threading.** Full-recompile-then-diff is the specified approach; add incrementality only if M3 profiling demands it.
- **No multi-dimension coverage solving.** The solver covers the AUTHOR dimension (outbox). Pinned atoms are looked up, not solved.
```
