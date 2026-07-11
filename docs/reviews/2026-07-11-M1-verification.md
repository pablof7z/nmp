# M1 verification — independent adversarial review

- **Date:** 2026-07-11
- **Reviewer:** Independent Opus adversarial reviewer (review-only, no code changes)
- **Subject:** M1 grammar-engine spike (`nmp-resolver` / `nmp-grammar` / `nmp-store`)
- **Builder's claim:** all 12 contract tests green; "the M1 kill did not fire — the grammar proved general."
- **Guarding against:** the old-NMP C5 lesson — a contract test that used a synthetic stand-in and looked proven for months while never exercising the real reducer path.

## Test run (reproduced independently)

`cargo test -p nmp-resolver -p nmp-grammar -p nmp-store` → **all green.**
- `nmp-resolver/tests/contract.rs`: 11 passed (tests 1–9, 11, 12).
- `nmp-resolver/tests/no_kind_branches.rs`: 1 passed (test 10).
- `nmp-store`: 13 unit tests passed (supersession/dedup/keying).
- grammar/store lib targets: 0/0 (unit tests live in modules; store's 13 ran).

Total = 12 M1 contract tests, confirmed by count.

---

## Per-item findings

### 1. Real path, not synthetic (the C5 lesson) — **PASS**

The tests are the genuine end-to-end path, not a synthetic stand-in. Trace:

- Events are built with `EventBuilder…sign_with_keys` (`testkit.rs:88-154`) — real signed `nostr::Event`s.
- `Harness::deliver` → `Engine::ingest` (`testkit.rs:55-57`) → `store.insert` (`engine.rs:308`) → outcome gate (`Inserted|Superseded` collected, `Duplicate|Stale` dropped, `engine.rs:309-312`) → dirty-mark via `match_event` (`engine.rs:324-337`) → `run_recompute` → `recompute_node` re-queries the store and diffs (`engine.rs:385-429`) → atom refcount deltas (`ref_atom`/`unref_atom`, `engine.rs:433-453`).
- Assertions compare against **independently constructed** `ConcreteFilter` expectations (`cf_kinds_authors`, `cf_kinds_tag`, `cf_coord` at `contract.rs:24-54`), never against engine-internal state, mocks, or hand-built deltas.
- Test 1: real kind:3 `{A,B,C}` delivered, then real kind:3 `{A,B,D}` (t=101) supersedes via the store's replaceable path; delta asserted `== [Close({1,C}), Open({1,D})]`.
- Test 2 (depth-2 NIP-29): real kind:39002 events superseded; cascade through the Derived node then the outer FilterNode.
- Test 9 (follows-minus-mutes): a real kind:10000 mute event flips the `SetOp(Diff)` result.

No test constructs a `DemandDelta` by hand, pokes the atoms table, or asserts on a mock. `MemoryStore::insert`/`query` (`memory_store.rs:41-95`) is the real door — dedup-first, then newest-wins + lexical-tiebreak supersede — and `query` delegates matching to `nostr::Filter::match_event` (no hand-rolled matching). **The real path is real.**

### 2. No-kind-branch guard (test 10) genuinely bites — **CONCERN (guard weak) / PASS (code clean)**

Two separate questions, split verdicts:

**(a) The code is genuinely kind-blind — PASS, verified by hand.** I read every line of `engine.rs`, `eval.rs`, `graph.rs`, `types.rs` and grepped all `kind` occurrences. Every reference is one of: a `kinds` *data* field copied opaquely from a filter (`graph.rs:57,166,192`, `engine.rs:475`); `event.kind.as_u16()` read as a *value into* an `Element::Coord` for AddressCoord projection (`eval.rs:81`) — a data projection, not a comparison; or the `GraphNodeInfo` label string / doc comments. **There is no branch on an event's kind value anywhere in the resolver.** Event→node routing is exclusively `match_event` against the node's own concrete filter (`engine.rs:331`) plus `Node`-variant structural dispatch (`recompute_node` `engine.rs:390-428`). Depth-1 and depth-2 provably traverse the *same* `run_recompute`/`recompute_node`/`compute_atoms` code — only the descriptor value differs. The kill-guard invariant holds at the code level.

**(b) The scanner itself is defeatable — CONCERN.** `no_kind_branches.rs:10` bans only the literal substrings `["== 3", "== 39002", "kind() =="]` plus lines that *start with* `match` and contain `kind`. I empirically defeated it: injecting
```rust
let k = ev.kind.as_u16();
match k { 3 => true, 39002 => false, _ => false }
```
into `eval.rs` left test 10 **green** (the `match k {` line contains no "kind"; the arms `3 =>` / `39002 =>` match no banned pattern). This is exactly the shape of laundering the M1 kill is supposed to make loud. It is a hollow-*guard*, not a hollow-*green*: the code it guards is currently clean, and any real per-shape special-casing would also have to defeat tests 11/12 (a `match k {3,39002}` engine would not handle kind:10003 or kind:30003). But the structural tripwire is weaker than the plan (§6.1) implies. **Recommend hardening before M2** (scan for `\.kind`/`as_u16()` adjacency to `match`/numeric arms, or assert positively that routing goes through `match_event`).

### 3. Generality (test 12) is a real witness — **PASS**

Test 12 (`bookmarks_filter`, `kinds:[1], #e := Derived(kinds:[10003] → Tag(e))`) passes with zero engine change. It is admittedly a *close cousin* of test 1 (both depth-1 `Derived→Tag`, differing only in tag char and target field-slot), so on its own it is a weakish witness. But generality is much better witnessed in aggregate by structurally *different* projections/compositions that also needed no engine change: test 11 (`AddressCoord` → co-pinned multi-field atoms, a genuinely different `Element` shape and merge path), test 9 (`SetOp(Diff)` composition), and test 2 (`Tag(d)` into a tag slot vs `Tag(p)` into authors). I attempted to construct a **4th shape that would force an engine edit** and could not, within the grammar: multiple independent bound fields already fan out via `compute_atoms`'s cartesian (`graph.rs:189-218`); deeper nesting is handled because `run_recompute` orders purely by depth (`engine.rs:350-378`) and the depth counter is unbounded (test 9 already reaches depth 4); Union/Intersect/Diff all fold generically (`eval.rs:109-138`). The dispatch is entirely structural over closed vocabularies. **Generality is genuine, not two hardcoded reads.**

### 4. Replace-not-rebuild metrics are real — **PASS**

`atoms_opened`/`atoms_closed` are incremented *only* on true 0→1 / 1→0 refcount crossings in `ref_atom`/`unref_atom` (`engine.rs:437-439`, `447-450`), computed from the live `atoms: BTreeMap` — not hardcoded. The diff that feeds them is a real `BTreeSet::difference` of old vs new atom sets (`engine.rs:420-424`), so only symmetric-difference elements churn. A secret rebuild is not possible undetected: if the engine closed-all/opened-all, **both** the exact `delta.ops` assertions (e.g. test 1 `== [Close(C), Open(D)]`) **and** the `atoms_closed==1 && atoms_opened==1` count assertions would fail (they would show `2·|set|`). Tests 1, 2, 9, 11 all assert the exact symmetric-diff counts. Replace-not-rebuild is real and measured.

### 5. Surgical-delta claims — **PASS**

- **Test 1:** zero churn on A,B asserted three ways — exact `delta.ops == [Close(C), Open(D)]`, exact counts `atoms_closed-before==1 && atoms_opened-before==1`, and post-state `demand.contains(A) && contains(B) && contains(inner)` (`contract.rs:226-243`). Solid.
- **Test 3 (re-root):** "every Close index < every Open index" asserted via the `seen_open` scan (`contract.rs:325-331`); reverse-of-open order asserted (`closed().last() == inner`, `contract.rs:341`); "no atom mentions old pubkey" asserted via `!demand_after.any(authors contains a_hex)` (`contract.rs:351-357`). (Minor: the leak check inspects the `authors` field only, adequate for this shape where `A_pk` can appear nowhere else.) Solid.
- **Test 9 (follows-minus-mutes):** muting A yields exactly `delta.ops == [Close({1,A})]` and nothing else, plus `atoms_opened-before==0 && atoms_closed-before==1`, plus B,C untouched (`contract.rs:545-554`). Exactly as required.

### 6. The deviations — **PASS (acceptable for headless M1)**

- **`QueryHandle::Drop` discards its delta:** `Drop` enqueues the id to `pending_drops` (`engine.rs:47-53`); the next mutating call drains it via `drain_pending_drops`, which calls `unsubscribe_inner` and **discards the returned `DemandDelta`** (`let _ = …`, `engine.rs:292`). The atoms table *is* correctly decremented (demand stays consistent), only the *notification* is dropped. Acceptable for M1 (contract tests use explicit `unsubscribe`, which does surface the delta; abstract demand has no wire consumer yet). **It becomes a real hole at M2+**, where a dropped close-delta means a CLOSE never reaches the wire. Flag for M2. Consistent with plan §4.
- **`NodeId=u64` over `HashMap` instead of slotmap:** **no soundness issue.** `alloc_id` is a monotonic counter that never recycles (`graph.rs:97-100`); `remove_node` deletes from the map without reusing the id, so there is no ABA/stale-handle aliasing. The plan's `SlotMap` sketch is satisfied by a counter+HashMap; memory of purged nodes is reclaimed on teardown (`engine.rs:280-285`).
- **No test weakened or over-mocked** beyond items already noted. Metrics counting is honest (`sets_reevaluated` counts every re-eval; `nodes_recomputed` only value-changes; `recompute_passes` bumps once per batch and *before* an empty seed, but early-returns before bumping on all-Duplicate/Stale batches — tests 4/5 rely on and confirm this, `engine.rs:313-316`).

### 7. Gaps where a kill could hide — **PASS with one noted nit**

- The one genuine structural weakness is item 2(b) — the scanner is the *only* automated tripwire for laundered per-shape branching, and it is bypassable. Today the code is clean (verified by hand), so no kill is hiding; but the *automated* defense is thinner than §6 claims.
- `recompute_passes` increments even when a batch's changed events match no Derived node (empty seed still bumps the counter). Not tested, harmless — no kill hides here.
- Test 3's leak assertion checks only the `authors` field, not `tags`, for the old pubkey. Adequate for the shape under test (a Reactive in authors), but a re-root of a shape with `Reactive` in a *tag* position would want the tag field checked too. Not exercised in M1; note for when Reactive-in-tag re-root is tested.

---

## Hollow-green / weakened tests

**One weakened guard, zero hollow-green contract tests.**
- Test 10 (`resolver_has_no_kind_specific_branches`) is a **weak guard** — empirically defeatable by trivial laundering (demonstrated). It is not hollow-green (it does scan real source and would catch the naive `== 3` / `kind() ==` / `match … kind` forms), but it under-delivers on the §6 promise of making per-shape branching "a red build."
- The 11 behavioral contract tests are **honest and non-hollow**: real signed events, real store door, real supersede, real recompute, assertions against independently-built expectations with exact delta-op and exact metric-count checks. I found no test asserting something trivially true, no synthetic stand-in, no mock on the data path.

---

## Verdict: **VERIFIED-WITH-NITS**

M1 is genuinely proved. The crown-jewel claim — a *general* reactive filter-binding grammar with surgical replace-not-rebuild deltas — holds on the real path, and I verified the single most important thing skeptically: the resolver contains **no branch on any event's kind value** (hand-audited across all four source modules and grep-confirmed), so depth-1 and depth-2 demonstrably run the *same* recompute code, differing only in descriptor value; the pre-committed kill ("the code grows the kind:3 case and the 39002 case") did not fire, and it would have been visible in tests 11/12 if it had. The tests exercise the genuine ingest→insert→supersede→re-eval→delta pipeline (retiring the C5 synthetic-stand-in failure mode), and the replace-not-rebuild metrics are computed from real refcount crossings with exact-count assertions that a secret rebuild could not pass. Generality survived my attempt to construct a breaking 4th shape. The nits are real but not bet-invalidating: (1) test 10's structural scanner is bypassable by laundered `match` branches and should be hardened before M2 — today the code is clean, but the *automated* tripwire is weaker than the plan states; (2) `QueryHandle::Drop` discards its withdrawal delta, correct for headless M1 but a must-fix at M2 when demand reaches the wire. Neither touches the M1 thesis. **The green is not hollow. Ship M1; carry the two nits into M2.**
