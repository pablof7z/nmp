# Crash-safe durable `Accepted` + canonical pending row — implementation plan (#2 + #3)

- **Date:** 2026-07-11
- **Status:** Design / implementation plan. No code. Governs GitHub issues
  **#2** (canonical pending-signature row, no app optimistic mirror) and **#3**
  (crash-safe durable `Accepted`). Epic #23's sequencing note treats these as
  **one atomic persistence seam**; this plan designs them together.
- **Contract sources (authoritative, not re-litigated here):** #43 agreed
  contract; #2/#3 required-contract clauses; `docs/design/retraction-and-negative-deltas.md`
  §1 (the one negative-delta lane), §3 (deadline driver), **§4 (store citizen,
  not overlay)**, **§4.2 (compensating re-insert, never un-supersede)**, §5
  (coverage never lowers on retraction); `docs/known-gaps.md` "Promoted v2
  contract gaps" (the crash-safe-`Accepted` and durable-logical-retry bullets).
- **Frame position:** step 3 of #43's recommended order (`#2/#3/#23 land
  crash-safe Accepted and canonical pending rows`), after #44/#52/#40 and
  before #47/#6 signer selection. This plan builds only the **persistence hook**
  for signer selection; the signer model is #47/#6.

---

## 0. The gap, precisely (grounded in current code)

The write path today **never inserts a local row into the store**. `on_publish`
(`crates/nmp-engine/src/core/mod.rs:530`) allocs a `ReceiptId`, emits
`WriteStatus::Accepted`, stores a `PendingWrite` in the **in-memory**
`self.pending: HashMap<ReceiptId, PendingWrite>` (`:269`), and either requests a
signer (`Effect::RequestSign`, `:562`) or goes straight to `on_signed`. `on_signed`
(`:603`) resolves routes and emits `Effect::PublishEvent` per relay (`:649`) — but
issues **zero** `store.insert`/`resolver.ingest`. The event becomes query-visible
**only when a relay echoes it back** through `on_relay_frame` → `ingest_observed`.

Two consequences, exactly the two issues:

- **#2:** every write is invisible to local live queries until a relay round-trip,
  forcing each app to build an optimistic mirror; offline / slow-NIP-46
  composition is invisible.
- **#3:** `PendingWrite` is memory-only (confirmed: no outbox table in
  `redb_store.rs` — tables are EVENTS, ADDR_INDEX, COVERAGE, TOMBSTONES,
  ADDR_TOMBSTONES, EXPIRATION_INDEX, BY_AUTHOR, BY_KIND, `redb_store.rs:41-76`).
  A process death between accept and ack silently loses the composition, its
  receipt, its displaced predecessor, and all delivery work.

The substrate the retraction family already landed (preserve, do not rewrite):
symmetric store door with `InsertOutcome::Superseded { replaced: Box<StoredEvent> }`
already **widened to carry the evicted row** (`nmp-store/src/lib.rs:119`),
`remove(id, RetractReason) -> Option<StoredEvent>` (`:199`), `RetractReason`
(`:163`), `resolver.retract`/`react` one-recompute lane
(`nmp-resolver/src/engine.rs:450,469`), and the `next_deadline`/`recv_timeout`
deadline driver (#39, `core/mod.rs:458`, `runtime/mod.rs:377`). This plan is the
**write-side feeder** into that already-built machinery.

---

## 1. The canonical pending row (#2) — a store citizen, not a tier

Per retraction doc §4.1 the pending row lives **in the one store** as row *data*,
not a second query path or a committed/pending authority split.

### 1.1 Row shape

Extend the two store value types (`nmp-store/src/lib.rs:47,83`):

```
Provenance {
    seen: BTreeMap<RelayUrl, Timestamp>,   // unchanged — relay observations
    local: Option<LocalOrigin>,            // NEW: set iff this row was locally authored
}

LocalOrigin {
    intent_id: IntentId,                   // stable, survives restart (see §2.2)
    signature: SigState,                   // Pending | Signed
    accepted_at: Timestamp,
}

SigState { Pending, Signed }
```

- A relay-observed row has `local: None` (untouched today's behaviour).
- A locally-authored row has `local: Some(..)`. After signing **and** relay echo
  it keeps `local` (still a locally-authored fact) **and** accretes relay
  `seen` — the "sending…" chip resolves off `seen.is_empty()`, the receipt stream
  stays the sole ack authority (retraction doc §4.1). No second field is needed
  to represent "confirmed by relay"; that is just `seen` growing.

**Body representation (design decision, flag as owner-lite — see §7 Q1).**
`StoredEvent.event` is a `nostr::Event`, which carries a `sig`. A NIP-01 id is
`hash([0,pubkey,created_at,kind,tags,content])` — the signature is **not** an
input — so the id is final at acceptance. The recommended representation: the
store row holds a `nostr::Event` whose NIP-01 fields are the frozen body and
whose `sig` is a **sentinel (zeroed)** until promotion. `Filter::match_event`
(the store's only matcher, `lib.rs:8-9`) ignores `sig`, so filtering,
`Derived` re-resolution, and replaceable/addressable supersession all work
unchanged on the pending row. The verify gate (#14) guards the *relay-ingest*
boundary, not the store door, so an engine-authored sentinel-sig row does not
fight it. `nostr::Event` is constructed without verification (`Event::verify`
is explicit); confirm the crate permits building an `Event` with an arbitrary
`sig` field (it does today) — this is the one representation nuance to ratify.

### 1.2 Entering through the ordinary door

Add **one** store-door method that runs the *identical* supersession/tombstone
logic `insert` runs, but stamps local provenance + `SigState::Pending` instead of
a `RelayObserved`:

```
// EventStore trait
fn accept_write(&mut self, accept: AcceptWrite) -> AcceptOutcome;
```

`AcceptOutcome` reuses the widened `Superseded { replaced: Box<StoredEvent> }`
shape so the resolver sorts it exactly like a relay insert. The resolver gains a
thin caller mirroring `ingest_observed` (`engine.rs:416`):

```
// ResolverEngine
pub fn accept_local(&mut self, accept: AcceptWrite) -> DemandDelta
// -> store.accept_write; Inserted/Superseded feed `react(inserted, removed)`
```

The pending row now participates immediately in ordinary filtering, `Derived`
bindings (an optimistic kind:3 edit re-resolves follows because `react` re-queries
the store fresh, `engine.rs:461`), replaceable/addressable winner selection,
deletes, and expiry — through the **existing add path**. `RowDelta::Added` flows
out `refresh_all_handles` with zero new visibility mechanism. **No app optimistic
mirror.**

### 1.3 Dedup against the relay echo

Promotion (§1.4) fills the real signature **before** `Effect::PublishEvent`, so by
the time a relay echoes the event, the stored row already carries the real sig and
the real id. The echo hits `insert`'s dedup-by-id first (`lib.rs:178-179`) →
`Duplicate { provenance_grew }` merges `RelayObserved` into `seen`. No churn, no
second write path — exactly the hand-off retraction doc §4.1 describes.

### 1.4 Promotion in place, zero id churn

Add a door method that swaps the sentinel sig for the real signature and flips
`SigState::Pending → Signed` **on the same row** (same EVENTS/ADDR_INDEX/
BY_AUTHOR/BY_KIND entries — no remove/add):

```
fn promote_signed(&mut self, id: EventId, sig: Signature) -> PromoteOutcome;
```

The signer result must exactly match the frozen body, pubkey, and id and carry a
valid signature (`nostr::Event::verify`) **before** `promote_signed` is called
(engine-side check, §3.3); a mismatch is a terminal protocol failure that retracts
(§3.4), never promotes.

---

## 2. The atomic acceptance boundary (#3)

### 2.1 One commit contains everything

`Accepted(intentId)` is emitted only after **one crash-atomic commit** of the set
#43/#3/known-gaps enumerate:

1. the frozen unsigned NIP-01 body,
2. expected pubkey + pinned signing-identity reference,
3. durability/policy + routing,
4. the canonical `Pending(intentId)` row from §1 (EVENTS + address/author/kind
   indexes),
5. the **displaced predecessor** (if the pending row superseded a replaceable
   winner) — needed for §3 compensation,
6. initial retry/attempt state (empty at acceptance — see §5),
7. receipt state (stable receipt id + `Accepted`).

A crash mid-accept must leave **either nothing or a fully-recoverable `Accepted`**
— restart must never observe the row without its obligation, or the obligation
without its row (retraction doc §4.1; #2 "Atomicity").

### 2.2 Where it lives: outbox tables in the store's redb Database

Atomicity across {pending row, displaced stash, intent journal, retry seed,
receipt} requires them to commit in **one redb `begin_write`/`commit`**. The
pending row lives in the store's EVENTS/ADDR_INDEX tables and each `insert`/`remove`
today opens its own transaction (`redb_store.rs:504,791`). Therefore the outbox
journal **must live in the same redb Database** and `accept_write` must be a
**single transaction that spans the event tables and the new outbox tables**.

New redb tables (all `TableDefinition<&str,&str>`, JSON values, matching the
existing convention `redb_store.rs:41-76`):

- `OUTBOX_INTENTS` — `intent_id → { frozen unsigned body JSON, expected_pubkey,
  signing_identity_ref, durability, routing, sig_state (Pending|Signed|
  AwaitingSigner), receipt_state }`. **Stores the obligation, never a raw secret**
  (#43 "core stores obligations, not raw secrets"; #47).
- `OUTBOX_DISPLACED` — `intent_id → StoredEvent JSON` (the evicted predecessor).
- `OUTBOX_ATTEMPTS` — `(intent_id, relay, ordinal) → { outcome, next_eligible_at }`
  (empty at acceptance; §5).

**Architecture note / boundary flag (owner — see §7 Q2):** this broadens
`nmp-store` from "event store" to "event **and** durable-outbox store". That is
the only placement that satisfies "one atomic persistence boundary". The *reducer*
logic (retry ownership, deadline scheduling, signer orchestration) stays in
`nmp-engine`; the store owns only the atomic persistence + recovery-read doors,
preserving the one-door principle (ledger #1). This is a crate-boundary touch and
per CLAUDE.md must be checked against `docs/architecture/crate-boundaries.md`.

`MemoryStore` implements the same doors atomically in-memory; its
`recover_outbox` returns empty (nothing survives a crash by construction).
Crash-safety is a `RedbStore` property (§7 Q4).

### 2.3 Restart recovery path

On boot, **before** the loop, `engine_loop` (`runtime/mod.rs:364`) calls a new
store door:

```
fn recover_outbox(&self) -> Vec<RecoveredIntent>;
```

and replays each `RecoveredIntent` into a fresh `EngineCore`:

- The pending rows are **already in the store** (committed at accept) — recovery
  does **not** re-insert them; they are live in queries from the first
  subscription.
- Rebuild in-memory `PendingWrite` (`core/mod.rs:222`) from the journal:
  `event_id`, `pending_relays` (from `OUTBOX_ATTEMPTS` non-terminal rows),
  `displaced` (from `OUTBOX_DISPLACED`), routing, durability, receipt id.
- Rebuild `event_to_receipt` (`core/mod.rs:270`).
- **Receipt ids are stable and unique across restart** (persisted in
  `OUTBOX_INTENTS`; `next_receipt` (`core/mod.rs:264`) is seeded past the max
  recovered id). Callers reattach to a receipt after relaunch.
- `sig_state = AwaitingSigner` intents re-emit a signer request path (§4);
  `Pending` (signed-but-not-fully-acked) intents re-arm retry deadlines (§5);
  ambiguous at-most-once attempts reload as `OutcomeUnknown` and are **never
  blindly retried** (#3; seeded here, policy in the retry follow-up §5).

---

## 3. Pre-signature compensation (authoritative correction)

Per retraction doc's top "Promotion correction" and §4.2: **write compensation is
pre-signature only.**

### 3.1 Cancel / terminal signing failure

A new `EngineMsg::CancelWrite(ReceiptId)` and the existing signer-error arm
(`on_signer_completed` Err, `core/mod.rs:582`) both route to one compensation
door that, in **one transaction**:

1. `remove(own_event_id, RetractReason::Rejected)` — frees the address slot,
   writes **no tombstone** (the row was never validly signed/published,
   retraction doc §4.2);
2. re-`insert`s the durable `displaced` predecessor through the **same one door**
   — it wins its address back by ordinary supersession (first-at-address);
3. deletes the `OUTBOX_INTENTS` / `OUTBOX_DISPLACED` / `OUTBOX_ATTEMPTS` rows.

The removed pending row and the restored predecessor both feed
`resolver.retract` + the add path → live queries see `Removed(optimistic)` +
`Added(predecessor)`, and a `Derived` over kind:3 re-resolves (the optimistically
added follow disappears from the feed graph). This is the exact §1 negative-delta
lane running for a fourth feeder — no new concept.

### 3.2 Temporary vs terminal

Temporary signer absence, a disconnected NIP-46 session, or a timeout is **not**
terminal: the intent stays `AwaitingSigner(pubkey)` (durably) and resumes on
reattachment (§4). Only explicit cancellation, explicit signer denial, an
unrecoverable invalid/mutated signer response, or protocol expiry compensate.

### 3.3 After signing: receipt-only

Once `promote_signed` runs, relay ACK/reject/timeout changes **only** the durable
receipt/attempt evidence (`OUTBOX_ATTEMPTS` + the receipt stream) — it never
retracts the signed row or resurrects a predecessor. The existing
`handle_write_ack`/`give_up_pending_writes` (`core/mod.rs:758,796`) already model
per-relay `Acked`/`Rejected`/`GaveUp` on the receipt only; this frame makes those
transitions **write through** to `OUTBOX_ATTEMPTS` (§5).

### 3.4 Chained replaceable edits — ordinary supersession, no state machine

Retraction doc §4.2: each `PendingWrite` stashes what *it* displaced (durable in
`OUTBOX_DISPLACED` keyed by intent). The door arbitrates every unwind — rejecting
the newer edit restores the older pending one (its event is the stash); rejecting
the older one while the newer holds the address is a `remove` no-op and the
re-offered grand-predecessor returns `Stale`. **No LIFO bookkeeping, no state
machine** — door semantics resolve every ordering. This frame adds only: making
`PendingWrite.displaced` durable and reloaded at boot.

---

## 4. `AwaitingSigner` — persistence hook only

At acceptance, if no signer for the expected pubkey is attached, persist the intent
with `sig_state = AwaitingSigner(pubkey)` in `OUTBOX_INTENTS` and emit
`WriteStatus::AwaitingCapability` (the variant already exists,
`outbox/mod.rs:100`). The pending row is inserted and visible regardless.

Reattachment wiring (this frame): `runtime/mod.rs`'s `Cmd::AddSigner`
(`runtime/mod.rs:402`) and boot recovery rescan `OUTBOX_INTENTS` for
`AwaitingSigner(pk)` matching the newly attached signer and emit the ordinary
`Effect::RequestSign` path. The store persists the **obligation + a stable
identity reference**, never a raw bunker/local secret (#43, #47).

**Out of scope here (do not build):** the signer default/override selection model,
provider registry, and platform vaults are #47/#6. This frame builds only the
persistence field + the "reattach triggers RequestSign" hook, so #47/#6 land on a
durable substrate.

---

## 5. Retry state seed — the durable-logical-retry seed only

At acceptance the relay set is unknown (routing happens post-sign, `on_signed`
`core/mod.rs:620`), so the retry seed persisted at acceptance is: the durability
class + an **empty** `OUTBOX_ATTEMPTS`. The first real attempt rows are written at
dispatch (post-sign) and on each per-relay transition:

- Persist `AttemptStarted` (relay, ordinal, `outcome=InFlight`, `next_eligible_at`)
  **before** `Effect::PublishEvent` (#3 "Persist `AttemptStarted` before
  dispatch").
- On `handle_write_ack`/`give_up` (`core/mod.rs:758,796`), write the terminal
  outcome (`Acked`/`Rejected`/`GaveUp`/`OutcomeUnknown`) + `next_eligible_at`.
- Routing is **append-only revisions**: a newly discovered destination adds a new
  `(intent, relay)` lane; it never erases prior evidence (#3).

Plug into the **one deadline scheduler** (#39, `core/mod.rs:458`): fold
`min(next_eligible_at)` over non-terminal `OUTBOX_ATTEMPTS` into `next_deadline()`
alongside NIP-40 expiry and neg-liveness. The `next_deadline` doc already says it
is "extensible to future timers (backoff, drop-grace)" (`core/mod.rs:455`) — this
is that extension.

**Flag (own frame):** the full retry *owner* — logical backoff curve, concurrency
caps, `OutcomeUnknown`/at-most-once policy, transport-vs-outbox retry ownership
boundary (#3 "Retry ownership" clause) — is bigger than the seed. Build here: the
durable attempt table, the write-through on each transition, and the `next_deadline`
fold. **Recommend a follow-up issue** for the backoff/concurrency/OutcomeUnknown
policy engine, referencing #3's retry-ownership clause and known-gaps "durable
logical retry is unbuilt".

---

## 6. Collision-safe decomposition

Dependency order **U1 → U2 → U3 → U4 → U5**. **U3 and U4 are the core-seam
serialization points** — flag to the orchestrator against #52 (public write
surface) and any other `core/mod.rs` work.

### U1 — Store: pending-row shape + local door + promote/retract + durable outbox
**Files:** `nmp-store/src/lib.rs` (extend `Provenance`/`StoredEvent`; add
`accept_write`/`promote_signed`/compensate/`recover_outbox` to `EventStore`),
`nmp-store/src/memory_store.rs`, `nmp-store/src/redb_store.rs` (new OUTBOX_* tables;
single-transaction `accept_write`; in-place `promote_signed`; single-transaction
compensation; `recover_outbox`). **Touches the STORE DOOR** — serialize vs any
concurrent store-door work (#28/#31 landed; watch routing #22). Suggest split:
- **U1a** row shape + `accept_write`/`promote_signed`/compensation on `MemoryStore`
  + the sentinel-sig representation.
- **U1b** the same doors + OUTBOX_* tables + `recover_outbox` on `RedbStore`
  (the crash-atomic transactions).

**Tests:** accept inserts a matching pending row; supersession returns the
displaced predecessor; `promote_signed` keeps id/address entry (zero churn);
compensation removes pending + restores displaced with **no tombstone**;
dedup-by-id merges relay echo into an already-signed local row; **redb
crash-injection**: kill between event-table write and outbox write leaves neither
(single transaction) — assert via a fault-injecting `Database` wrapper /
mid-transaction panic + reopen.

### U2 — Resolver: local add path
**Files:** `nmp-resolver/src/engine.rs` (`accept_local` mirroring `ingest_observed`
`:416`; route `Superseded` into `react(inserted, removed)`). Small. **Depends U1.**
**Tests:** `accept_local` seeds the add path; a superseding local edit both adds
the new row and removes the predecessor through one `react`; `Derived` over kind:3
re-resolves; `Metrics` witness (`atoms_opened+atoms_closed == |symmetric diff|`)
holds.

### U3 — Engine core: rewire the write lifecycle through durable accept
**Files:** `nmp-engine/src/core/mod.rs` — `on_publish` (`:530`) calls
`store.accept_write` and `resolver.accept_local` (emit `Accepted` only after the
commit); `on_signed` (`:603`) calls `promote_signed` **before** `Effect::PublishEvent`
and validates exact body/id/pubkey + `verify`; `on_signer_completed` Err (`:582`)
and a new `on_cancel` route to §3 compensation; `PendingWrite` (`:222`) grows
`displaced` + intent-id linkage; `AwaitingSigner` persistence (§4);
`EngineMsg::CancelWrite` (`:136`). **TOUCHES core/mod.rs SEAM — the primary
serialization point.** **Depends U1, U2.**
**Tests:** ordinary + replaceable pending row visible pre-relay; exact NIP-46
validation (mutated response → terminal, retracts, no promote); cancellation
retracts + restores predecessor; relay rejection **after** signing touches receipt
only; chained pending edits unwind correctly (§3.4); AwaitingSigner persists +
pending row still visible; `resolver_has_no_kind_specific_branches` stays green.

### U4 — Restart recovery + AwaitingSigner reattach + retry deadline fold
**Files:** `nmp-engine/src/core/mod.rs` (recover-from-journal constructor path;
`next_deadline` folds `OUTBOX_ATTEMPTS`; signer-attach rescans AwaitingSigner),
`nmp-engine/src/runtime/mod.rs` (`engine_loop` `:364` boot calls `recover_outbox`
and replays; `Cmd::AddSigner` `:402` triggers rescan). **TOUCHES core/mod.rs +
runtime SEAM.** **Depends U1, U3.**
**Tests (kill/restart falsifiers — load-bearing):** accept offline → restart →
pending row still query-visible + receipt reattachable; signer detach → restart →
reattach → sign → publish → per-relay evidence; exact-byte resend after restart
(frozen body unchanged); route revision append-only across restart; ambiguous
at-most-once reloads `OutcomeUnknown`, never blindly retried; a persisted
`next_eligible_at` fires via the deadline driver with no polling.

### U5 — Headless crash-injection / restart-recovery falsifier suite
**Files:** new `nmp-store/tests/outbox_crash_atomicity.rs`,
`nmp-engine/tests/durable_accepted_restart.rs`. **Depends U1–U4.** Consolidates the
kill/restart proofs #2/#3 require: every transaction boundary, receipt replay,
signer detach/reattach, exact-byte resend, route revision, cancellation, ambiguous
at-most-once handoff, logical backoff without polling; plus retraction doc §5's
coverage falsifier (retract each way → coverage rows bit-identical, `gc` remains
the only lowering path).

### Follow-up (flag, do not build here)
- **Retry-owner policy engine** (backoff curve, concurrency caps, OutcomeUnknown /
  at-most-once policy, transport-vs-outbox boundary) — new issue under #23/#3.
- **FFI/facade cancel + receipt-reattach surface** — coordinate with #52 (public
  write surface); this frame adds the core `EngineMsg::CancelWrite` but the FFI
  projection is #52's serialized surface work.

---

## 7. Owner questions / ambiguities (flagged, not invented around)

1. **Sentinel-sig `nostr::Event` vs store-native pending body.** The ratified
   contract says signature state is *data on the one row* (retraction §4.1). I
   recommend storing a `nostr::Event` with a zeroed `sig` until promotion (keeps
   the single query/supersession path untouched). Confirm this does not violate a
   "stored events are always verifiable" invariant anywhere; if it does, the
   pending body must be a distinct store-native type and every matcher path taught
   about it. **Needs owner nod.**
2. **Outbox journal lives in `nmp-store`'s redb Database.** Required for the
   single-transaction atomicity #3 demands (§2.2). This broadens `nmp-store`'s
   remit and is a crate-boundary touch (CLAUDE.md → check
   `docs/architecture/crate-boundaries.md`). **Needs owner confirmation** that the
   store may own outbox tables rather than a separate durable crate.
3. **`OutcomeUnknown` / at-most-once policy scope.** #3 requires `OutcomeUnknown`
   handling and "never blindly retried". I seed the persisted field + the
   never-retry-on-reload rule here and recommend the *policy engine* as the retry
   follow-up frame (§5). Confirm this split.
4. **`MemoryStore` durability semantics.** A MemoryStore-backed engine cannot be
   crash-safe. Recommend: it implements the same doors atomically in-memory,
   `recover_outbox` returns empty — crash-safety is a `RedbStore`-only guarantee.
   Confirm no contract requires refusing durable writes on a volatile store.
5. **Pinned signing-identity reference shape.** #47 owns the model; at acceptance
   this frame pins `expected_pubkey` + an opaque `signing_identity_ref`
   placeholder. Confirm the placeholder is acceptable until #47 defines the real
   reference (obligation, not secret).
```
