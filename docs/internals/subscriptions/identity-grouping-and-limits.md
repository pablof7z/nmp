---
title: Subscription identity, grouping, and relay limits
category: subscriptions
slug: identity-grouping-and-limits
status: partly-built, partly-decided-not-built
date: 2026-07-27
owns:
  - how demand becomes wire subscriptions
  - filter merging (coalescing) and its correctness contract
  - wire subscription identity and its failure modes
  - relay subscription/array limits and what enforces them
related:
  - docs/consults/2026-07-11-fable-coverage-attribution.md
  - docs/design/routing-and-ownership.md
  - docs/design/query-demand-and-evidence.md
issues:
  - "#899 unmergeable demands collide on one SubId and silently vanish"
  - "#900 AuthorUnion narrows an unconstrained authors filter"
---

# Subscription identity, grouping, and relay limits

This is the full account of how a query becomes REQ frames on a socket: how many
subscriptions you get, which filters get combined, what names those
subscriptions carry, and why every one of those decisions is load-bearing.

It exists because three separate defects in this area were found in one day, and
all three were consequences of a single design choice that was never written
down. Read §5 first if you only want the failure modes.

**Status is mixed and is marked per section.** Sections marked BUILT describe
shipped behaviour. Sections marked DECIDED describe an agreed design that is not
yet written. Sections marked OPEN are unresolved.

---

## 1. The pipeline — BUILT

```
app declares a query
   ↓
resolver   → demand atoms          (relay-agnostic; one per resolved value)
   ↓
router     → routing, partitioning, MERGING, identity minting
   ↓
diff_plans → previous plan vs next plan → Req / Close ops
   ↓
engine     → attribution snapshots, negentropy diversion
   ↓
transport  → one REQ frame per filter, on a socket
```

Two facts about the ends of this pipe are worth stating immediately, because
almost every misconception about this system comes from getting one of them
wrong.

**There is no merging below the router.** Transport batches *inbound* frames
(`max_verify_batch`, `max_engine_batch` in `crates/nmp-transport/src/pool.rs`)
and never coalesces outbound REQs. The final emission is
`ClientMessage::req(wire_id, vec![filter])` in
`crates/nmp-engine/src/runtime/mod.rs` — **one filter per REQ frame, one REQ per
`WireOp::Req`**. `Router::compile` step 5 is the only place filters are ever
combined.

**The router recompiles everything, every time.** `Router::compile`
(`crates/nmp-router/src/router.rs`) is documented as "THE entry point — recompile
the whole per-relay plan from `demand`, diff vs the previous plan." It is not
incremental. There is no batching window and no debounce: aggregation is
recomputed on every demand mutation, over whatever demand is live at that
instant.

That second fact is worth internalising because it answers a question people ask
repeatedly: *how long can pass between two subscriptions before they stop being
combined?* No time at all, and also forever — timing is irrelevant. Measured
against a live relay, opening subscriptions 0ms, 50ms and 250ms apart produces
byte-identical wire traffic.

---

## 2. Atoms: why demand fans out — BUILT, and correct

`Graph::compute_atoms` (`crates/nmp-resolver/src/graph.rs`) builds **the
cartesian product of the base filter across each bound field's resolved
elements**. A filter whose `#d` binding resolves to five groups becomes five
atoms, each carrying one value.

This looks like the bug. It is not. Narrow atoms are the ratified identity for
coverage, evidence, and routing
(`docs/consults/2026-07-11-fable-coverage-attribution.md` §1). Specifically:

- **Coverage** is keyed per narrow atom (`CoverageKey`), so a wide-keyed row
  would orphan per-value coverage.
- **Routing evidence** is threaded per resolved element; a pre-batched atom
  would smear per-value relay hints into one blob.
- **The outbox solver** regroups by `(Skeleton, AccessContext)` and re-unions
  authors for the k-cover — it *requires* per-author granularity.
- **Retraction** is an exact set diff on cached atoms; singleton atoms make
  withdrawal of one value surgical.

The architecture's pattern is therefore: **fan out once at the resolver for
identity; regroup downstream, per concern.** Every layer below re-aggregates
along its own axis.

Two consequences that are easy to miss:

- **Fan-out is multiplicative.** Two bound fields of size *m* and *n* produce
  *m×n* atoms. This bounds how many axes one filter should bind.
- **Consumers cannot avoid it.** Even a `Binding::Literal` set fans out per
  element. The grammar cannot express "one atom, many values", by design. So
  "the application should batch its queries" is not an available fix for
  anything in this document.

There is also a *wide* form already computed and thrown away: `Graph::wide_concrete`
merges the base with the full resolved set of every bound field. It is used only
for the local store query and dirty-marking, never for the wire.

---

## 3. Merging (coalescing) — BUILT

### 3.1 The contract

`crates/nmp-router/src/coalesce.rs` states one correctness property:

```
matches(try_merge(a, b))  ⊇  matches(a) ∪ matches(b)
```

A merged filter must match **at least** everything both inputs matched. Merging
may over-fetch; it may never under-fetch. A rule not proven to widen is dropped
and its filters ship as separate REQs.

Over-fetching is safe because `crates/nmp-router/src/deliver.rs` re-filters every
returned event against each consuming atom's *own* original filter before
delivery. Widen-only guarantees no under-delivery; local re-filtering guarantees
no over-delivery. **Do not weaken the local re-filter** — it is the reason merge
mistakes cost bandwidth rather than correctness.

### 3.2 Where merging happens

`Router::compile` partitions demand by `(RelaySessionKey, SourceAuthority)` —
per relay, per access context, per source — then calls `coalesce_with` within
each partition. Coalescing is **equal-context-only**: two atoms differing in
`AccessContext` or `SourceAuthority` never merge, and never share an id.

Coalescing runs over the **whole engine's live atom set**, not per query. Two
unrelated subscriptions that happen to produce compatible filters on the same
relay will be combined.

### 3.3 The rules as shipped

`RuleRegistry::default_widen_only()` = `[AuthorUnion, KindUnion, IdUnion]`.

Each rule requires that **exactly one field differs** and everything else is
equal — including `since`, `until`, and `tags`. That single-field restriction is
deliberate: merging two fields at once over-widens into cartesian corners.

```
{kinds:[1], authors:[A]} + {kinds:[2], authors:[A]}  → merge  → {kinds:[1,2], authors:[A]}
{kinds:[1], authors:[A]} + {kinds:[1], authors:[B]}  → merge  → {kinds:[1], authors:[A,B]}
{kinds:[1], authors:[A]} + {kinds:[2], authors:[B]}  → REFUSE
```

The third pair would merge to `{kinds:[1,2], authors:[A,B]}`, which also fetches
kind 2 from A and kind 1 from B — events neither side asked for. Sound, but
wasteful, and the waste is unbounded on sparse inputs. **Unioning all arrays at
once is ruled out.**

Three guards deserve individual attention:

- **`neither_limited`.** Both operands must carry no `limit`. A relay-side
  `limit` caps the *result count*, not the predicate: two `limit:200` REQs for
  disjoint authors each promise 200 rows; a merged `{authors: a∪b, limit:200}`
  promises 200 total. The union would silently under-fetch. Note this makes
  `AuthorUnion` **partial**, which §5.1 shows is the root of a live bug.
- **Output caps.** `IdUnion` refuses above `MAX_IDS_PER_FILTER = 256`; the
  overflow ships as further REQs rather than being dropped. Caps make a rule
  *pair-dependent* — whether two filters merge depends on their combined size,
  not on either alone.
- **`ids` presence.** `IdUnion` refuses unless both sides carry `ids`.
  `AuthorUnion` has no equivalent guard, and that asymmetry is issue #900.

### 3.4 The tag axis has no rule — this is the gap

Nothing in the registry merges on tags. `AuthorUnion` and `KindUnion` both
require `a.tags == b.tags`; `IdUnion` requires both sides carry `ids`. So two
filters differing only in a `#p` or `#d` value **never combine**, at any scale.

Measured against a live `nak serve` relay
(`crates/nmp/examples/tag_fanout_live.rs`):

| | live subscriptions | widest filter |
|---|---|---|
| 20 demands differing only in `#p` | **23 REQs**, one value each | 1 value |
| 6 demands differing only in `authors` | **1 sub**, widened in place | 6 values |

Same relay, same run, same pinned source. The author filter accumulates
`[A]` → `[A,B]` → `[A,B,C]` on one subscription id, and shrinks back down on
teardown. The tag filters never touch each other.

At scale (`crates/nmp-router/tests/tag_kill_measurement.rs`): 300 groups over 2
hosts compiles to **300 subscriptions per host** against a real-world cap of
~20, while every filter carries 1 value out of a ~500-value budget. Compiling
the identical demand with `RuleRegistry::dedup_only()` — the empty registry —
gives an **identical** result, asserted as an equality. The registry is a
measured no-op on this axis.

---

## 4. Identity — BUILT, and the source of the trouble

### 4.1 How a subscription is named today

`SubId::for_wire` (`crates/nmp-router/src/plan.rs`) derives the id from
`(relay, Skeleton::of(filter).hash() folded with source/access, access)`.

`Skeleton::of` (`crates/nmp-router/src/route.rs`) **erases `authors` and nothing
else**. Every other field — kinds, ids, tags, since, until, limit — stays in the
hash.

The erasure has a purpose, stated in `plan.rs`: adding or removing an author
re-uses the same id, so on the wire that is **one overwriting REQ**, not a
close-and-reopen of everything. NIP-01 defines a REQ with an existing sub-id as
replacing that subscription's filter. This is what produces the in-place
widening measured above.

### 4.2 The bet the erasure makes

Deleting a field from the id is a bet: *anything that lands in the same id will
have been merged into one filter first.*

When the bet holds, one id names one filter and everything works. When it fails,
two filters share one id — and `diff_plans` keys its delta by `SubId`, so the
duplicate collapses. One REQ never reaches the wire.

**The general statement:** identity erasure is *static*; mergeability is
*dynamic*. Wherever they diverge, demand is silently lost.

### 4.3 Determinism is bookkeeping, not a requirement

The id is a pure function of the filter so that the router need not remember
which id it previously gave a subscription. NIP-01 itself requires only that ids
are unique per connection and that reuse means replacement.

Verified: nothing in reconnect/replay, `clear_session`, NIP-77 role ids,
persistence, or the #106 anti-alias tests depends on the derivation. Coverage is
consulted only by the `MaxAge` freshness gate and by diagnostics — **never
during filter construction**.

This matters because determinism is what buys the entire problem space in §5.

### 4.4 Wire string format

The wire string is the 64-hex-character `Display` of a 32-byte hash. That is
**exactly NIP-01's 64-character cap.** Consequences:

- A visible prefix (e.g. `+`) makes it 65 characters — a protocol violation.
- A truncated id is unsafe: the inflight map is keyed `(session, string)` and
  **silently overwrites on collision**. At 24 bits and 300 subscriptions, that is
  ~0.3% collision probability per compile — a certainty over a session, and it
  manifests as misattributed EOSE.
- Any mark belongs *inside* the hash (a folded byte), where it costs zero
  characters and zero entropy. Nothing ever parses the string back.

---

## 5. The failure modes — all three measured

### 5.1 Unmergeable filters collide and demand vanishes (#899)

`neither_limited` makes `AuthorUnion` partial. Two atoms identical except
`authors`, both carrying a `limit`, refuse to merge — then collide on the erased
skeleton.

Measured against the real router:

```
demand atoms:               2
WireReqs in the plan:       2
distinct SubIds:            1
REQs actually emitted:      1
ops on identical recompile: 0
```

The second demand is planned, **reports as planned**, never reaches the relay,
and never repairs. Silent demand loss is the worst failure class in this system:
everything downstream believes the request is live.

`limit` is the *trigger*, not the defect — under `dedup_only()` two ordinary
unlimited atoms collide identically. At scale, 6 authors × 3 shapes plans 13
WireReqs onto **3** ids.

A second consequence of the same root: `crates/nmp-engine/src/core/query.rs`
resolves a REQ's `absorbed` coverage keys with `reqs.iter().find(...)` —
first-match-wins — so a duplicated id **mis-attributes coverage before the diff
even drops the REQ**.

### 5.2 A merge rule narrows a filter (#900)

`AuthorUnion` accepts `None` vs `Some` (nothing excludes `None`), then does
`a.authors.unwrap_or_default()` — turning "unconstrained" into "empty set":

```
{kinds:[1], authors: None}   ← matches EVERY author
{kinds:[1], authors: {x}}
             ↓
{kinds:[1], authors: {x}}    ← now matches only x
```

This **violates the widen-only contract** — the single correctness property the
whole module rests on. Events legitimately demanded are never requested.

It survived for **two** reasons, and the second is the subtler one:

1. The property-test generator never paired `None` with `Some` — it built both
   operands as `Some(non-empty)`, with `tags` always identical and empty.
2. **A widening property over pairs no rule accepts is silently green.** Nothing
   asserted that the rules ever *fired* during the run. A generator can drift
   into producing only unmergeable pairs and the test still passes.

The fix therefore carries **fire counters** as a hard failure: each rule must
merge a minimum number of pairs per run (measured: AuthorUnion 17, KindUnion 23,
IdUnion 20 per 256 cases). Vacuity is now a test failure, not a silent pass.

`KindUnion` had the identical defect and `IdUnion` a narrower version of it
(refusing `None` but accepting `Some(∅)`). All three now share one admission
test. Notably the new generator found the `KindUnion` defect **unaided** — it was
not written to look for it.

### 5.3 Tag fan-out exceeds relay limits (see §3.4, §6)

Not a correctness bug, an operational one: 300 subscriptions against a cap of
~20.

### 5.4 Not a bug: duplicate REQs at connect

Measured: 10 subscriptions produce 12 REQ frames; 40 produce 43. Constant, never
more than two sends of one filter.

This is `apply_replay` behaving as documented: on `RelayConnected` it resends
`EngineCore`'s full current req list — "even on the very first `Connected` for a
session" — calling the duplicate "a harmless, idempotent overwrite." Harmless
client-side; the relay does re-run the query. Not a plan-diff bypass.

---

## 6. Relay limits — parsed, and enforced nowhere

Relays cap **concurrent subscriptions** at roughly 20 (sometimes up to 200), and
accept **arrays of ~500 values** without complaint. Fan-out inverts exactly the
resource relays are provisioned for.

The codebase already knows this and stopped enforcing it:

- `crates/nmp-router/tests/kill_measurement.rs` carries
  `MAX_SUBS_PER_RELAY = 20` and `MAX_FILTER_AUTHORS = 1_000`, commented *"relays
  accept large author arrays but cap concurrent subscriptions."*
- Those thresholds were once a runtime type, `RelayLimits`, **deleted in #123 as
  "a never-enforced router contract"** — the kill measurement was its only
  consumer.
- NIP-11 `max_subscriptions` **is** parsed
  (`crates/nmp-engine/src/relay_information.rs`) and surfaced through FFI, but
  nothing in `nmp-engine/src/core/` or `nmp-router/src/` reads it. Planning never
  consults it.
- The router's `cap` parameter bounds the number of **relays**, not
  subscriptions per relay.

`diag.rs` reports `wire_sub_count` per session and nothing consumes it.

**DECIDED (owner, 2026-07-27): enforced, not advisory.** A per-relay
subscription budget becomes a real planning input, using the existing `limited` /
`refused_sessions` reporting seam rather than reviving `RelayLimits`.

Sequencing is load-bearing: pre-collapse, a budget over 300-vs-20 only forces
triage that drops 280 atoms' coverage — fail-closed but useless. **Land the
collapse first**, then the budget is a guard rail rather than a guillotine.

Note also `max_subid_length`: parsed, unread. A relay advertising `< 64` would
reject our fixed-length ids and nothing would notice. Worth a diagnostic. It must
**never** feed id derivation — NIP-11 documents refresh, and a mutable derivation
input is identity instability.

---

## 7. The decided design — DECIDED, NOT BUILT

Three designs were reviewed adversarially. The chosen one has two halves that
are independent of each other.

### 7.1 Merging: one structural rule

Replace `AuthorUnion` / `KindUnion` / `IdUnion` with a single rule derived from
the filter's shape:

```
arrays  (kinds, authors, ids, and EACH tag name)  → union, when exactly one differs
scalars (since, until)                            → must be equal
limit                                             → refuse
caps                                              → refuse if the result exceeds
```

Tags stop being a missing fourth rule and become instances of the general case.
Two details are load-bearing:

- **One component per tag NAME.** Tags are conjunctive across names, so
  `{#e:X}` and `{#p:Y}` must never merge — the result would demand both. Merging
  across tag *names* is a narrowing, not a widening.
- **Refuse an unconstrained operand on every axis.** This is the #900 fix,
  generalised — but the *shape* of "unconstrained" differs per axis, and the
  polarity on tags is **inverted**. See §3.5.

### 3.5 What "unconstrained" means, per axis — MEASURED, counterintuitive

Merging must refuse an operand that leaves the merged axis unconstrained,
because unioning it away narrows the result. Which shapes are unconstrained is
not uniform, and it is the opposite of what most readers assume.

For `authors` / `kinds` / `ids`, **both** of these match every event:

| shape | meaning |
|---|---|
| `None` | unconstrained |
| `Some(∅)` | **also unconstrained** — `nostr`'s `match_event` treats an empty set as no constraint, NOT as "matches nothing" |

So both must be refused. `Some(∅)` is constructible through the FFI option
boundary, and it does **not** collide with `None` in `DescriptorHash` —
`canonical_encoding` emits `null` versus `[]`.

For **tags**, the polarity flips:

| shape | meaning |
|---|---|
| tag name **absent** | unconstrained — matches everything |
| tag name present with `∅` values | matches **nothing** — tagged and untagged events alike |

So on tags the trap is folding an **absent** name into a present one; `{t: ∅}` is
the harmless end. This is the reverse of the array axes, and it is why "refuse
`None` vs `Some`" cannot be transplanted onto tags unexamined.

Recorded because the structural rule (§7.1) has to get this right on four axes
with two different polarities.

### 7.2 Identity: allocated ids, structural-signature matching

The wire id becomes an **allocated opaque token** — minted at first appearance,
carried forward by matching, closed when unmatched — rather than a function of
the filter. Matching uses a per-component signature:

```
since | until | h(kinds),|kinds| | h(authors),|authors| | h(ids),|ids|
      | h(values) per tag name   | limit
```

A new filter **continues** the prior it differs from in **exactly one
component**, with **zero-diff ranked first** (an unchanged filter must match
itself, or no-op recompiles break).

- **Ties** break by value-set overlap on the differing component, then by
  canonical filter hash. Overlap is *content-grounded*, not positional — this is
  not the sort-order matching that was rejected, where churn anywhere reorders
  unrelated subscriptions.
- **Injectivity comes from the assignment** — each prior id assigned at most
  once, fresh ids unique by minting — not from id content.
- **Never recycle an id** within a session: a reused id lets a straggler frame
  resolve against a different filter's inflight state. Fold a per-router
  monotonic seed into the mint. Restart is safe because connections drop and
  `clear_session` wipes stale mappings.

**Why this over the alternatives:**

| design | verdict |
|---|---|
| deterministic ids + collision check | sound, smallest delta, and the only one where multi-window siblings are stable. But keeps a hysteresis boundary, keeps the limited-churn cost, and re-imposes a soundness proof on every future erased axis. |
| allocated ids, lineage matching (`absorbed` overlap) | works, but couples identity to `coverage_key`'s erasure choices — and `coverage_key` is version-tagged for evolution, so changing it would silently change which subscriptions continue. |
| **allocated ids, signature matching** | **chosen.** Filter-local, no coverage coupling, resolves prior-merge cases without overlap arithmetic, handles full turnover better. |

**What it deletes:** the erasure rules, any un-groupable marker, the collision
scan, and — the win nobody predicted — the limited-churn cost. Because
injectivity comes from the assignment rather than from id content, a *limited*
filter whose authors churn is a one-component difference and **overwrites in
place**. It also closes the Close/reopen straggler race in §8.2, because a
never-recycled id makes a late EOSE for a closed subscription resolve to nothing.

**What it costs:** the router holds matching state (bounded — it is `prev_plan`,
already held and pruned every compile, not the unbounded-map class); ids stop
being recomputable from filters, so a log line needs the plan to interpret; ~12
test fixtures that predict ids need rework; the negentropy prober must be
domain-separated from the allocated namespace.

### 7.3 The relationship between the halves

They are independent. Merging controls **how many** subscriptions exist;
identity controls **what they are called** and whether growth replaces in place.
Neither substitutes for the other: without merging, 300 filters remain 300
subscriptions whatever their ids; without identity, growth churns.

---

## 8. Accepted costs and open corners

### 8.1 Accepted — pin as tests, do not "fix"

- **Compound churn.** Two components moving in one recompile — an author
  resolves *and* the window advances — is a 2-diff, so it closes and reopens.
  Reviewed and **dismissed by the owner as not a real workload** (2026-07-27);
  not to be measured or designed around. **Do not relax to "≤2 components with
  overlap evidence"** regardless — every relaxation re-imports lineage matching's
  ambiguity for no gain.
- **Window siblings.** Two filters identical except `until`, both moving in one
  compile, are each one-diff from each prior, and a scalar has no overlap metric.
  Needs an arbitrary-but-deterministic tiebreak; the residual swap is accepted.
  Heavy multi-window pagination is the single workload that would argue for the
  collision-check design instead.

### 8.1b Retraction — DECIDED (owner, 2026-07-27)

When a newer answer invalidates what we previously held, **close whatever is now
known to be incorrect and open it again with the right values.** Correctness
first; do not try to preserve a subscription whose demand has been contradicted.

Stated preference on *how*: this should be expressed **declaratively or via
signals**, not as imperative teardown bookkeeping scattered through the
recompile path. The recompile is already a full recomputation from demand
(§1), so the natural shape is that retraction falls out of the recomputed
demand rather than being a separate imperative step.

Note the interaction with §7.2: under signature matching, a filter whose values
shrink is still a one-component difference, so the common case is an in-place
overwrite carrying the survivor set — the same wire behaviour the author axis
already exhibits (an 8-author filter shrinking one value at a time, never a
CLOSE). Explicit closure is needed only where the demand is genuinely gone, not
merely narrower.

### 8.2 OPEN — the Close/reopen straggler race

The coverage ruling assumed a Close leaves pending snapshots to be "harmlessly
popped never-attributed." The code now **discards** the inflight FIFO and wire
mapping at Close. So: Close at compile N, re-open the same skeleton at N+1
re-registers the same wire string with a fresh FIFO, and a straggler EOSE from
the pre-Close REQ mints coverage for a request the relay has not finished
serving.

Correct layer: the wire string is a **per-connection namespace** owned by
`EngineCore`; the router `SubId` is a **plan identity**. Incarnation freshness
belongs at the engine's wire-string boundary. §7.2's never-recycle rule closes
this as a by-product; if that design is not taken, this needs its own fix.

### 8.3 Rejected, with reasons

- **Batching at the resolver** (multi-value atoms). Breaks coverage keying,
  smears routing evidence, breaks the per-author outbox solve, coarsens
  retraction diffs.
- **Unioning all arrays at once.** Cartesian over-fetch; ruled out by the owner.
- **Erasing tags inside `route::Skeleton`.** It is the outbox grouping key and
  the `with_authors` reconstruction basis.
- **Emitting `wide_concrete` to the wire** while keeping narrow atoms for
  attribution. Dual demand truth; bypasses the widen-only proof and the
  coverage-containment mechanism.
- **Coverage-aware merging.** A merge that asks for *less* than the naive union
  cannot satisfy the widening contract. Not a difficulty — a contradiction.
- **Multi-filter REQs.** NIP-01 allows a filter list per subscription, which
  would make collisions structurally impossible. But EOSE and CLOSE are
  per-subscription, so a list coarsens per-filter completion and forbids
  independent teardown. At most a bandwidth optimisation; not an identity fix.
- **Debounce / batching windows.** Measured to buy nothing: there is no time
  window to widen, regrouping costs one in-place REQ, and a re-served event
  produces zero additional row deltas because canonical dedup absorbs it.

---

## 9. Evidence index

Everything asserted above is measured, not reasoned. Reproduce with:

| what | where |
|---|---|
| engine-level fan-out; warm cache; EOSE independence; multi-relay; 50 values | `crates/nmp-engine/tests/core_headless/derived_tag_fanout.rs` |
| three-way cost comparison; the #899 falsifier and its control | `crates/nmp-router/tests/tag_fanout_churn.rs` |
| 300 groups vs a 20-subscription cap; registry-is-a-no-op equality | `crates/nmp-router/tests/tag_kill_measurement.rs` |
| author-axis limits under `AuthorUnion` (the pre-existing twin) | `crates/nmp-router/tests/kill_measurement.rs` |
| live REQ frames against a real relay | `crates/nmp/examples/tag_fanout_live.rs` |
| intended behaviour, `@wip` | `features/routing/subscription-collapse.feature` |

The live probe is the strongest evidence and the cheapest to re-run:

```
nak serve --port 10547 --verbose
cargo run -p nmp --example tag_fanout_live -- ws://localhost:10547 20 0 both
```

---

## 10. Rules of thumb

1. **Fan-out at the resolver is correct.** Fix the wire, never the atoms.
2. **Widen-only is the contract.** A merge may over-fetch; it may never
   under-fetch. The local re-filter is what makes that safe — do not weaken it.
3. **Merging controls count; identity controls churn.** Different problems.
4. **Deleting a field from an id is a bet that merging happened.** If merging can
   refuse, the bet can lose, and losing is silent.
5. **A `debug_assert` is not a guard.** It compiles out. If an invariant matters
   in production, check it in production.
6. **Property-test generators are the real defence.** #900 lived because a
   generator never produced `None`. Every axis added here needs generator
   coverage before it needs a rule.
