---
title: Subscription identity, grouping, and relay limits
category: subscriptions
slug: identity-grouping-and-limits
status: built
date: 2026-07-27
owns:
  - how demand becomes wire subscriptions
  - filter merging (coalescing) and its correctness contract
  - wire subscription identity and its failure modes
  - relay subscription/array limits and what enforces them
  - what a widening subscription costs the relay, and why (§11)
related:
  - docs/consults/2026-07-11-fable-coverage-attribution.md
  - docs/design/routing-and-ownership.md
  - docs/design/query-demand-and-evidence.md
issues:
  - "#899 unmergeable demands collide on one SubId and silently vanish"
  - "#900 AuthorUnion narrows an unconstrained authors filter"
  - "the tag axis has no merge rule (§3.4)"
  - "#933 per-EOSE delta subscriptions — measured, analysed, NOT BUILT (§11)"
  - "#1340 observation admission no longer reprojects every sibling"
  - "#1341 pending app demand is grouped before immutable relay admission"
---

# Subscription identity, grouping, and relay limits

This is the full account of how a query becomes REQ frames on a socket: how many
subscriptions you get, which filters get combined, what names those
subscriptions carry, and why every one of those decisions is load-bearing.

It exists because three separate defects in this area were found in one day, and
all three were consequences of a single design choice that was never written
down. Read §5 first if you only want the failure modes.

**Status is marked per section.** Sections marked BUILT describe shipped
behaviour; sections marked OPEN are unresolved. §7's design is now built —
§7.1 as `nmp_router::StructuralUnion`, §7.2 as `nmp_router::wire_id` — and
§6's per-relay subscription budget is built as `nmp_router::CompileBudget`.
What remains unbuilt is §8.1b/§8.2 and #933's coverage-driven derived-demand
growth. The 10ms app-admission cohort in §1 and §11.5 is built; it is not the
per-EOSE mechanism #933 describes.

---

## 1. The pipeline — BUILT

```
app declares a query
   ↓
resolver   → demand atoms + immediate local projection
   ↓
pending admission (10ms from first uncovered app demand)
   ↓
router     → route and MERGE only that unsent cohort, per relay/context/source
   ↓
admission  → append immutable REQs, or attach to exact active coverage
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

**App admission and global replanning are different transitions.** Opening an
observation reads only that observation's canonical local projection. If an
active REQ already carries its exact `CoverageKey`, the new observation simply
attaches to it. Otherwise the first uncovered app request arms a 10ms,
first-arrival-anchored deadline. More compatible observations may join that
pending cohort without extending the deadline. When it expires, NMP routes the
cohort and coalesces it inside each `(RelaySessionKey, SourceAuthority)`
partition.

REQs become immutable when admitted. A later cohort never widens, narrows,
renames, or closes a still-useful incumbent; it opens additional REQs for the
coverage still missing. Withdrawal closes a shared REQ only after its last
active absorbed key is gone. This avoids paying the relay to rerun a broad
query merely because another screen element appeared.

`Router::compile` still performs a whole-demand replan when the world actually
invalidates routing -- for example an active-account reroot, route-directory
change, or relay-budget change. Those transitions may genuinely move existing
demand between sessions. Their old and new plans are compared by exact
`(session, sub-id, limited)` coverage assignment, and only affected
observations refresh acquisition evidence. A plan-only change never rereads
unrelated event rows.

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

During ordinary app admission, coalescing runs over the **pending cohort**, not
per query and not over already-sent requests. Two unrelated observations that
arrive in the same cohort and land in the same relay/context/source partition
can therefore combine. Existing REQs participate only through their exact
absorbed coverage keys: they can satisfy a new observation, but are never
merge candidates. A true routing invalidation still uses the whole-demand
compiler described in §1.

### 3.3 The rule as shipped

`RuleRegistry::default_widen_only()` = `[StructuralUnion]` — ONE rule, derived
from the filter's shape rather than named after a field (§7.1). It requires
that **exactly one array component differs** and everything else is equal.
That single-component restriction is deliberate: merging two at once
over-widens into cartesian corners.

Until 2026-07-27 this was three rules — `AuthorUnion`, `KindUnion`, `IdUnion` —
each hard-coding one field, with `tags` required equal by all three. §3.4
records what that cost. The guards below survived the collapse unchanged.

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
- **Output caps.** The union refuses above `MAX_IDS_PER_FILTER = 256` on
  `ids`, and above `MAX_TAG_VALUES_PER_FILTER = 500` under any one tag name;
  the overflow ships as further REQs rather than being dropped. Caps make the
  rule *pair-dependent* — whether two filters merge depends on their combined
  size, not on either alone.
- **Unconstrained-operand admission.** Both operands must actually constrain
  the axis being unioned. `AuthorUnion` had no such guard, and that asymmetry
  was issue #900; the shared `both_constrain` test generalised it to every
  axis, with the inverted polarity §3.5 describes on tags.

**Cap-driven chunking is greedy, and the chunk COUNT is an artifact of merge
order, not `⌈n/cap⌉`.** Mutually-mergeable filters pair up in a doubling
cascade (1→2→4→…), so chunks stall at the largest power of two still under the
cap: `MAX_IDS_PER_FILTER = 256` lands exactly on its cap, while 500 stalls at
256 and leaves real headroom unused. Measured: 1200 `#d` values at a 500 cap
produce **4** filters sized `[256, 256, 256, 432]`, not 3.

What *is* provable is a window. A terminal state of `merge_fixed_point` has no
mergeable pair left, and only the cap can refuse two same-axis chunks, so every
pair sums over the cap — meaning at most one chunk holds `cap/2` or fewer
values. With `k` chunks over `n` values that gives `(k-1)·(cap/2 + 1) ≤ n`,
alongside the floor `⌈n/cap⌉`. Tests assert that window rather than the number,
and `features/routing/subscription-collapse.feature` was revised from "exactly
3 subscriptions" to a bound for the same reason. The inefficiency is
bin-packing only: every value still ships, and the count stays orders of
magnitude inside the ~20-subscription ceiling. Improving it means changing the
fixed point, which a differential oracle pins byte-for-byte against its naive
twin — separate work, not a tidy-up.

The scale falsifiers divide responsibility without making the BDD runner a
load generator (#994). The router mechanism test still feeds all 1200
singleton atoms and pins the exact `[256, 256, 256, 432]` result. The BDD
scenario carries the same 1200 values through 21 independent app watches:
without structural union that exceeds its relay ceiling of 20, while the
passing path must still put every value on the wire and keep each filter at or
below 500. This avoids 1200 synchronous whole-plan recompiles while preserving
the acceptance kill.

### 3.4 The tag axis had no rule — CLOSED

Until 2026-07-27 nothing in the registry merged on tags. `AuthorUnion` and
`KindUnion` both required `a.tags == b.tags`; `IdUnion` required both sides to
carry `ids`. So two filters differing only in a `#p` or `#d` value **never
combined**, at any scale.

Historical measurement under the removed deterministic same-id scheme, against
a live `nak serve` relay
(`crates/nmp/examples/tag_fanout_live.rs`):

| | live subscriptions | widest filter |
|---|---|---|
| 20 demands differing only in `#p` | **23 REQs**, one value each | 1 value |
| 6 demands differing only in `authors` | **1 sub**, widened in place | 6 values |

Same relay, same run, same pinned source. The author filter accumulates
`[A]` → `[A,B]` → `[A,B,C]` on one subscription id, and shrinks back down on
teardown. The tag filters never touch each other.

At scale (`crates/nmp-router/tests/tag_kill_measurement.rs`): 300 groups over 2
hosts compiled to **300 subscriptions per host** against a real-world cap of
~20, while every filter carried 1 value out of a ~500-value budget. Compiling
the identical demand with `RuleRegistry::dedup_only()` — the empty registry —
gave an **identical** result, asserted as an equality. The registry was a
measured no-op on this axis.

**Closed by `StructuralUnion`.** Same falsifier, same file, re-measured:

| | dedup-only floor | `default_widen_only()` |
|---|---|---|
| subscriptions per host | 300 | **1** |
| total wire subs, 2 hosts | 600 | **2** |
| widest filter | 1 value | **300 values** (bound 500) |

The equality assertion is now a strict improvement and the kill verdict is
`fired=false`. Two neighbouring measurements moved with it:
`tag_fanout_churn.rs` now records resolver fan-out and a pre-batched atom
compiling to the *same plan* (8 REQs, 7 raw-plan CLOSEs, and 1 live sub either
way); each byte-changing step is a typed fresh-id transition, so EngineCore
offers its successor before committing the predecessor close.
`derived_tag_fanout.rs`'s tag-versus-author contrast has become an asserted
equality.

---

## 4. Identity — BUILT

### 4.1 How a subscription is named today

`SubId::allocate` (`crates/nmp-router/src/plan.rs`) mints an opaque,
never-reused id from the router's monotonic counter plus relay, source, and
access context. The filter is not encoded into the token.

An exact byte-identical request keeps its allocated id and emits no wire work.
When the filter bytes change, structural component matching identifies the
predecessor but the successor receives a fresh id. `CompileOutcome` records the
typed predecessor/successor transition. `EngineCore` offers the successor REQ
first and retires the predecessor only after the exact successor handoff is
accepted. A local refusal keeps the predecessor live and owns one retry.

### 4.2 The bet the erasure makes

The removed deterministic scheme erased `authors` from the filter skeleton.
That made a bet: *anything that lands in the same id will have been merged into
one filter first.*

When the bet holds, one id names one filter and everything works. When it fails,
two filters share one id — and `diff_plans` keys its delta by `SubId`, so the
duplicate collapses. One REQ never reaches the wire.

**The general statement:** identity erasure is *static*; mergeability is
*dynamic*. Wherever they diverge, demand is silently lost. Allocated opaque
ids remove that bet from current identity.

### 4.3 Determinism is bookkeeping, not a requirement

The current id is not a pure function of the filter. The router retains its
allocated plan identity and a monotonic mint counter. NIP-01 requires only that
ids are unique per connection; exact zero-diff retention is local bookkeeping,
not a protocol requirement that byte changes reuse an id.

Verified: nothing in reconnect/replay, `clear_session`, NIP-77 role ids,
persistence, or the #106 anti-alias tests depends on the derivation. Coverage is
consulted only by the `MaxAge` freshness gate and by diagnostics — **never
during filter construction**.

This matters because the removed deterministic erasure created the entire
problem space in §5; the current allocated namespace excludes it.

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

### 5.4 Duplicate REQs at connect — CLOSED (#1075)

The old path gave reconnect replay two owners: the transport worker retained a
REQ preamble while `EngineCore` also resent its full plan on
`RelayConnected`. The first Connected edge could likewise arrive after an
initial REQ had already been accepted by that still-dialing handle. Both paths
sent a byte-identical replacement, causing the relay to rerun and restream the
same query.

The reducer now remembers the exact accepted `(session, subscription, filter,
transport generation)`. An unchanged request already accepted on that
generation mints neither another request revision nor another wire frame. A
changed filter remains a real NIP-01 replacement. Disconnect retires the
accepted state, so the fresh generation receives the current request exactly
once. The transport's automatic reconnect preamble stays empty; replay after
the ordered Connected edge is reducer-owned.

---

## 6. Relay limits — BUILT (#931)

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

### 6.1 As built

`Router::compile` takes a `CompileBudget` (`nmp-router/src/budget.rs`) instead
of a bare relay cap: the operator's whole-demand relay ceiling, plus each
relay's own `AdvertisedRelayLimits`. A bare `usize` still converts into one, so
every existing caller says exactly what it always said.

The engine builds it from `nip11_information`, the same map diagnostics already
reads and `recompile` already prunes — no second cache to age out. Enforcement
runs per session AFTER coalescing and AFTER wire-token assignment
(`refuse_over_budget`), because both decide what the count actually is.

**Absence is not a number.** A relay that advertised nothing is UNBUDGETED.
Two of the eight relays measured on 2026-07-27 (relay.nostr.band,
relay.snort.social) publish no document at all; a fabricated default would drop
their demand over a guess, and would flap damus between 200 and that guess
whenever one HTTP GET failed. What guards an unadvertised relay instead is the
per-session subscription COUNT in diagnostics and in
`features/routing/relay-subscription-limits.feature` — a fan-out escape is a
defect for CI to catch, not a reason to refuse a user's demand in production.
(Fable, consulted adversarially on fail-open versus a guessed default, reached
the same verdict independently.)

**Refusal is loud.** Every refused subscription's exact logical owners join
`RelayPlan::limited_demands`, so `plan_is_fresh_for` refuses to call those
demands fresh and `acquisition_evidence` reports
`ShortfallFact::LocalLimit` to the app.
`RelayPlan::subscription_shortfalls` carries the per-session (budget, planned,
refused) triple, and diagnostics carries `subscription_budget` /
`subscriptions_refused` per session. A relay advertising ZERO joins
`refused_sessions` instead, which preserves that field's "absent from the plan
by construction" invariant; a merely-trimmed session stays planned.

**Incumbents outrank newcomers.** Ranking by coverage alone would evict an
established subscription whenever a newer one outranked it, re-admit it next
compile, and oscillate — churn caused by the budget itself. A subscription the
previous plan already carried wins.

**Refresh.** NIP-11 acquisition is driven by connect, so an advertisement is
always learned AFTER the compile that planned the relay. Resolution therefore
recompiles — but only when the pair `(max_subscriptions, max_subid_length)`
actually moved, never on revalidation or `supported_nips` churn — and then
refreshes handles so the shortfall reaches the subscriber. Nothing advertised
feeds identity: wire ids are allocated tokens (§7.2), and the budget is
deliberately not among their inputs.

### 6.2 `max_subid_length` — diagnosed, never derived from

Parsed and unread until now. A relay advertising `< 64` would reject our
fixed-length ids and nothing would notice. It is now reported per session
(`subid_length_rejects_our_ids`) and enforces nothing. It must **never** feed id
derivation — NIP-11 documents refresh, and a mutable derivation input is
identity instability.

---

## 7. The decided design — BUILT

Three designs were reviewed adversarially. The chosen one has two halves that
are independent of each other.

### 7.1 Merging: one structural rule — BUILT

`nmp_router::StructuralUnion` replaces `AuthorUnion` / `KindUnion` / `IdUnion`
with a single rule derived from the filter's shape:

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

**One component model, two consumers.** `Component` and `differing` live in
`crates/nmp-router/src/component.rs`, shared by `coalesce` and `wire_id`. The
sharing is load-bearing rather than incidental: coalescing and predecessor
selection must agree about which single logical axis changed. A byte-changing
successor still receives a fresh token and is offered before its predecessor
closes; the component match identifies that transition, never an in-place
overwrite. §7.3's independence is about FUNCTION (count versus transition
identity), not about the coordinate system.

`differing` destructures `ConcreteFilter` by name so that adding an eighth
field is a compile error there. A field it forgot would be reported as
always-equal — which for `wire_id` misnames a subscription, but for `coalesce`
is a real NARROWING: the merge would keep `a`'s value and drop `b`'s
constraint.

### 7.2 Identity: allocated ids, structural-signature matching — BUILT

The wire id is an **allocated opaque token** — minted at first appearance and
never recycled — rather than a function of the filter. Matching uses a
per-component signature to identify predecessor relationships:

```
since | until | h(kinds),|kinds| | h(authors),|authors| | h(ids),|ids|
      | h(values) per tag name   | limit
```

A byte-identical filter keeps its prior token and emits no wire work. A new
byte-changing filter may identify as predecessor the prior it differs from in
**exactly one component**, with **zero-diff ranked first** so unchanged filters
always retain themselves. The changed filter still receives a fresh token.

- **Ties** break by value-set overlap on the differing component, then by
  canonical filter hash. Overlap is *content-grounded*, not positional — this is
  not the sort-order matching that was rejected, where churn anywhere reorders
  unrelated subscriptions.
- **Injectivity comes from the assignment** — each prior request selected as a
  predecessor at most once, fresh ids unique by minting — not from id content.
- **Never recycle an id** within a session: a reused id lets a straggler frame
  resolve against a different filter's inflight state. Fold a per-router
  monotonic seed into the mint. Restart is safe because connections drop and
  `clear_session` wipes stale mappings.
- **Changed bytes open before close.** Router reports the typed
  predecessor/successor relation. EngineCore offers the fresh-id successor,
  keeps the predecessor on local refusal, and retires it only after the exact
  acceptance edge. Exact zero-diff alone retains an id.

**Why this over the alternatives:**

| design | verdict |
|---|---|
| deterministic ids + collision check | sound, smallest delta, and the only one where multi-window siblings are stable. But keeps a hysteresis boundary, keeps the limited-churn cost, and re-imposes a soundness proof on every future erased axis. |
| allocated ids, lineage matching (`absorbed` overlap) | works, but couples identity to `coverage_key`'s erasure choices — and `coverage_key` is version-tagged for evolution, so changing it would silently change which subscriptions continue. |
| **allocated ids, signature matching** | **chosen.** Filter-local, no coverage coupling, resolves prior-merge cases without overlap arithmetic, handles full turnover better. |

**What it deletes:** the erasure rules, any un-groupable marker, and the
collision scan. A limited filter whose authors churn can still identify its
predecessor by one-component difference, but its byte-changing successor uses
a fresh id. It also closes the Close/reopen straggler race in §8.2, because a
never-recycled id makes a late EOSE for a retired subscription resolve to
nothing.

**What it costs:** the router holds matching state (bounded — it is `prev_plan`,
already held and pruned every compile, not the unbounded-map class); ids stop
being recomputable from filters, so a log line needs the plan to interpret; ~12
test fixtures that predict ids need rework; the negentropy prober must be
domain-separated from the allocated namespace.

### 7.3 The relationship between the halves

They are independent in FUNCTION, and share one coordinate system (§7.1).
Merging controls **how many** subscriptions exist;
identity controls **what they are called** and which prior request a fresh-id
successor conditionally replaces. Neither substitutes for the other: without
merging, 300 filters remain 300 subscriptions whatever their ids; without
identity, a byte-changing transition cannot preserve its predecessor until the
successor is accepted.

---

## 8. Accepted costs and open corners

### 8.1 Accepted — pin as tests, do not "fix"

- **Compound churn.** Two components moving in one recompile — an author
  resolves *and* the window advances — is a 2-diff, so it cannot claim a
  structural predecessor and opens as an independent fresh-id request while
  unmatched prior work closes normally.
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
shrink is still a one-component difference, so the common case is a typed
fresh-id successor carrying the survivor set. EngineCore offers that successor
before closing its predecessor and keeps the predecessor live if local
placement is refused. Demand that is genuinely gone still closes directly.

### 8.1c PARTIAL (#1004) — ordinary-plan assertions raced NIP-77 capability proof

Found while un-`@wip`ing the BDD scenarios and originally attributed to an
indeterminate live-engine recompile boundary. Issue #1004 captured the
recurring active-suite failure and its full wire trace identified the actual
interleaving.

The relay's behavioral NIP-77 probe sometimes resolved between two watch
mutations. The first mutations therefore emitted ordinary planned REQs; the
next broad Public recompile correctly opened a distinct `limit:0` NIP-77 live
candidate. Once that candidate's exact EOSE arrived, the gap-free handoff
correctly closed the overlapped ordinary predecessor. The historical failing
record was unambiguous under the implementation at that time: two in-place
ordinary replacements, then a fresh `limit:0` candidate covering the enlarged
author set, then the predecessor CLOSE. Current ordinary byte changes instead
use the fresh-id accepted-open-before-close transition in §7.2.

That is not a router-plan CLOSE and not an observation race. It is a harness
capability race: the assertion claimed to measure ordinary NIP-01 plan
replacement while allowing the relay to change acquisition protocol midway
through the stimulus.

The subscription-collapse feature now drives an explicit NIP-11 document
whose `supported_nips` excludes 77, so the engine deterministically suppresses
the behavioral probe and the scenario observes only the ordinary router plan.
The no-CLOSE assertions additionally reject any `limit:0` live candidate by
its exact wire shape before evaluating the plan invariant, so losing that
isolation fails with the real cause rather than reviving the flaky
misclassification. `nmp-test-support` separately pins that wire-shape
classification.

That closes #1004's active ordinary tag/author failure. One separate residue
remains `@wip`: the derived-group stimulus is downstream of an inbound EVENT,
so outbound-wire quiet cannot establish that ingestion, resolution, and
recompilation reached a named generation. It has an engine-level falsifier but
the live BDD harness still lacks that generation boundary. The earlier
investigation below is retained for that residue; its claim that the ordinary
author/tag scenarios demonstrated the same recompile race is superseded by
the complete #1004 wire trace above.

The wire `Then` steps read the relay's socket ONCE, after the client wire has
been quiet for a window. That is not sufficient for any assertion whose subject
is downstream of an INBOUND frame: `wait_wire_quiet` watches client-to-relay
traffic only, so "seed a kind:39001 → relay pushes it → the client ingests,
re-resolves the derived set, recompiles, emits a REQ" has a genuinely quiet
client wire in the middle of it. The read lands in that gap and reports what
had happened by then.

Making those steps poll (`nmp_bdd::world::wire_record_when`) was tried. It
exposed two effects which the original investigation conflated. The ordinary
tag/author CLOSEs were NIP-77 handoffs, now deterministically excluded by
#1004. The derived-group scenario still crosses an inbound pipeline for which
the harness cannot name a completed plan generation. Measured over eight
consecutive suite runs against real in-process relays:

- a three-value `#p` watch then dropping one closed **two** subscriptions;
- the derived five-groups-then-one `#d` sequence closed **one**, and in another
  run opened a second subscription instead of widening;
- the pre-existing author-axis regression guards also showed a CLOSE; #1004's
  complete record later proved that CLOSE was a NIP-77 handoff, not the
  derived recompile-boundary residue.

The end state was correct every time: one subscription carrying every value,
nothing under-fetching. The remaining derived observation is therefore churn,
not a demonstrated correctness defect.

The mechanism is interleaving. `tag_fanout_churn.rs` presents every growth step
as one recompile over the whole demand set and measures the same deterministic
fresh-id transition sequence for fan-out and pre-batched input. A live engine
does not guarantee one recompile per demand: two can land in separate compiles,
the coalescer's grouping can differ between them, and a request with no typed
successor is retired directly rather than at an accepted transition edge.

Proving the derived case live requires giving the harness a way to await a
specific plan generation rather than a quiet socket. Until then the polling
helper stays, used only to SEQUENCE a stimulus (so "one more group" arrives
after the first subscription is genuinely live), never to take an assertion.

**It reaches further than the scenarios it was found on, and §6's own feature
is in it.** Measured 2026-07-27 over NINE completed `cargo test -p nmp-bdd
--test bdd` runs on a branch carrying NO library changes: **three red, six
green** — a third of runs, not an occasional blip. Two scenarios carry it:

- `features/queries/reactive-follows.feature`, "Unfollowing one person
  touches only that person's subscriptions" — red twice, on `the
  subscriptions serving Alice and Bob are untouched`.
- `features/routing/relay-subscription-limits.feature`, "A catalog of three
  hundred groups fits inside a limit of twenty" — red once, reporting **2**
  live subscriptions where the end state holds 1.

Same mechanism, same verdict: a transient second subscription exists between
two compiles that did not group identically, the one-shot socket read lands
inside it, and the end state is correct every time. So the flake is a
property of the harness's observation model rather than of any one feature —
**a single red run here is not evidence against the change under test.** Note
what the second scenario means for §6: the subscription-budget feature
asserts a COUNT, which is exactly the quantity this interleaving perturbs, so
it is structurally the most exposed assertion in the suite.

### 8.1d CLOSED (#1211) — the derived read now waits on the engine's own proof

§8.1c ends by saying that proving the derived case live "requires giving the
harness a way to await a specific plan generation rather than a quiet socket."
The await already existed; the harness simply was not reading it. #12 put the
INTERIOR `Derived` atoms into the subtree that a source's
`SourceEvidence::reconciled_through` is computed over, so that watermark is
`None` until the inner demand's own request has settled AND every outer atom
its rows resolved to has itself been requested and settled. That is not a plan
generation counter, and it is better than one: it is a durable per-source
coverage fact, produced by the component that owns coverage, and it is already
delivered to every subscriber on every frame.

`nmp_bdd::world::wire_settled` now waits on that for any watch whose filters
are compiled from rows it must ingest first, and only then waits out the
outbound handoff. `WIRE_QUIET`'s doc no longer claims to bound recompilation;
it bounds the socket, which is all it can see.

What the 400ms window was actually doing was measured before the change, by
dumping the derived `#d` set's successive resolutions on the 300-group
catalog. It grows as a contiguous SUFFIX of the seeded range — `{300}`,
`{290..300}`, `{244..300}`, `{53..300}`, `{1..300}` — because the fixtures
carry strictly increasing `created_at` and the relay replays stored events
newest-first. A wire count taken part-way through therefore misses a
contiguous LOW-numbered prefix, which is exactly the shape #1211 reported
(`group-0001..group-{N}`, N varying 12/47/123/197/206/220/223/233 across
runs). The suite failed 2 runs in 4 locally on `1d2ea5fc` and 4 in 6 on the
issue's `8d51d069`; it does not fail now.

This closes the derived-set half of §8.1c. The COUNT-interleaving residue in
that section is a different mechanism (two compiles that did not group
identically, leaving a transient second subscription) and is untouched here.

### 8.2 CLOSED — the Close/reopen straggler race (#932)

The coverage ruling assumed a Close leaves pending snapshots to be "harmlessly
popped never-attributed." The code instead **discards** the inflight FIFO and
the wire mapping at Close. So: Close at compile N, re-open the same skeleton at
N+1 re-registers the same wire string with a fresh FIFO, and a straggler EOSE
from the pre-Close REQ mints coverage for a request the relay has not finished
serving. Coverage is durable and is what `plan_is_fresh_for` trusts, so this
over-claims acquisition outright — the engine believes it holds data that never
arrived.

Correct layer: the wire string is a **per-connection namespace** owned by
`EngineCore`; the router `SubId` is a **plan identity**. Incarnation freshness
belongs at the engine's wire-string boundary, never in router identity.

§7.2's never-recycle rule closed this for the plan-identity path. It did not
close it everywhere, and the audit below is what established where the residue
actually was.

#### 8.2.1 The audit: every path that registers a wire string

Attribution's `(session, wire string) -> SubId` map is written in exactly one
place, `AttributionState::record_send`, reached either directly or through
`EngineCore::record_observed_request`. Every caller, and whether a later
incarnation could repeat a string a `discard_sub` had already dropped:

**Planned REQ, ordinary recompile** (`apply_wire_delta`). SAFE. The id is the
router's allocated token; the mint counter is monotonic for the `Router`'s
lifetime and the one-diff sweep only ever assigns tokens drawn from the
PREVIOUS plan, so a token that left the plan is never handed out again.

**Planned REQ, replay on connect** (`on_relay_connected`). SAFE, twice over:
the same allocated token, and `AttributionState::clear_session` wipes the whole
session's map immediately before the replay, so no pre-disconnect string
survives to be repeated.

**Planned REQ, replay on the AUTH ready transition** (`finish_auth_ok`). SAFE.
Same allocated token, and it appends to the SAME FIFO rather than a fresh one —
that is the ruling's intersection rule operating normally, not a reincarnation.

**NIP-77 live candidate**, `nip77_role_sub_id(plan, 0x71, filter)`. WAS THE
RESIDUE. Content-derived from a plan token that structural-signature matching
deliberately CARRIES FORWARD across recompiles, so a filter that churns away
and back re-derives an identical string after the first was closed and
discarded. `limit:0` poisons its coverage, so the observable is the handoff
barrier itself: a straggler tripped `activate_live_and_open_neg` for a
candidate the relay never acknowledged.

**NIP-77 NEG session**, `nip77_role_sub_id(plan, 0x72, filter)`. WAS THE
RESIDUE, same mechanism. Unlimited and carrying the demand's real absorbed
keys, so a straggler EOSE on a repeated string mints coverage directly.

**NIP-77 missing-ids backfill**, `nip77_role_sub_id(plan, 0x73, ids)`. WAS THE
RESIDUE, same mechanism. Its own `absorbed` is empty, but its EOSE is what
unlocks the deferred NEG credit, so a straggler released that credit early.

**NIP-77 fallback backlog**, `nip77_role_sub_id(plan, 0x74, filter)`. WAS THE
RESIDUE and the sharpest instance: unlimited (nothing poisons it) and carrying
the demand's real absorbed keys. The falsifier drives exactly this one and
watches durable `RecordCoverage` intervals appear for a request the relay had
not finished serving.

**Negentropy prober**, `SubId::for_wire(relay, probe_filter(), Public,
Public)`. SAFE, and worth stating explicitly because the string is maximally
reproducible — a fixed filter makes it constant per relay. It is never
registered in attribution at all: `Prober::begin_probe` keys it into the
prober's own `pending` map, and no probe ever calls `record_send`. It measures
protocol support and touches no coverage identity.

**`SubId::for_wire` in `core/evidence.rs`**. SAFE. Test fixtures only, inside
`#[cfg(test)]`.

#### 8.2.2 The fix

`nip77_role_sub_id` takes a monotonic incarnation minted by `EngineCore`
(`next_nip77_incarnation`), folded into the digest after the role byte and the
filter hash. Every derivation therefore yields a string nobody has been handed
before, and a straggler for a closed role subscription resolves to nothing —
the derived-namespace counterpart of what §7.2 does for planned subscriptions.

The counter only ever increments. It survives recompiles, `clear_session`, and
reconnects untouched; a counter that reset would re-mint a string a straggler
could still be addressed to, which is the whole defect.

It is folded IN, never appended — see §4.4. The wire string stays at exactly 64
hex characters, and the falsifier asserts that length rather than trusting it.

Why not mint at `record_send`, as the ruling's own example suggested: the plan
path must NOT be incarnated. `Effect::Replay` ships `WireReq`s straight out of
the router's plan and `on_wire_request_handoff` keys on `(session, sub_id)`, so
a blanket mint at send time would break that router-to-engine correspondence.
Only ids the engine both mints and stores itself can carry an incarnation, and
the four NIP-77 roles are exactly those ids.

One consequence to know about: `start_backlog_req` used to displace a prior
backlog by noticing that the newly derived id COLLIDED with a pending entry's
key. Fresh ids make that collision unobservable, so the displacement is now an
explicit plan-scoped sweep — shared with `cancel_nip77_repair_for_plan` rather
than written twice, because a `BacklogActivatesLive` entry owns a nested live
candidate that lives in no other map. A plan carries at most one repair phase
at a time, so the sweep is expected to find nothing; a reachability probe
across the whole engine suite confirmed the old collision branch never fired.

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
- **Sliding debounce or batching below the router.** Still rejected. A sliding
  deadline can starve under a steady arrival stream, and batching resolver
  atoms destroys their coverage identity. The built admission window is
  different: fixed at 10ms from the first uncovered app request, delays only
  unsent wire work, and groups only after routing inside a relay/context/source
  partition. Cache projection is immediate and sent REQs stay immutable.

---

## 9. Evidence index

Everything asserted above is measured, not reasoned. Reproduce with:

| what | where |
|---|---|
| engine-level collapse; warm cache; EOSE independence; multi-relay; 50 values; the tag/author equality | `crates/nmp-engine/tests/core_headless/derived_tag_fanout.rs` |
| fan-out and pre-batched compiling to one plan; the #899 falsifier and its control | `crates/nmp-router/tests/tag_fanout_churn.rs` |
| 300 groups INSIDE a 20-subscription cap; strict improvement over dedup-only | `crates/nmp-router/tests/tag_kill_measurement.rs` |
| widen-only over the full component-shape space with PER-AXIS fire counters; the tag polarity, both ends; the two-tag-name refusal; cap chunking | `crates/nmp-router/tests/coalescing.rs`, `crates/nmp-router/src/coalesce.rs` |
| author-axis limits under `AuthorUnion` (the pre-existing twin) | `crates/nmp-router/tests/kill_measurement.rs` |
| live REQ frames against a real relay | `crates/nmp/examples/tag_fanout_live.rs` |
| intended behaviour, `@wip` | `features/routing/subscription-collapse.feature` |
| §8.2 the Close/reopen straggler, role path: durable coverage minted by a straggler, plus the positive leg | `crates/nmp-engine/tests/core_headless/negentropy.rs` (`a_reopened_backlog_req_never_inherits_a_closed_incarnations_eose`, `a_reopened_live_candidate_never_inherits_a_closed_incarnations_eose`) |
| §8.2 the same race on the plan path, green by §7.2 alone | `crates/nmp-engine/tests/core_headless/live_queries.rs` (`a_reopened_plan_subscription_never_inherits_a_closed_tokens_eose`) |
| §8.2 observed at the relay, in the product's own voice | `features/coverage/reopened-requests.feature` |
| §11 what a widening subscription costs a relay, versus asking only for the new values | `crates/nmp/examples/reserve_cost_live.rs` |

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
7. **Measure the saving before designing the mechanism.** §11's whole verdict
   turned on two numbers — 90% of the waste and 20x the subscriptions — that
   took one afternoon and an example file to obtain, against four design
   problems that had been open for a day.
8. **Fire counters belong per AXIS, not per rule.** Collapsing three rules into
   one made a whole-rule counter *weaker* than what it replaced: the rule can
   fire prolifically on `authors` and never once touch `tags`. A widening
   property over pairs no rule accepts is vacuously green, and that is half of
   why #900 survived.
9. **A chunk count is not a contract.** Where a cap shards a filter, assert the
   provable window and the relay's own ceiling — never `⌈n/cap⌉`, which the
   greedy fixed point does not promise.

---

## 11. What the historical same-id widening cost — MEASURED; #933 NOT BUILT

Whole-demand replanning can make a growing *derived* value set into one
subscription widened in place under the removed identity scheme. That trade
had a price nobody had put a number on: NIP-01 says a REQ carrying an existing
sub-id REPLACES that subscription,
so the relay re-runs the whole query and re-serves every event it already
served. #933 proposed paying it back for derived growth — keep the incumbent,
and open a small second subscription carrying only the newly discovered
values.

Ordinary app observation admission no longer takes that path. The 10ms cohort
groups unsent work, then freezes each admitted REQ; later uncovered app cohorts
append another REQ. #933 remains about value-set growth produced *inside an
already-running derived query*, especially across EOSE/RTT boundaries.

This section is the design pass #933 asked for. The verdict is **do not build
it**, and the reason is not that the saving is small — the saving is large,
up to 90% of served bytes. The reason is that the time floor the issue treats
as its easy problem has no standalone answer, and every variant that survives
scrutiny reaches safety the same way: by making the split into coverage-driven
planned demand, which reverses §4.3 and pays out four more invariants behind
it. A cheaper change collects part of the same saving.

Read §11.6 first if you only want the ruling. §11.1 is the number; §11.4 is
why the number is not enough.

### 11.1 The saving, measured

`crates/nmp/examples/reserve_cost_live.rs` speaks NIP-01 directly to a seeded
`nak serve`, because the subject is the RELAY's cost and NMP's own wire
behaviour is already established by §3.4. Two strategies, same relay, same
seed, same end state:

- `overwrite` — the historical baseline. One sub-id; each growth step REQs the
  cumulative set.
- `delta` — #933. First subscription untouched; each later step opens a
  separate subscription carrying only that step's new values.

Twenty `#p` values, 30 events each, ~475 bytes per event. The GROWTH SCHEDULE
is how many new values each step reveals:

| schedule | overwrite | delta | saved | concurrent subs |
|---|---|---|---|---|
| `9` (one step) | 128 KB | 128 KB | **0.6%** | 1 → 1 |
| `8,1` | 243 KB | 128 KB | 47% | 1 → 2 |
| `5,1,3` (#933's worked example) | 286 KB | 128 KB | **55%** | 1 → 3 |
| `3,3,3` | 257 KB | 128 KB | 50% | 1 → 3 |
| `5,3,3,3,3,3` | 1.02 MB | 285 KB | 72% | 1 → 6 |
| `1`×20 | 2.90 MB | 285 KB | **90%** | 1 → **20** |

Reproduce:

```
nak serve --port 10547
# seed N events per #p value, then:
cargo run -p nmp --example reserve_cost_live -- ws://localhost:10547 5,1,3
```

Four things to read out of that table, and one of them is a caveat about the
table itself.

**The waste is real and it is quadratic in the number of GROWTH STEPS.**
`overwrite` serves `E·(v₁ + v₁₊₂ + …)`; `delta` serves `E·n`. Nothing about
the number of values drives it — a set that resolves in one step wastes
nothing at all, and the same set arriving one value at a time wastes 90%.

**The step count is a derived-demand recompile-granularity artifact, not a
property of the demand — and where a real workload sits in that bracket is NOT
measured here.** §1's app-admission cohort does not govern resolver changes
caused by ingested rows; those still enter the whole-demand invalidation path.
`derived_tag_fanout.rs` case D records that derived resolution is driven by
INGESTED ROWS, not by EOSE, which is what produces the `1`×20 row. But that
case feeds events one at a time into a headless core. The live runtime does
not: `on_relay_frames` accumulates event candidates across a whole inbound
batch and ingests them in ONE `ingest_relay_observations` call, so one
committed mutation and one recompile, over batches of up to
`max_engine_batch: 4_096` frames within `max_engine_batch_wait: 200µs`
(`crates/nmp-transport/src/pool.rs`). A burst of twenty revealing events from
one relay therefore tends toward ONE growth step, not twenty. What defeats
the batch is spacing, not volume: multi-relay staggered discovery, or a
live-tail set that gains a member every few minutes. **`0.6%` and `90%` are
both real; which one a given workload gets is the first thing any revisit of
#933 must measure, and this study does not answer it.**

**The rightmost column is the NEVER-CLOSE variant's bill, and it must not be
charged to the other one.** `reserve_cost_live` opens `delta{step}` per step
and never CLOSEs — deliberately, as the pessimal bound. #933's own wording is
"short-lived". A close-at-EOSE variant whose uncovered values coalesce into
one backfill filter (same shape, unfloored, one-component different — so
`StructuralUnion` merges them) peaks at two or three concurrent
subscriptions, not twenty. The two variants have DIFFERENT bills: the
never-close variant pays in subscriptions, the close-at-EOSE variant pays in
coverage machinery (§11.4). Neither pays both, and an argument that charges
one design for both is attackable.

**Both variants trade against exactly the resource §6 declared scarce.**
#930 spent re-serve bandwidth to buy subscriptions, 300 → 1. #933 spends
subscriptions to buy the bandwidth back. Same trade, opposite directions, and
only one of the two resources has a hard relay ceiling attached to it. Note
the existence proof already running in production, though: `neither_limited`
means every LIMITED query is in per-value-subscription posture today (§11.2),
without incident. The subscription cost is a real cost; it is not, on its
own, prohibitive.

### 11.2 The benefit window is narrower than the table

Two conjuncts must hold before any of the above applies, and both are one
line of code each.

**`limit` must be absent, or nothing widens in the first place.**
`neither_limited` (`crates/nmp-router/src/coalesce.rs:221`) is
`a.limit.is_none() && b.limit.is_none()`. A limited filter never merges, so
its atoms never coalesce, so nothing ever overwrites, so there is no re-serve
to save. **Limited queries are already in delta mode by accident** — one
never-widened REQ per value, paying the subscription cost and none of the
bandwidth cost. #933 is a proposal to move unlimited queries to where limited
queries already sit.

**The relay must not be NIP-77-capable on a Public session**, or the
overwrite the design attacks never reaches the wire. `crates/nmp-engine/src/
core/query.rs:203` is `let broad = filter.limit.is_none();` and the arm below
it diverts every broad Public REQ on a probed relay into `begin_neg_handoff`
— the plan's op is **never pushed to `kept_ops`**. So the whole benefit set is
`unlimited AND mergeable AND (non-Public OR unprobed) AND multi-step growth`.

### 11.3 The four recorded problems, answered

**#1 — where the time floor is stamped. The issue's own probable answer is
UNSAFE, not merely insufficient.** Flooring the merged wire filter at
materialisation cannot work standalone, for a reason that has nothing to do
with where the stamp goes. `CoverageKey` is per narrow atom
(`crates/nmp-store/src/coverage.rs`), so the only floor a MERGED filter may
carry is the minimum proven `through` across its `absorbed` keys. A value
discovered on this compile has no row at all, so that minimum is 0 and the
filter is unfloored — on exactly the compile the feature exists for. Any
floor above it under-fetches the new value's history, which is the widen-only
violation §5.2 calls the one correctness property the module rests on. The
floor is sound only AFTER a split has separated covered values from uncovered
ones. **Problem 1 has no standalone answer: the floor and the split are one
mechanism, and the floor is the dependent half.**

**#2 — one interval per row. Confirmed, and it binds the two TIERS against
each other, not just time-chunked backfill — but it is a satisfiable
constraint, not an unanswered problem.** `merge_interval`
(`crates/nmp-store/src/coverage.rs:122`) treats `incoming.from <=
cur.through + 1` as touching and unions; anything else keeps whichever
interval has the greater `through` and **discards the other outright**. The
issue's conclusion (chunk descending only) is right. What it misses is that
the live tier and the backfill tier land on the SAME key one step later —
once a delta value joins the incumbent filter, a live tier floored at `now`
proves `[now, eose]` against a row holding `[0, delta_eose]`. Disjoint.
Recency wins. The backfill's proof is destroyed.

Two precisions, both of which cut AGAINST treating this as a blocker:

- The destroyed proof re-fires **forever** only if the re-fired backfill is
  `until`-bounded, which pins its `through` below the live row's `from` on
  every retry. An UNBOUNDED re-fire EOSEs at ~now, mints `[0, ~now]`, touches
  the live row and unions — so the loop terminates after one wasted
  full-history refetch. The natural design carries `until` (to avoid
  double-serving the live range), so the scary version is the likely one; but
  "forever" is a property of that choice, not of the storage model.
- The fix falls out of the same paragraph: cap the live tier's floor at
  `min(through) + 1` over its absorbed keys. Then `from <= delta_eose + 1`
  always, so the intervals always touch and always union. The floor can never
  simply be "now", and is only ever as high as the laggiest value in the
  merged filter — a real constraint on the design, cheaply satisfied.

**#3 — backfill squeezed from both sides. Confirmed exactly as written.**
`AttributionState::record_send` snapshots `limited: filter.limit.is_some()`,
and `attribute_eose_detailed` poisons on `fifo.iter().any(|s| s.limited)` —
one limited snapshot voids every key that EOSE could have proven. Unlimited is
the `broad` predicate above. Emitting outside the plan path IS possible; the
four NIP-77 role ids do exactly that. What that costs is stated in §8.2.2 and
in the issue's own closing note: such ids must be engine-minted and
engine-stored, they are invisible to `diag.rs` (which projects `RelayPlan`
only), and they are not replayed by `on_relay_connected`, so a reconnect
mid-backfill loses them silently.

**#4 — on a NIP-77 relay the premise is false, and more so than stated.** The
plan Req is not overwritten on the wire; it is dropped. What actually happens
is a fresh `0x71` live candidate, then `open_neg_session`
(`crates/nmp-engine/src/core/query.rs:813`) stripping `since`/`until`/`limit`
and re-querying the **entire local store** for the shape to seed
`Reconciler::open`. Scoping that reconciliation to the delta values would mean
seeding the reconciler with a deliberately partial view of what we hold, which
inverts negentropy's contract — the seed IS the claim. **The honest answer is
that NIP-77 relays are out of scope and negentropy is the delta mechanism
there.** Suppressing the tiers on probed relays is the only variant that does
not ship two unreconciled loops.

### 11.4 The real cost: every survivable variant converges on the same bill

**A — as specified, a lost backfill is never retried; the only retry path
that exists costs the whole design.** Each current byte-changing successor
still sends the wide UNFLOORED filter, so any history previously missed is
re-served. That accidental self-repair is precisely what the delta design
removes. If a
one-shot backfill is lost — disconnect mid-flight, CLOSE before EOSE, a relay
that never EOSEs — the next compile is zero-diff and no REQ is emitted.
Nothing else picks it up: `decide_handle_acquisition` is explicitly one-shot
("an unsatisfied `MaxAge` becomes `Live` once and stays there"), history's
re-request evidence is its own `acquired_tie_seconds` set rather than
coverage rows, and every `get_coverage` reader in the engine is a gate or a
diagnostic — §4.3's "never during filter construction" is exact.

The engine DOES have a repair pattern, and it is worth naming because it is
the only one available: **anchor retryable work in the plan, and re-derive
ephemeral work from the plan on reconnect.** That is how the NIP-77 role subs
survive a disconnect — not by being replayed themselves, but because
`on_relay_connected` re-derives the whole flow from `router.plan().reqs`.
Borrowing it means making the split a COMPILE INPUT: `compile(demand,
directory, budget, coverage)`, partitioning a merged shape's absorbed keys
into covered and uncovered, planning a floored incumbent over the former and
one coalesced unfloored backfill over the latter. Every leg of the failure
closes — replay resends it, a missing coverage row means the next compile
still plans it, attribution works through the front door unpoisoned, and
`diag.rs` can see it because it is in `RelayPlan`.

That variant is sound. It is also the whole bill:

- it reverses §4.3 — coverage becomes an input to filter construction;
- identical-demand recompiles stop being zero-diff whenever coverage moved,
  which breaks §5.1's `ops on identical recompile: 0` unless floors are
  frozen between value-set changes, which is more state;
- folding a backfilled value back into the incumbent needs a recompile
  trigger on coverage minting — nothing recompiles on EOSE today — or the
  backfill lingers, holding budget, until the next demand mutation;
- `shadow_plan_for` must be fed the same coverage snapshot, or `MaxAge`
  evaluates against a plan shape the live router no longer produces.

**B — and the fold is a 2-diff, so every growth step churns.** This is the
cost neither the issue nor the first pass of this section listed, and it is
the sharpest one. Under §7.2 `Since` and a tag's values are SEPARATE
components (`crates/nmp-router/src/component.rs`). A floored-incumbent design
moves both in the same compile at every growth step: the value set gains the
backfilled value AND the floor advances. That is a two-component move, so
`wire_id::assign` mints a fresh token without a structural predecessor. The
next request opens independently and unmatched prior work closes normally. It
is exactly the **compound churn** §8.1
dismissed as "not a real workload" and forbade designing around — reintroduced
as the steady state of the feature. The replacement REQ is floored, so the
churn is bandwidth-cheap; but it creates a new physical generation on every
growth step and therefore pays the transition cost each time.

**C — a lesser cost: #931 ranks the backfill last.** `refuse_over_budget`
runs per session after coalescing and token assignment, and §6.1's ranking is
that **incumbents outrank newcomers**; a backfill is a newcomer by
construction. Recorded for completeness rather than as a blocker, because it
is narrow and loud: it can only fire on a relay that advertises
`max_subscriptions` AND is already over cap — the state §3.4's collapse made
rare — and when it does fire, the refused keys join `limited`, so
`plan_is_fresh_for` returns false and `ShortfallFact::LocalLimit` reaches the
app. It also has a clean in-router answer, since the split and the budget would
live in the same compile step: when `planned + 1 > allowed`, emit the current
unfloored merged filter for that session as the full-query successor instead
of a separate backfill.

### 11.5 The cheaper change — BUILT for app-admission bursts

The common interactive burst is not derived growth. A render pass asks for a
set of independent live queries -- for example many kind:0 avatar profiles --
one call at a time. Sending each call immediately creates one narrow REQ before
the next call exists. Replanning all live demand after each call groups them,
but repeatedly rewrites already-running subscriptions and, before #1340, also
reread every sibling's canonical rows.

#1340/#1341 split that lifecycle into three explicit facts:

1. Project the new observation from cache immediately and read no sibling.
2. If exact active coverage already exists, attach locally with no compile.
3. Otherwise hold only the unsent relay demand for 10ms from its first
   arrival, route/coalesce that cohort once, and append immutable REQs.

The timer is not a sliding debounce, does not sit in the resolver, and does
not postpone cache delivery. Its grouping locus is the router's existing
per-relay/context/source partition, so two queries are never combined merely
because they happened to arrive together. Cancellation before the deadline
removes the pending demand without producing a REQ.

This collects the app-admission burst regime and removes the quadratic local
projection sweep. It does **not** collect derived values revealed by relay
events after an observation is already running. Those can be spaced by RTTs
or minutes, and an interactive admission window cannot cover them. An
EOSE-anchored mechanism reaches that different regime and remains #933.

### 11.6 Verdict

**#933 stays open, unbuilt, with this analysis.** The saving is real; the
mechanism as specified is not available. What would have to change first, in
order:

- **coverage becomes an input to filter construction**, reversing §4.3 —
  because it is the only way to know which values are already covered, and
  the only way a lost backfill is ever retried (§11.4 A);
- **identical-demand recompiles stop being zero-diff** whenever coverage
  moved, unless floors are frozen between value-set changes — §5.1's `ops on
  identical recompile: 0` is a shipped assertion;
- **something must recompile when coverage is minted**, which nothing does
  today, or a backfill lingers past its own EOSE holding a subscription;
- **compound churn becomes the steady state**, because value-set growth plus
  a floor advance is a 2-diff under §7.2 — the exact workload §8.1 forbade
  designing around (§11.4 B);
- **suppression on probed NIP-77 Public sessions** (§11.3 #4), which is a
  scope decision rather than a mechanism, and is already the accepted answer;
- **a floor discipline proven never to produce a disjoint coverage merge**
  (§11.3 #2) — satisfiable, but it must be proven rather than assumed.

**Two** of those six break invariants that landed this week, and both are
from the identity work: the zero-diff recompile (#899) and the compound-churn
exemption (§7.2/§8.1). The rest are older properties, a mechanism that does
not exist yet, and one scope decision. But the count is not the argument —
the first bullet is. Every variant that survives scrutiny gets there by
making the split coverage-driven planned demand, and then pays the whole
list. That convergence — not the subscription count, not the budget ranking —
is the verdict's foundation.

Two smaller arguments are recorded here but are NOT load-bearing, and a
future revisit should not have to refute them: the 20x subscription bill
(which belongs to the never-close variant only, §11.1), and the budget
ranking (narrow, loud, and answerable in-router, §11.4 C).

The measurement exists so that a revisit argues against numbers. What it does
not yet establish is where a real workload sits between `0.6%` and `90%` —
that bracket is the honest state of knowledge, and closing it is the cheapest
next thing anyone could do here.

*Reviewed adversarially by Fable, 2026-07-27, against an earlier draft that
called §11.4 A permanent and unanswerable. It is not: the plan-replay variant
above is Fable's, and demoting the subscription count and the budget ranking
from blockers to costs is its correction. The verdict survived the review;
three of its supporting arguments did not.*
