# Fable ruling — coverage-watermark keying & attribution under coalescing

- **Date:** 2026-07-11
- **Status:** RULING for M3 build-step B (and a one-field M2 addition). Resolves M3 plan §8 underspecified #1 and #3. Provisional-until-v2 like everything else, but the builder implements THIS without re-deciding.
- **Context read:** M3 plan §3.1/§3.4/§8; VISION P5/P6/§7 ledger #7; M2 plan §2.5/§4.1; `nmp-router/src/{coalesce,plan,deliver}.rs`; harvest doctrine in old-repo `nmp-store/src/types/coverage.rs` + `nmp-core/src/kernel/coverage_ledger.rs`.

## 0. The ruling in one paragraph

Coverage is **keyed by the NARROW atom** (window-erased), **never by the wide wire filter**, and the wide→narrow derivation is discharged **at send time by construction**, not at read time by filter-subset inference. When the router materializes a wire filter it already knows exactly which narrow atoms it absorbed (widen-only guarantees `wide ⊇ atom` for each); that set is **snapshotted onto the outgoing REQ**. An EOSE/NEG-DONE is attributed against the **send-time snapshot of the specific REQ it terminates** — never against the sub's *current* filter — and writes one coverage row per absorbed atom. Runtime subset-testing of filters is banned: it is exactly the unproven-lattice reasoning P5 demoted, and it is where "wide W credited to an atom W no longer supersets" sneaks in.

## 1. The key + the row

```rust
/// The coverage identity of a narrow demand atom: its ConcreteFilter with
/// since/until/limit ERASED, canonically hashed (same FNV path as
/// DescriptorHash). The time window lives in the row's interval, never in
/// the key — otherwise a floored refetch (since = T+1) hashes differently
/// and can never find its own row.
pub struct CoverageKey(DescriptorHash);   // = shape_hash(atom)

impl ConcreteFilter {
    /// authors/kinds/ids/tags kept; since/until/limit cleared; then hash().
    pub fn coverage_key(&self) -> CoverageKey;
}

/// One proven, retained interval per (atom-shape, relay).
pub struct CoverageRow {
    pub key: CoverageKey,
    pub relay: RelayUrl,
    pub covered_from: Timestamp,     // 0 in the plain unfloored case
    pub covered_through: Timestamp,  // the watermark
}
```

- `record_coverage(key, relay, proven: CoverageInterval)` — the store merges (§3); it has **no public lowering path**. Lowering happens only inside `gc()` (§5).
- `get_coverage(key, relay) -> Option<CoverageInterval>` — `None` = refuse the floor (harvest rule, unchanged).
- The wide wire-filter hash appears **nowhere in the durable store**. Wide filters are transient routing artifacts; a wide-keyed row is orphaned by the first re-coalesce and can only be read back via subset tests (banned above).

**Deviation from harvest, justified (import gate):** the old repo's row was downward-closed `[0, T]` (single `covered_through`). I add `covered_from` because two things `[0,T]` cannot represent honestly:
1. **GC-split.** LRU evicts OLD events. Evicting `e` from a proven `[0,T]` leaves `[0, e.ts−1] ∪ [e.ts+1, T]`; downward-closed must keep the OLD side and discard proof of the entire recent range `[e.ts+1, T]` — the range live queries actually use. A dormant shape's watermark ratchets toward 0 while the store still holds everything recent.
2. **Windowed/paginated queries** (app `since:X`; M4 collections `loadMore` widens `since` downward per VISION §10). Their steady state is "covered `[X, now]`, not below" — unrepresentable downward-closed, which would force earning coverage only by over-fetching from 0.

One interval per row, not a set of intervals (that would be introduced complexity). Disjoint-interval conflicts resolve by **recency wins** (§3); the discarded proof costs bandwidth, never correctness — the P5 degradation shape.

## 2. Attribution — send-time snapshots, and the overwrite race

**The M2 surface change (one field).** `WireReq` gains:

```rust
pub struct WireReq {
    pub sub_id: SubId,
    pub filter: ConcreteFilter,
    pub provenance: Vec<RouteProvenance>,
    pub absorbed: BTreeSet<CoverageKey>,   // NEW: every narrow atom this wire filter supersets
}
```

Populated at materialization (M2 pipeline §4.1 step 3: each per-author atom contributes its `coverage_key()`; pinned atoms contribute theirs) and **concatenated through every merge** exactly as `coalesce_with` already threads provenance (`nmp-router/src/coalesce.rs::coalesce_with`). Because AuthorUnion/KindUnion are property-tested widening, `wide ⊇ atom` holds for every key in `absorbed` **by construction at the moment of materialization** — this is the containment rule, discharged once, at build time. Note `KindUnion` can merge across skeletons, so `absorbed` must be carried on the `WireReq`, not re-derived from the sub-id's skeleton.

**The snapshot.** When the engine sends a REQ (or opens a NEG session), `EngineCore` records:

```rust
struct AttributionSnapshot {
    absorbed: BTreeSet<CoverageKey>,  // from the WireReq as sent
    floor:    Option<Timestamp>,      // the wire filter's since, as sent
    until:    Option<Timestamp>,      // the wire filter's until, as sent
    limited:  bool,                   // wire filter had `limit`
}
```

kept in a **FIFO per (relay-generation, sub_id)**, in `EngineCore` state (in-flight wire bookkeeping — never persisted; on restart there are no outstanding REQs).

**The rule that prevents the silent regression (the load-bearing sentence):** an EOSE on sub `s` is attributed to the **intersection over all currently outstanding snapshots on `s`**, never to the sub's current plan:

- attributed atoms = `∩ snapshot.absorbed` over outstanding snapshots;
- proven interval = `[max floors, min(eose_time, min untils)]` (the interval every candidate REQ proves, whichever this EOSE terminates);
- poisoned (record NOTHING) if **any** outstanding snapshot has `limited = true`;
- then pop the oldest snapshot.

Why intersection: `SubId = (relay, skeleton)` is deliberately stable across author churn — an overwriting REQ reuses the sub-id (M2 `plan.rs`), and a relay that receives REQ(W1) then REQ(W2) may EOSE once or twice, in an order the engine cannot distinguish. Attributing to the *current* filter credits atoms only in W2 with an EOSE that may prove only W1 — exactly the "wide W credited to an atom it does not superset" failure. The intersection is sound regardless of which REQ the EOSE belongs to; atoms in the churn margin (removed C, added D) simply wait for the next EOSE. Under-crediting is always safe (refused floor → refetch → bandwidth). In the overwhelmingly common case there is one outstanding snapshot and the intersection is the whole snapshot.

**Fail-safes (all mandatory):**
- EOSE on a sub with an **empty** snapshot FIFO records **nothing** (never reconstruct from the current plan).
- Disconnect / pool generation bump **clears** that relay's snapshots; the pool translator already drops old-generation frames (M3 §3.2), so cross-generation attribution is structurally impossible — a replayed sub on the new generation gets fresh snapshots.
- NEG sessions carry exactly one snapshot (open→done, no overwrite semantics); NEG runs unfloored/unlimited (harvest Stage A), so NEG-DONE proves `[0, done_time]` for every absorbed atom.

## 3. The window rules (since / until / limit)

An EOSE terminating a REQ sent with `since = F` (absent ⇒ 0), `until = U` (absent ⇒ ∞), no limit, proves the interval **`P = [F, min(t_eose, U)]`** for every absorbed atom at that relay. `t_eose` is the engine clock at EOSE receipt; no advancement during the live phase after EOSE (an open sub delivering events is presence, not proven coverage — live-tail advancement is a permitted post-M3 refinement, not M3).

Store merge on `record_coverage(key, relay, P)` against existing `[from, through]`:
- **No row** → insert `P`.
- **Overlapping or adjacent** (`P.start ≤ through + 1` and `P.end ≥ from − 1`) → union (extend either end). The planner floors REQs at `covered_through + 1`, so contiguous upward extension is THE common path; a backfill REQ with `until` reaching down to `from − 1` extends downward.
- **Disjoint** → keep the interval with the greater `through` (recency wins), discard the other. Bandwidth, never correctness.
- The merge is the only writer; outside `gc()` a row's proven span never shrinks.

**`limit` POISONS coverage — unconditionally in M3.** A limited REQ's EOSE proves the relay stopped, not that it exhausted the window; it records **nothing** for any absorbed atom. (Permitted future refinement, NOT M3: if the engine counted events returned for that specific REQ and `count < limit`, the limit never bound and the EOSE is honest — do not build this now; the per-REQ counting interacts with the overwrite race.) Consequence to state plainly: a `limit:500` initial fetch never earns a watermark; coverage is earned by negentropy (unfloored, unlimited) or by unlimited/window-walked REQs. That is the intended shape of "negentropy-first."

**What the watermark does NOT claim (doctrine, one paragraph):** a row asserts completeness w.r.t. the relay's holdings *as of the completed sync*. A relay can later acquire an event with old `created_at` (late propagation, rebroadcast); a floored live sub will still deliver it (the filter matches; the floor only bounds the stored-events dump), but a relay that acquired it while we were offline is a genuine residual hole. Periodic unfloored negentropy reconciliation is the repair lane. This is inherent to any watermark scheme and was already true in the harvest; do not attempt to "fix" it in the REQ path.

## 4. Re-coalescing churn

**Coverage is a property of `(atom-shape, relay)` earned at a point in time; it does not reference, and is never revalidated against, any wide filter.** When demand changes and atoms re-coalesce into a different wide filter W2:

- Rows written from W1's EOSE **survive untouched** — they were attributed to atom keys via W1's send-time snapshot, and the fact "relay R had returned everything matching atom `a` in `[from, through]`" does not decay because we later ask R a differently-shaped question.
- An EOSE for old-W1 arriving *after* the re-coalesce still counts for exactly W1's snapshot atoms (through the intersection rule if W2 overwrote the same sub-id, in full if W1's sub was closed — a Close does not clear pending snapshots on a still-connected generation; the relay still EOSEs what it was serving, though most relays won't after CLOSE, in which case the stale snapshot is harmlessly popped never-attributed on generation bump).
- An atom that leaves the demand set and returns later finds its old row by the same `CoverageKey` — coverage is durable truth, and that is the whole point (`watermark_cold_start_offline`, M3 test 9).

## 5. GC ordering (M3 §8 underspecified #3)

- When `gc()` evicts event `e`, then for **every** coverage row whose shape matches `e` (window-erased match, all relays — conservative; provenance-scoped narrowing is a permitted later optimization) with `e.created_at ∈ [from, through]`: shrink to **`[e.created_at + 1, through]`** (keep the upper side — LRU evicts old, claims protect recent, recency wins consistently with §3). If the result is empty, delete the row.
- **Same store transaction as the delete.** Never delete in one txn and lower in another: a crash between them leaves a row claiming coverage of a range the store no longer holds — a direct ledger-#7 breach ("authoritative empty" that is actually a hole). Lower-with-delete atomically; if the backend cannot do both atomically, lower FIRST, delete second (an over-lowered row is bandwidth; an over-claiming row is corruption).
- `record_coverage` stays advance/merge-only; the shrink lives privately inside `gc()`.
- Because `ClaimSet ⊇` live demand, GC cannot evict events matching a live atom's shape, so live queries' watermarks never lower under GC in practice; the shrink path fires for dormant shapes. Claim matching must be **window-erased** too (a live query with `since:X` still claims its shape's older events for coverage-integrity purposes) — if the owner later wants GC to reclaim pre-window history of live shapes, the shrink rule above already keeps the row honest, at the cost of the query's `from` rising above `X` → `Unknown` (§6). Ship window-erased claims in M3.

## 6. Per-query aggregation — when is a query `CompleteUpTo(T)`?

A query's resolved demand is a set of atoms; each atom's **covering set** is its *current* solver assignment (outbox: the relays whose current wire REQs absorb it, i.e. `RouteProvenance.covers_authors`; pinned: the pinned relays).

- **Atom at relay** is proven for the query's window `W_q = [q.since or 0, q.until or now]` iff a row exists with `from ≤ W_q.start`; its contribution is `through`.
- **Atom** is covered iff **EVERY relay in its current assignment** is proven. Atom watermark = `min(through)` over the assignment. An empty assignment (`NoCandidates` shortfall) ⇒ `Unknown`.
- **Query** = `CompleteUpTo(min over atoms of atom watermark)` iff every atom is covered; otherwise **`Unknown`**.

**Unanimity, not 1-of-k — ruled, with reasoning:** the 2-relay minimum exists precisely because one relay can be missing events another holds. Calling an atom covered on 1-of-2 asserts authoritative-empty while the second relay — possibly the only holder — hasn't finished. That is ledger #7 regressed in spirit ("one relay's emptiness = emptiness"). `Unknown` under a lagging relay is honest and costs nothing but humility; the diagnostic plane shows exactly which `(atom, relay)` pair is holding the query back.

Two consequences to state so the builder doesn't "fix" them:
- A query can go `CompleteUpTo` → `Unknown` when re-routing adds a fresh relay to an atom's assignment (new relay, no row). Correct — the new relay's holdings ARE unknown. Monotonic presentation is the app's concern.
- One unroutable author holds a whole feed at `Unknown`. Correct at this layer; see owner flag #1.

## 7. Plan-sketch amendments this ruling implies

- **M2 `WireReq`** gains `absorbed: BTreeSet<CoverageKey>` (§2); threaded through `coalesce_with` alongside provenance.
- **M3 §3.1** `CoverageRow` gains `covered_from`; `record_coverage`/`get_coverage` take/return the interval. `MemoryStore` stays the lockstep oracle.
- **M3 §3.4** `Effect::RecordCoverage(CoverageKey, RelayUrl, CoverageInterval)` — emitted **once per absorbed atom** per attributed completion.
- **M3 §5 test 2** ("exactly one `RecordCoverage`") amends to "exactly one per atom absorbed by the EOSE'd wire filter, per the send-time snapshot"; add its falsifier twin: *overwrite the sub's filter (add author D), deliver an EOSE while both snapshots are outstanding, assert D is NOT credited* (the intersection rule, tested).
- **M3 test 9** unchanged in meaning: plain unfloored subscribe ⇒ `from = 0`, `CompleteUpTo(covered_through)` reads exactly as written.
- **M3 test 13** gains the assertion that the shrink and the delete are observed atomically (reopen-after-simulated-crash sees either both or neither).

## 8. Worked example

Query Q = `{kinds:[1], authors := Derived(...)}}` resolving to one atom `a = {kinds:[1], authors:{A}}`, `key = h_a`. Solver assigns `a` to `{R1, R2}`.

1. On R1, `a` coalesces with B's and C's atoms → `W1 = {kinds:[1], authors:{A,B,C}}`, sub `s1`, sent unfloored, no limit. Snapshot(s1) = `{h_a, h_b, h_c}, floor:0, until:∞`. On R2, `W2 = {kinds:[1], authors:{A,D}}`, snapshot `{h_a, h_d}`.
2. `EOSE(s1)` at `t1` → rows `(h_a,R1,[0,t1])`, `(h_b,R1,[0,t1])`, `(h_c,R1,[0,t1])`. Q is still **`Unknown`** — R2 (which may uniquely hold an A-event) hasn't finished.
3. `EOSE(s2)` at `t2` → `(h_a,R2,[0,t2])`. Both covering relays proven → **Q = `CompleteUpTo(min(t1,t2)) = CompleteUpTo(t1)`**. An empty tail beyond `t1` is now authoritative-empty for `[0,t1]`.
4. Churn: the follow list adds E, routed to R1 → overwriting REQ on the SAME `s1` with `W1' = {A,B,C,E}`, floored at `since = t1+1` (all of A,B,C contiguously extendable; E has no row so the coalesced wire floor is the min ⇒ unfloored — snapshot records the floor actually sent). Snapshot FIFO on s1 = `[old {h_a,h_b,h_c}, new {h_a,h_b,h_c,h_e}]`. If a straggler EOSE (for old W1) arrives now, it credits `∩ = {h_a,h_b,h_c}` — **E is not credited by an EOSE that may not have asked for E**. The next EOSE credits the full new snapshot.
5. Restart offline: `get_coverage(h_a, R1/R2)` rows persisted → subscribing Q again yields `CompleteUpTo(t1)` with zero network — and a different shape G with no rows yields `Unknown`, never `Complete` (M3 test 9).
6. GC later evicts a kind:1 A-event with `created_at = t0 < t1` (shape dormant, unclaimed): both `h_a` rows shrink to `[t0+1, t1/t2]` in the same txn as the delete. A future Q (unwindowed, `W_q.start = 0 > from`? no — `from = t0+1 > 0`) reads `Unknown` until a backfill or NEG re-proves the bottom; a future windowed Q with `since ≥ t0+1` still reads complete.

## 9. Owner flags (decisions above my pay grade — none block the M3 builder)

1. **Shortfall vs query-level completeness (surface policy, decide by M4).** Ruled here: strict — any atom with an unroutable/unproven covering relay makes the whole query `Unknown`, with per-(atom,relay) coverage + shortfall visible in diagnostics. The owner may want a softer surfaced gradation (e.g. `CompleteModuloShortfall` / k-of-n) on the M4 result contract. That is a *presentation-of-truth* choice for the SDK surface, not a store/attribution question — nothing in this ruling changes if it lands.
2. **Recorded deviation from harvest doctrine:** interval `[covered_from, covered_through]` replaces downward-closed `[0, covered_through]` (§1 justification: GC-split honesty + M4 pagination). If the owner rejects the extra field, the fallback is harvest-exact downward-closed PLUS the rule "GC eviction inside the proven range lowers `covered_through` below the evicted event" — sound but discards recent proof and walls off pagination coverage; I recommend against it.
3. Non-blocking future refinements deliberately excluded from M3 (noted so nobody gold-plates): live-tail watermark advancement on healthy open subs; `count < limit` un-poisoning; provenance-scoped GC lowering.
