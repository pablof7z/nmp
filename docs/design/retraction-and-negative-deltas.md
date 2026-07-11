# Retraction & negative deltas — the removal family

- **Date:** 2026-07-11
- **Status:** Design with the store/resolver retraction substrate now partially
  landed. Answers the "supersession retraction blindness" item in
  `docs/known-gaps.md` and triage item #6 in
  `docs/reviews/2026-07-11-external-feedback-triage.md`; composes with issues #2 (local
  echo), #3 (durable intents), #6 (async signer / optimistic pipeline), #14 (verify gate).
- **Promotion correction:** write compensation is **pre-signature only**. A
  durable `Accepted` intent atomically creates a canonical pending row; cancel or
  terminal signing failure retracts it and may restore what it displaced. Once
  signing promotes that same row, relay ACK/rejection/timeout changes only the
  durable receipt. It never retracts the signed row or resurrects a predecessor.
- **Scope:** how the engine emits *negative* deltas — an event leaving the replica — for
  four triggers (replaceable supersession, kind:5 deletion, NIP-40 expiry,
  pre-signature intent termination) through ONE lane, with zero new app-facing concepts.

---

## 0. The finding, precisely

The retraction primitive is **half-built**, and the half that exists is the app-facing
half:

- `RowDelta::{Added, Removed}` (`crates/nmp-engine/src/core/mod.rs:104`) is already the
  delivery contract, and `refresh_handle` (~:1380) computes each handle's delta by
  diffing the store's **full current matching set** against `last_rows`. If a row leaves
  the store and a refresh runs, the app receives `Removed(id)` today, through machinery
  it already handles. Nothing new crosses the FFI. **Q4 is answered by construction.**
- The store's one door already removes rows (supersession drops the loser,
  `memory_store.rs:91`), and `refresh_all_handles` runs after every ingest — so a
  superseded event **already retracts correctly from root-level live queries**.

At the time this design was written, everything upstream of that diff was
missing. This is historical problem evidence; §6 records what has since landed:

1. **The dirty-mark is add-only.** `Engine::ingest_observed`
   (`crates/nmp-resolver/src/engine.rs:404`) pushes only the *arriving* event into
   `changed`; the dirty-seed loop (:424–:439) matches `changed` against each `Derived`
   node's inner filter. When the new winner happens to match the same inner filter as
   the old one (a fresh kind:3 replacing an old kind:3 under `kinds:[3], authors:[me]`),
   the `Derived` node recomputes and the shrink falls out — by luck of shape overlap,
   not by design. When the *removed* event matched an inner filter the *new* one does
   not (a kind:39002 member-list update that no longer `#p`-tags me; a kind:5 whose own
   kind matches nothing; an expiry or pre-signature termination where **no event arrives at all**), no
   seed is planted and the derived set keeps the ghost member forever.
2. **The store then never removed except by supersession.** No kind:5 processing, no
   expiration index, no removal door for a terminated pending row (grep-verified: zero
   hits for deletion/expiry in `crates/`).
3. **`InsertOutcome::Superseded { replaced: EventId }` carries only the id** — but the
   dirty-mark needs to `match_event` the removed event against inner filters, and the
   store has already dropped the row by the time the outcome is returned.
4. **Nothing drives `tick()`** in the live runtime (`known-gaps.md` "No time driver").

The design below is one mechanism that closes 1–4 together.

---

## 1. The removal dirty-mark: `StoreCommit` — the door reports both sides

### 1.1 The store door becomes symmetric

Every mutation of the store reports **what entered and what left, as full events**, at
the only moment the leaving row still exists — in the door's hands:

```rust
// nmp-store
pub enum InsertOutcome {
    Inserted,
    Duplicate { provenance_grew: bool },
    /// WIDENED: the evicted row itself, not just its id. The store is
    /// holding it at the moment of eviction; returning it costs nothing
    /// and is the only time it can be returned at all.
    Superseded { replaced: StoredEvent },
    Stale,
    /// NEW: refused at the door — already expired, or tombstoned by a
    /// prior kind:5 (see §2/§3). Never stored, nothing to retract.
    Refused(RefuseReason),
}

/// NEW engine-facing doors, all returning the removed rows:
fn expire_due(&mut self, now: Timestamp) -> Vec<StoredEvent>;
fn next_expiration(&self) -> Option<Timestamp>;
fn remove(&mut self, id: EventId, reason: RetractReason) -> Option<StoredEvent>;
```

`RetractReason { Rejected, Deleted, Expired }` exists so diagnostics can count
retractions per cause and so `remove` is self-documentingly *not* a general delete API.
None of this is app-facing: the app never holds the store; the two-noun surface
(`Handle`/FFI) gains nothing. Ledger #1's mechanism ("no public index/storage setter")
is unchanged — these are engine-internal doors on the same one-door store, and every
one of them still routes through the same supersession/tombstone/expiry logic.

### 1.2 The resolver consumes removals symmetrically

`ingest_observed`'s per-batch outcome becomes a commit record:

```rust
struct StoreCommit {
    inserted: Vec<Event>,   // what now matches queries that didn't before
    removed:  Vec<Event>,   // what no longer matches anything (full events)
}
```

and the dirty-seed loop runs the **identical, shape-generic test** over both vectors:

```rust
// engine.rs — the existing loop, with one added iterator
for derived_id in self.graph.derived_node_ids() {
    if let Some(cf) = self.graph.wide_concrete(d.inner) {
        let nf = cf.to_nostr();
        if inserted.iter().chain(removed.iter())
            .any(|e| nf.match_event(e, MatchEventOptions::new()))
        {
            seed.insert(derived_id);
        }
    }
}
```

That is the whole mechanism. It is symmetric to the add path because the add path was
already built the right way: `recompute_node` for a `Derived` **re-queries the store
fresh** (engine.rs:498) — the store no longer holds the removed event, so the recomputed
`ResolvedSet` shrinks by exactly the retracted members; the parent `FilterNode`'s atom
diff (`old_atoms.difference(&new_atoms)` → `unref_atom` → `DemandOp::Close`) closes
exactly the retracted member's atoms and nothing else. **Replace-not-rebuild holds with
zero new code** — the existing `Metrics` witness (`atoms_opened + atoms_closed ==
|symmetric diff|`) extends unchanged to retraction tests. The `DemandDelta` reaches
`recompile()`, the router diffs the plan, and a surgical `WireOp::Close` goes out for
the retracted member — the reverse of the open path, through the same pipe.

A second entry point covers removals that arrive with **no inbound event** (expiry,
pre-signature termination):

```rust
// nmp-resolver
pub fn retract(&mut self, removed: Vec<Event>) -> DemandDelta   // seeds from `removed` only
```

Internally `ingest_observed` and `retract` share one
`react(inserted, removed) -> DemandDelta`. There is exactly one recompute engine; the
four triggers differ only in who feeds `removed`.

### 1.3 Row delivery: nothing changes

`EngineCore`'s pattern after any commit stays what it already is:
`resolver.react(…)` → `recompile(&mut effects)` → `refresh_all_handles(&mut effects)`.
The refresh diff emits `RowDelta::Removed` for:

- the removed event itself, on any root query it matched, and
- **the cascade**: rows authored by a retracted `Derived` member stop matching any of
  the handle's current atoms (`rows_and_coverage_for` queries by the *current* atom
  set), so they diff out as `Removed` in the same pass — the outer feed sheds an
  unfollowed author's notes with no additional mechanism.

### 1.4 Where each trigger's removal originates

| Trigger | Origin of the `removed` event | Lane |
|---|---|---|
| Replaceable supersession | `InsertOutcome::Superseded { replaced }` — widened variant | ingest commit |
| kind:5 deletion | processed **inside `insert`** (§2): door tombstones + drops targets, returns them | ingest commit |
| NIP-40 expiry | `store.expire_due(now)` from `tick()` (§3) | `resolver.retract` |
| Pre-signature intent termination | engine removes the pending row on cancel, explicit signer denial, unrecoverable signer/protocol failure, or protocol expiry (§4) | `resolver.retract` |

One lane, four feeders. The tripwire from VISION §4 ("a *second* mechanism for
expressing read demand") is respected: retraction is not a mechanism for expressing
anything — it is the demand/row machinery running in reverse.

---

## 2. kind:5 deletion — inside the one door

VISION §3's ownership table already assigns "replaceable/**delete**/expiry on insert
through one door" to the store; this section makes it real.

- **On inserting a kind:5:** for each `e`-tag id (and `a`-tag address) whose target the
  store holds **and whose author equals the kind:5's author** (NIP-09: only the author
  may delete; enforced structurally, not by policy code downstream), drop the row and
  return it in the commit's `removed`. Targets not held still record a tombstone.
- **Tombstones persist** (`deleted: id → deleting-event-id`, and for `a`-tags:
  `(address, created_at_ceiling)`): a relay replaying the deleted event later must be
  `Refused` at the door, or arrival order silently resurrects deleted content. The
  tombstone check runs before storage, after dedup-by-id.
- The kind:5 event itself is stored normally (it is an ordinary event; relays and other
  clients need it re-servable) and is claimable/GC-able like any regular event — the
  *tombstone* is the durable fact, not the kind:5 row.
- **Trust boundary:** deletion honors only verified events — this composes with (and
  further motivates) issue #14's verify-before-ingest gate. A forged kind:5 must never
  reach the door.

Tombstone growth is unbounded in principle; retention policy is the owner decision (§7).

---

## 3. NIP-40 expiry — the deadline driver (D8-compliant)

### 3.1 Store: an expiration index

At insert, an `expiration` tag is parsed once; `(expiry_ts → {id})` goes into a
persistent ordered index (BTree in memory; a redb table for `RedbStore`, so deadlines
survive restart). An event whose expiration is already past is `Refused` at the door —
never stored, nothing to retract. `expire_due(now)` drains all entries `<= now`,
removes the rows, returns them; `next_expiration()` peeks the minimum.

### 3.2 Engine: one deadline set

`EngineCore` gains `next_deadline(&self) -> Option<Timestamp>` = min over:

- `store.next_expiration()` (NIP-40),
- open `neg_sessions`' liveness deadlines (`started_at + 30s` — the sweep `tick()`
  already implements but nothing fires),
- future timers (drop-grace debounce, backoff) join the same set later.

`tick(now)` (existing message, extended): run the neg-liveness sweep (unchanged), then
`let removed = store.expire_due(now); resolver.retract(removed);` → recompile →
refresh. Expired rows flow the same lane as everything else.

### 3.3 Runtime: sleep-until-next-deadline, not a poll loop

The engine loop (`runtime/mod.rs::engine_loop`) currently blocks on `cmd_rx.recv()`.
It becomes:

```rust
loop {
    let cmd = match core.next_deadline() {
        None     => cmd_rx.recv().map_err(...)?,            // no deadlines: sleep forever
        Some(dl) => match cmd_rx.recv_timeout(dl.saturating_sub(wall_now())) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => {              // woke EXACTLY at the deadline
                dispatch(core.handle(EngineMsg::Tick(wall_now())), …);
                continue;                                    // re-arm from the NEW next_deadline
            }
            Err(Disconnected) => break,
        },
    };
    …
}
```

Properties, against D8's letter and spirit:

- **Zero new threads** — the existing engine thread's `recv` grows a timeout.
- **Wakes exactly at the next deadline**, never on a fixed cadence; with no deadlines it
  blocks indefinitely (a light embedder pays nothing).
- **Volume-independent**: wake cost ∝ deadlines due, not events stored; the timeout is
  recomputed from `next_deadline()` on *every* loop iteration, so an ingest that
  introduces an earlier expiration re-arms naturally — the ingest message itself is the
  wakeup. No "interrupt the sleep" machinery.
- A past-due deadline yields a zero timeout → immediate tick. Cold start: the first
  loop iteration reads the persisted index, so events that expired while the process
  was dead retract at boot through the identical path.
- `EngineMsg::Tick` stays a plain message — every expiry behavior remains headlessly
  testable against a synthetic clock, with the runtime driver tested separately (spawn,
  insert an event expiring in 100ms, assert `Removed` arrives with no further input).

This also closes `known-gaps.md`'s "No time driver for liveness/timeout sweeps".

---

## 4. Accepted pending rows & pre-signature compensation — store citizen, not overlay

### 4.1 The verdict: the pending row lives IN the one store

Issue #2/#6 already lean this way; this design confirms it and names why the
alternative is the trap.

**A delivery overlay** (engine shows transient rows over the store, retracts on
pre-signature termination) *is* the second-system store: it would need its own filter matching (which
overlay rows match which query?), its own participation in `Derived` evaluation (an
optimistic kind:3 edit must re-resolve follows or optimism is a lie), its own dedup
against the relay echo, its own provenance, its own GC exemption, its own persistence
for issue #3. Every store responsibility, re-implemented in a shadow tier with
different semantics — the framework-reborn shape Brainstorm's UNDO probe warned about.

**Store-resident** costs almost nothing because the store was already built right:

- The accepted row enters through the ordinary door with local provenance —
  `Provenance` grows a `local: Option<LocalOrigin>` alongside `seen` (issue #2's
  `Local` origin; a row *field*, exactly ledger #5's shape). The canonical row
  also carries typed signature state, conceptually `Pending(intent_id)` or
  `Signed(signature)`. That state is data on the one row, not a second query
  path or committed/pending authority split.
- **`Accepted` is one durable commit.** The frozen unsigned event body, expected
  author, durable intent/receipt state, pending row, and any displaced predecessor
  become durable atomically. A restart may observe all of them or none of them;
  it must never recover an obligation without its row, or a row without its
  obligation.
- **Relay-echo reconciliation remains ordinary dedup**: after signing and publish,
  when a relay echoes the event back,
  `insert` hits dedup-by-id first and merges `RelayObserved` provenance into the local
  row (`Duplicate { provenance_grew: true }`) — the app's "sending…" chip resolves off
  provenance, the receipt stream stays the sole ack authority. An overlay would need
  bespoke code for precisely this hand-off.
- Pre-signature visibility (issue #6, `Accepted`-time) composes: a NIP-01 event id is the
  hash of `[0, pubkey, created_at, kind, tags, content]` — **the signature is not an
  input** — so the row's id is final before the signer answers; signing completes the
  row in place with zero id churn. The verify gate (issue #14) guards the
  *relay-ingest* boundary, not the store door, so an engine-authored unsigned-pending
  row does not fight it. A valid signer response atomically promotes the same
  row to `Signed(signature)`; there is no remove/add churn and no second write
  path.
- GC: pending rows must survive collection — the engine (which already constructs the
  `ClaimSet`) adds a claim per in-flight `PendingWrite`. An engine-composed claim, not
  a store concept.

**Pre-signature termination** (explicit cancellation, signer denial, an
unrecoverable invalid signer response/protocol failure, or protocol expiry) =
`store.remove(event_id, Rejected)` → `resolver.retract(vec![row])` → the same
negative-delta lane as §1. Temporary signer absence, a disconnected NIP-46
session, or a timeout is not terminal for a durable intent: it remains
`AwaitingSigner(pubkey)` and resumes when a matching signer is attached.

**After signing there is no compensating retraction.** Per-relay `Acked`,
`Rejected`, `GaveUp`, and outcome-unknown states are delivery evidence on the
receipt only. The signed event remains a canonical local fact and continues to
participate in every matching query, replaceable/delete/expiry semantics, and
ordinary relay-provenance merge.

### 4.2 Pre-signature resurrection: compensating re-insert, never un-supersede

The sharp corner: an accepted **replaceable edit** supersedes the current
winner at insert; pre-signature termination must bring the predecessor back; the one-door store has
no un-supersede — and must never grow one.

The answer falls out of §1.1's widening: `InsertOutcome::Superseded { replaced:
StoredEvent }` hands the evicted predecessor **back out of the door at the moment of
supersession**. The engine stashes it on the pending write it already tracks:

```rust
struct PendingWrite {
    …existing fields…,
    /// The row this pending insert displaced, if any — held only until
    /// this write is signed or terminates before signing.
    displaced: Option<StoredEvent>,
}
```

- **On signature promotion**: drop the stash. The predecessor lost for real;
  later relay outcomes cannot restore it.
- **On pre-signature termination**: `store.remove(own_event_id, Rejected)` (frees the address slot,
  clears `addr_index`), then re-`insert` the displaced event **through the same one
  door**. It wins its address back by ordinary supersession rules — first-at-address.
  No un-supersede operation ever exists; resurrection is a compensating action replaying
  an event the door itself returned. Both the removal and the re-insert feed the §1
  lane, so live queries see `Removed(optimistic)` + `Added(predecessor)` and a
  `Derived` over kind:3 re-resolves — the follow you optimistically added disappears
  from the feed graph too.
- **Chained pending edits** (edit twice before the first signs): each `PendingWrite` stashes
  what *it* displaced; the door arbitrates every unwind. Rejecting the newer edit
  restores the older pending one (its event is the stash). Rejecting the *older* one
  while the newer still holds the address: `remove(older_id)` is a no-op (that id is no
  longer stored) and the re-offered grand-predecessor comes back `Stale` against the
  newer winner — nothing churns, which is correct. No LIFO bookkeeping, no state
  machine: door semantics resolve every ordering.
- **Restart** (issue #3): `displaced` is part of the same atomic durable
  `Accepted` record as the intent and pending row. Recovery can therefore resume
  signing or compensate without reconstructing an undo history from guesses.
- Tombstone interaction: `remove(…, Rejected)` writes **no tombstone** — the retracted
  pending row was never signed and therefore could not have been validly
  published. Signed rows never enter this compensation branch.

---

## 5. Coverage & watermarks: retraction never lowers

The rule, stated as the invariant it already almost is: **`record_coverage` merges; only
`gc` lowers; retraction touches no coverage row.**

- A watermark records scoped acquisition evidence — "this relay emitted EOSE or
  completed reconciliation for this planned shape through T" — not global
  completeness and not row presence. Supersession, deletion, and expiry are *more*
  knowledge about the window, not less: the local set still equals "everything **valid**
  in the proven window."
- **Why GC shrinks but retraction doesn't** (the distinction that makes this sound):
  GC *forgets* an event the relay still legitimately serves — keeping the watermark
  would let retained source evidence omit rows that exist and are wanted,
  so `gc` shrinks the interval (coverage.rs `shrink_after_eviction`). A
  deleted/expired event is *invalid*, not forgotten — and the door **refuses
  re-admission** (tombstone check / expired-at-insert check), so a hypothetical
  re-fetch of the window converges to the same store state. The watermark's claim
  remains true. `covered_through` does not move.
- Empty rows remain honest evidence: after a kind:5 deletes the only matching
  row, the snapshot may carry `0 rows` plus the retained per-relay watermark.
  The app interprets that evidence; NMP does not promote it to a global
  "complete" or "authoritative empty" claim.
- The local row never interacts with relay coverage at all: coverage is keyed per
  (shape, **relay**) and a `Local`-provenance row was never attributed to any relay's
  proven interval; its retraction is invisible to the planner's `covered_through + 1`
  flooring.
- Falsifier for CI: retract each way (supersede / delete / expire /
  pre-signature terminate), assert
  every coverage row is bit-identical before/after; assert `gc` remains the only
  lowering path by construction (no other caller of the shrink helpers).

---

## 6. LANDED substrate vs promoted remaining work

**Landed substrate (preserve; this promotion does not rewrite it):**

- `RowDelta::Removed` + `refresh_handle`'s full-set diff — the entire app-facing
  retraction contract; ships today, zero FFI change.
- The symmetric store door, kind:5 tombstones, persistent NIP-40 expiration
  index, and removal-returning APIs.
- Resolver `react(inserted, removed)` / `retract` dirty-seed, engine feed-through,
  and the resulting surgical close/`RowDelta::Removed` behavior.
- `tick()` + `EngineMsg::Tick` + the neg-liveness/expiry mechanism (the live
  deadline driver remains separate).
- `PendingWrite` registry, receipt terminals (`Rejected`/`Failed`), dedup-first
  provenance merge (the echo-reconciliation path), `ClaimSet`.

**Remaining under the promoted contract:**

- One atomic durable `Accepted` transaction containing the frozen event body,
  intent/receipt state, canonical `Pending(intent_id)` row, and displaced stash.
- `AwaitingSigner(pubkey)`, signer reattachment, exact signer-response
  validation, and atomic in-place promotion to `Signed(signature)`.
- Pre-signature cancel/terminal-failure compensation only; signed relay outcomes
  remain receipt-only.
- Engine `next_deadline()` + runtime `recv_timeout` deadline driver, extended as
  the single scheduler for retry eligibility rather than per-intent timers.
- Retraction counters in diagnostics for real removal causes; relay rejection is
  explicitly not one of them after signing.
- Headless falsifiers: (a) derived-set retraction where the new winner does NOT match
  the inner filter (the smoking-gun case); (b) kind:5 targeting a `Derived` member +
  tombstoned redelivery; (c) synthetic-clock expiry incl. expired-at-insert refusal and
  boot-time catch-up; (d) pre-signature-cancelled replaceable edit resurrects its
  predecessor while a signed-but-relay-rejected edit does not, incl. chained-edit
  orderings; (e) coverage evidence bit-identical across all four; (f) metrics
  witness: only the retracted member's atoms churn.

---

## 7. Biggest risk & the owner decision

**Biggest risk — tombstone retention.** Deletion correctness *requires* the door to
refuse redelivered deleted events, which requires remembering deletions; remembered
deletions grow without bound over a long-lived replica. GC-ing tombstones re-opens a
resurrection window (a relay replaying a deleted event after its tombstone was
collected re-admits it). This is inherent Nostr tension, not introduced complexity —
but it needs a policy.

**Owner decision (RESOLVED 2026-07-11):** kind:5 policy at the door — (a) deletion is
honored default-on, author-only, enforced inside `insert` (ledger-#1's "delete on insert
through one door" made real), and (b) **tombstone retention is PERMANENT** — tombstones
are ~40 bytes/deletion and deletions are rare, so no GC/resurrection window is
introduced; the door refuses redelivered deleted events for the life of the replica.
Revisit only if a real long-lived replica proves the growth matters (at which point the
GC-bounded variant with a documented resurrection window is the fallback, not the
default). Build accordingly: tombstones are a permanent durable table, not GC-claimed.

Everything else in this design is derivable from settled principles (one door,
replace-not-rebuild, D8, two nouns, ledger #5/#7) and carries no new app-facing
surface: the app's entire experience of this family is `RowDelta::Removed` — a variant
it already handles.
