# "Where did my query go?" Tracing demand through the compiler

**Status: CURRENT + TARGET.** The current filter-demand compiler is built. The
target descriptor adds source authority and access context to identity, sharing,
and evidence.

After this chapter you can trace a single `observe` call all the way to the wire — through binding resolution, coverage-solving, coalescing, and per-relay REQ emission — and read each stage's output off the diagnostics screen. Invisible-by-design routing becomes falsifiable.

## The pipeline

You hand the engine a live query. Between that call and a REQ landing on a socket, the demand runs through a fixed compiler. Here is the whole thing, once, before we zoom in:

```
observe(filter)
   │
   ▼  resolve bindings           closed Derived graph -> concrete pubkey set
   ▼  active_demand              a BTreeSet<ConcreteFilter> of narrow atoms
   ▼  classify                   each atom: outbox (has authors) | pinned (no authors)
   ▼  build_candidates           per author: write_relays ∪ extras ∪ (indexers if discovery)
   ▼  coverage-solve             greedy 2-relay-min k-cover, capped
   ▼  coalesce                   exact dedup → widen-only merges (AuthorUnion, KindUnion)
   ▼  per-relay REQ              one WireReq per (relay, skeleton), overwriting sub-id
   ▼  deliver                    inbound events fan out to matching handles
   ▼  local re-filter            every delivered event re-checked against the app's filter
```

The compiler is *pure*: `Router::compile(&demand, directory, cap) -> WireDelta`. Same demand plus same relay facts always yields the same wire plan. That determinism is why plans diff cleanly and why the diagnostics screen can show you exactly what the compiler decided.

## Stage 1 — demand: bindings become concrete atoms

The resolver evaluates bindings against the current-pubkey input and canonical
store, producing concrete selection atoms. A NIP-02-derived query with a
caller-selected outer kind might become
`{ kinds:[9999], authors:{<300 hex pubkeys>} }`. This is selection only; the
target semantic descriptor also retains source authority and access context.

## Stage 2 — classify: outbox vs pinned

Each atom is classified by whether it carries an `authors` dimension (`route::classify`):

- **Outbox atom** (has authors): the author set is the routable dimension. The engine erases `authors` to form a **skeleton** — everything about the filter *except* who — and coverage-solves the authors against their mailboxes.
- **Pinned atom** (no authors, e.g. a NIP-29 group timeline): relays come straight from a lane fact (`GroupHost`, `DmInbox`), no solving.

The skeleton matters downstream: two atoms with the same skeleton (identical but for authors) share a subscription id, so adding or removing an author is *one overwriting REQ*, not a close-and-reopen.

## Stage 3 — coverage-solve: 2-relay-min, capped

For each skeleton's authors, `build_candidates` assembles typed candidates.
Protocol-owned contextual authority is a separate target contribution, not an
app relay array.

The solver (`solver::solve`) is a **greedy, deterministic, capped k-cover**. Its contract, from `CoverageInput`:

- `k` — the routing objective. The current default asks for two relays per
  author when candidates and cap permit it.
- `cap` — a **required** global fan-out ceiling. This is **bug-class ledger #4 (uncapped fan-out)** as a type: the relay set is the solver's output, bounded by `cap`, never an accumulated union of everyone's mailboxes. `|selected| <= cap` always.

Set-cover is NP-hard, so the solver is greedy with a lexicographic tiebreak (determinism → reproducible plans → stable diffs), not an optimizer. When it can't reach `k` for an author, it says *why* via a typed `Shortfall`:

- `NoCandidates` — the author lists no relays at all.
- `FewerCandidatesThanK` — the author lists fewer than `k` relays; the ceiling, not a defect.
- `CapExhausted` — the global cap was hit before this author reached `k`.

### Worked example: 2-relay-min under an adversarial mailbox

Two real solver tests bracket the behavior. **Heavy overlap** — three authors who all write to relays 0, 1, 2:

```rust
solve(&CoverageInput { candidates, k: 2, cap: 10, .. });
// selected.len() == 2   — picks a minimal covering pair, not all three
// every author covered by exactly 2 relays, zero shortfall
```

**Disjoint mailboxes** — the adversarial case. Ten authors, each with its own unique pair of relays (twenty distinct relays), a cap of 6:

```rust
solve(&CoverageInput { candidates, k: 2, cap: 6, .. });
// selected.len() == 6           — the cap held; NOT 20
// shortfall is non-empty
// every shortfall.reason == CapExhausted
```

That is the guarantee in action: an adversarial mailbox layout that *wants* to blow your connection budget to twenty relays is clamped to six, and the engine tells you, per author, that the cap — not a bug — is why some authors fell short of two-relay coverage. You read `CapExhausted` and decide (raise the cap, or accept the reduced redundancy); the engine never silently over-fetches to paper over it.

## Stage 4 — coalesce: widen-only, or ship separately

The solver produces one route entry per (author, relay) pair, *ungrouped*. Coalescing folds them into as few wire REQs as possible — but only through merges *proven* to widen. The contract (`coalesce.rs`): `matches(try_merge(a,b)) ⊇ matches(a) ∪ matches(b)` for all events. A merged filter must match *at least* everything its inputs did; never fewer.

Two default rules, both property-tested green:

- **`AuthorUnion`** (load-bearing): atoms identical except `authors` merge into the author union. Trivially widening — more authors matches more events. This is how ten single-author route entries for one relay become one REQ with ten authors.
- **`KindUnion`** (optional): atoms identical except `kinds` merge into the kind union.

A rule whose widening claim is *not* verified is **dropped**, and its two filters ship as separate REQs — graceful degradation, never a silent under-match. The registry surfaces dropped rules by name (`dropped_rules()`), which flows to the diagnostics snapshot's `droppedMergeRules`. If you register a candidate merge whose property test came back red, you *see* it dropped rather than shipping an unsound narrowing.

Exact-canonical dedup runs first and always (the trivially-correct floor): two byte-identical filters become one REQ regardless of rules.

## Stage 5 — per-relay REQ: overwriting sub-ids

The coalesced filters become `WireReq`s, keyed by a `SubId(relay, skeletonHash)`. Because the sub-id is derived from the skeleton (authors erased), author churn re-uses the same sub-id: on the wire that is **one overwriting REQ**, not close+reopen (NIP-01 replaces a sub's filter when the id repeats). `diff_plans` then emits only what actually changed — an untouched relay never even appears in the wire delta; within a relay, all `Close` ops precede all `Req` ops.

## Stage 6 — deliver + local re-filter

Inbound events fan out to every handle whose demand matched, then each delivered event is **re-checked locally against the app's original filter** (`deliver.rs`) before it reaches your `for await`. Coalescing widened the wire filter, so a relay may return events a *different* atom asked for; the local re-filter is what guarantees each subscription still only sees rows that match *its* query. Widen on the wire, narrow at delivery.

## Reading it on the diagnostics screen

Every stage above is legible in the diagnostics stream (see *Diagnostics & debugging*). For each relay:

```swift
for await snap in nmp.observeDiagnostics() {
    for r in snap.relays {
        print(r.relay)
        print("  wire subs:", r.wireSubCount)      // stage 5 output
        print("  authors:", r.authorsServed)        // stage 3 coverage
        for lane in r.byLane { print("  lane", lane.lane, lane.count) }  // stage 2/3
        for f in r.filters { print("  filter:", f) } // EXACT wire JSON, stage 4/5
    }
    print("uncovered authors:", snap.uncoveredAuthorCount)  // stage 3 shortfall
    print("dropped merge rules:", snap.droppedMergeRules)   // stage 4
}
```

`uncoveredAuthorCount > 0` is current shortfall evidence. The target expands
this to graph, wire, relay, result, connection, and AUTH shortfalls while
retaining exact filter JSON and plan revision. Diagnostics explains the plan;
it never upgrades it into a global completeness claim.

## Gaps to know

- The cap is currently enforced per-skeleton rather than globally across a multi-kind query; tighten before that matters (flagged in ledger #4's CI note). For single-skeleton feeds the behavior above is exact.

---

<!-- nav-footer -->
<sub>← [Relays: outbox & indexers](17-relays.md) · [Index](README.md) · [Offline & sync](19-offline-sync.md) →</sub>
