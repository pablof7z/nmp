# Consult: "composite feed as the sole feed model" doc — discriminating extraction

- **Date:** 2026-07-11
- **Reviewer:** Fable (architect consult; read-only)
- **Input:** external agent doc arguing composite-feed-as-sole-model (lanes = `{expression, map: DeliveredEvent -> [RowContribution]}`, NMP owns acquisition/replay/dedup/retraction/ordering/pagination/cursors/windows/recovery).
- **Against:** `docs/VISION.md` (two-noun surface, M0 passed with SetOp + durability-class + encrypt-capability amendments), `docs/bug-class-ledger.md` (#11 no app interest expansion, #12 no presentation in core), `docs/design-record.md`.
- **Grounding check (code-grounded audit rule):** doc's v1 citations are mixed. `FlatFeed`, feed windows/cursors/pagers, feed-session replay all real (`crates/nmp-feed/src/{window,pager,pull_controller}.rs`, `crates/nmp-feed-session/src/{source_replay,flat_replay}.rs` in the old repo). `CompositeFeedParams` and `KeyedReadCollection` do **not** grep in the old repo at HEAD — treat the doc's symbol-level claims as loosely grounded. Notably, v1's own custom-feed layer (`crates/nmp-feed-session/src/custom.rs`) states "NO closure crosses the boundary" and resolves `Custom(id)` to registered *closed data* — v1 already knew the rule the doc's lane mapper breaks.

---

## Verdict table

| Verdict | Item | Reason (one line) |
|---|---|---|
| **FOLD-IN** | **Windowed-collection RESULT contract** (ordered deltas, window, opaque cursors, has_more/exhausted) | Real gap: our grammar work is all demand-side; the falsifier IS a windowed feed. Folds in as a result shape of the query noun, not a third noun (§Q1). |
| **FOLD-IN** | **Two-cursor distinction** (source/ingest-seq vs presentation/sort-key) | Real, hard-won, already lived in v1 (`after_seq` replay + `FeedCursor`); VISION doesn't state it; prevents the "late old event silently skipped" bug (§Q3; candidate ledger #13). |
| **FOLD-IN** | **"NMP owns a virtualizable collection, not UI virtualization"** | One clean boundary sentence; slots straight into the ownership table's third column. |
| **FOLD-IN** | **Retraction as a first-class collection delta** | Delete/supersede at the store door must surface as `Remove`/`Update` deltas inside an open window; implied by our one-door store but worth stating in the result contract. |
| **FOLD-IN** (as closed vocabulary, not closure) | **Row-key selection** (row keyed by e-tag root, address coord, etc.) | The legitimate residue of the lane mapper: a closed `RowKey := EventId \| Tag(char) \| AddressCoord` selector — same family as `Selector`; this is v1's settled `canonical_row_id` knob (#3082), not app code. |
| **ALREADY-HAVE** | One canonical event pipeline | Our single mutating store door (ledger #1, #5). |
| **ALREADY-HAVE** | One demand model | The Filter-of-Bindings grammar + one resolver/compiler is exactly this; the doc converges. |
| **ALREADY-HAVE** | X-Ray receipts / diagnostics first-class | Our diagnostic plane + write receipts (ledger #9); convergent, nothing new. |
| **ALREADY-HAVE** | "Admission" as distinct from acquisition | That is P5's local re-filter against the consumer's original filter — a *value* (the filter itself), already structural. |
| **ALREADY-HAVE** | Multi-lane *demand* (partially) | SetOp(Union/…) covers set-union within a field; heterogeneous lanes are a *list of ordinary queries* merged result-side (§Q1), not new demand grammar. |
| **REJECT** | **Lane mapper as app closure feeding engine state** (`DeliveredEvent -> [RowContribution]` with app-computed `sort_key`/`row_key`/`payload`) | Violates the no-closure principle where it matters: engine ordering/cursor/dedup correctness would depend on unintrospectable, possibly non-deterministic app code; regresses even v1's own "no closure crosses the boundary." See §Q2 for the exact line and what survives. |
| **REJECT** | **"One expression from which NMP derives demand AND admission AND mapping"** | The dangerous blend: it welds an app closure onto the demand/admission path, gutting ledger #11's mechanism ("no seam through which the app can…"). Demand+admission are engine values; mapping is app code *after* delivery. |
| **REJECT** | **Protocol modules registering lane-mappings into the engine** | Module registration is exactly what P1 abolished; this is the v1 registry seam reborn. Protocol knowledge enters as (a) closed-vocabulary extensions shipped in engine releases, or (b) plain app-side libraries that *construct descriptors* and *fold delivered rows* — no registration surface. |
| **REJECT** | **Unified `Observation<T>/Collection<T>/Operation<T>` live-resource family** | Folding writes into a generic live-resource blurs the read/write split the design record explicitly defends (reads replayable/no terminal state; writes durable/terminal receipt — its critique #3 of the tri-plane agent). Shared *refcount/teardown mechanics* internally, fine; shared app-facing abstraction, no. |
| **REJECT** (as v2 guidance) | **The thesis itself: "composite feed = the SOLE feed model; delete single-source/preset model"** | It is advice for the OLD repo's feed-session family. v2 has no feed model to consolidate; adopting the lane/expression/registration machinery would import a feed *framework* into an engine whose whole bet is two nouns. Take the result contract; leave the framework. |

---

## Q1 — Is "Collection" a third noun, a result shape, or something else?

**Position: a result shape of the one query noun — with one precise concession about demand.** Not a third noun.

The two-noun rule (and §4's tripwire) constrains what may *express read demand or write intent*. It says nothing about the result contract, and VISION already commits the result side to structure: rows + batched row deltas + a coverage variant (ledger #7, §4 FFI). The doc's genuine contribution is that for the windowed-feed case this result contract must be a **bounded ordered replica with a window**, and we have not written that down. So:

- **Base result shape (every query):** a keyed row-delta stream + coverage. Unordered; the app folds freely.
- **Collection result shape (opt-in observation mode on the SAME descriptor):** an engine-maintained ordered windowed view over that replica. Same demand key, same graph node — two observers at different scroll depths share the subscription.

**Type sketch:**

```
// THE noun (unchanged)
Query := Filter<Binding>                    // demand; hashable value

// Observation modes — result side only
engine.observe(q)              -> Live<RowDeltas>          // base shape
engine.observeCollection(scope, w) -> Live<Collection>      // ordered window view
    scope  := NonEmpty<[Query]>             // "lanes" = a LIST of the one noun
    w      := Window { order: OrderKey, initial_limit }
    OrderKey := CreatedAtDesc | (CreatedAt, Id) | …         // CLOSED, like Selector — never a comparator closure
    RowKey   := EventId | Tag(char) | AddressCoord           // CLOSED — the settled canonical_row_id knob

Collection delta := Insert{at} | Remove{at} | Update{key} | Move{from,to}
Collection state := { rows, tail: Coverage }                // Coverage = Unknown | CompleteUpTo(watermark)
cursor (opaque)  := presentation (order-key value, id)      // engine-internal: ingest-seq + watermarks
handle.loadMore(cursor)   // no new demand primitive — see below
```

**How window/cursor mechanics attach without becoming a second demand mechanism:** pagination drives the filter's **own** temporal vocabulary. `loadMore` causes the engine's window controller to widen `until`/`limit` on the *same compiled demand node*, flowing through the one resolver/compiler exactly like a `Derived` re-evaluation, and widen-only per P5. The app never expresses demand a second way — it consumes a handle; the engine translates window extent into fields the `Filter` already has. **Rule:** window mechanics may only widen the descriptor's own `since/until/limit` on its existing graph node; they may never mint a new filter, binding, or subscription primitive.

**The concession:** heterogeneous lanes ("kind:1 from follows ∪ kind:6 from list X") are not one `Filter` — SetOp unions *within a field*, not across whole filters. Resolution: `observeCollection` takes a non-empty **list of the existing noun** and merge-sorts result-side. A lane is therefore an ordinary live query plus a closed `RowKey` — nothing else. Union `has_more` = the meet of per-lane coverage at the window tail (`exhausted` only when every lane proves `CompleteUpTo` past the boundary) — ledger #7's type composes; `has_more`/`exhausted` IS the coverage variant surfacing on the collection, not a new state machine. A "gap" is an Unknown-coverage region between proven regions — representable from watermarks, rendered by diagnostics.

**Gate note:** this amends the noun surface's *result contract*, so per M0's own rule it takes a Tier A propose/refute round before it binds. This consult is the propose half.

## Q2 — The lane mapper closure: the line

**The rule, crisply:**

> **Values in, code after.** Anything the engine uses to *decide* — what to fetch (demand), what is admitted into engine state (admission = P5 re-filter), how rows are keyed, ordered, deduped, paginated, or cursored — must be a closed, introspectable value (Filter, Binding, Selector, RowKey, OrderKey, Window). Anything that *consumes* delivered rows and produces app state may be arbitrary app code — but it runs in app space, after delivery, and **nothing it produces flows back into the engine's demand, admission, ordering, or cursor mechanics.** Closures may consume the engine's output; they may never parameterize the engine's behavior.

Applied to the doc: delivery-side folding is legitimately the app's (our ownership table already grants "folding query streams into its own view state; all derivation beyond the closed Selector vocabulary"). The doc's mapper is **not** that: its `RowContribution` carries `sort_key` and `row_key` that the *engine* then uses for ordering, dedup, cursors, and window membership. That routes engine correctness through unintrospectable app code — the engine can no longer prove cursor stability, replay deterministically, dedup demand across observers, or explain a row in diagnostics; and an app-computed `payload` inside engine machinery reopens #12 pressure. The doc's "one expression → demand AND admission AND mapping" is the blend to refuse: it re-creates the seam ledger #11 exists to remove, one layer up. v1 itself already enforced the correct side of this line (`custom.rs`: "NO closure crosses the boundary" — `Custom(id)` → registered closed data through the same resolver). What survives of the mapper: `RowKey` and `OrderKey` as closed vocabularies (fold-in above); everything else it did is an app-side fold over the delivered stream.

## Q3 — Two cursors: fold in (one paragraph's worth)

Not fully implied today, and cheap to state. VISION has coverage watermarks (source completeness) and replace-not-rebuild, but never says: **delivery into an open window is driven by ingest sequence, not presentation order** — a late-arriving *old* event must surface as an `Insert` delta inside an already-delivered window, and the presentation cursor `(order-key, id)` is only ever a *pagination* token, never a delivery gate. v1 ran exactly this split (`after_seq` pull replay vs `FeedCursor`), so it is harvest-grade knowledge, not speculation. Recommend a candidate **ledger entry #13 — "late-arriving old event silently missing from an open window"**: mechanism = window deltas keyed by ingest-seq with membership recomputed on insert; the presentation cursor type has no delivery-gating operation.

## Q4 — Best remaining fold-in / biggest reject

- **Most useful contribution overall:** naming the result-side contract at all — window policy, opaque cursors, delta vocabulary, `has_more`-as-coverage, retraction deltas. Our M0/M1 work is demand-obsessed; the falsifier will hit this within days of M4/M5, and settling it now (as an observation mode, Tier A'd once) prevents the falsifier from hand-rolling a feed — which would be the old fragmentation returning in the app.
- **Biggest reject:** the thesis. "Composite feed as the sole feed model" with protocol modules registering closure lane-mappings is the v1 feed-session *framework* asking to be rebuilt inside v2 under a new name. The engine ships a collection view over N ordinary queries; it does not ship a feed model.
