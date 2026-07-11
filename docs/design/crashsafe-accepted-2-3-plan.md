# Crash-safe durable `Accepted` + canonical pending row — implementation plan (#2 + #3)

- **Date:** 2026-07-11
- **Status:** Active implementation plan. U1 store doors landed in #58, U2
  resolver integration landed in #74, and U3 engine lifecycle merged in #78.
  U4 restart recovery/reattachment is the current unit. Governs GitHub issues
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

## 0. The pre-U3 gap, precisely (historical grounding)

The description below records the state U3 replaced. As of #78, local durable
acceptance, pending-row visibility, signing/promotion, and compensation are
built; U4 owns restart recovery and reattachment.

Before U3, the write path **never inserted a local row into the store**. `on_publish`
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
// Engine<S: EventStore>  (U2, landed #74)
pub fn accept_local(&mut self, accept: AcceptWrite)
    -> Result<(AcceptOutcome, DemandDelta), PersistenceError>
// One resolver-OWNED store.accept_write(accept)? call; match the returned
// outcome BY REFERENCE to derive react inputs (never consume/reconstruct):
//   Inserted | Superseded | Kind5Processed -> row.event into `inserted`
//     (Superseded's `replaced` and Kind5Processed's `hidden` into `removed`)
//   Duplicate | Stale | Refused -> empty delta
// then return `Ok((outcome, self.react(inserted, removed)))`.
```

**Contract note (ratified post-U1, supersedes the `-> DemandDelta` sketch
above; this is the shape landed in #74).** `accept_local` OWNS the single
`accept_write` call and returns the outcome UNCHANGED alongside the delta.
U3 must NOT call `store.accept_write` and `resolver.accept_local` separately
— that is **double acceptance** (two transactions, two allocated
`intent_id`/`receipt_id` pairs). U3 calls `resolver.accept_local` ONCE and
reads the store-allocated ids off the returned outcome via
`AcceptOutcome::journaled_intent_id()` / `journaled_receipt_id()` to journal
its `PendingWrite` and emit `Accepted`. A door-level `PersistenceError`
propagates as `Err` with the resolver graph untouched (`accept_write` is
atomic — nothing committed on `Err`).

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

### U2 — Resolver: local add path — **LANDED (#74)**
**Files:** `nmp-resolver/src/engine.rs` (`accept_local` mirroring `ingest_observed`
`:416`; owns ONE `store.accept_write(accept)?` call and routes `Inserted`/
`Superseded`/`Kind5Processed` into `react(inserted, removed)` — see §1.2's ratified
signature: returns `Result<(AcceptOutcome, DemandDelta), PersistenceError>`, the
outcome unchanged, so U3 makes a SINGLE call and reads the ids off it — plus
`testkit.rs`'s `Harness::accept`/`accept_write_of` fixtures and
`tests/local_write_u2.rs`). Small. **Depends U1.**
**Tests (all in `tests/local_write_u2.rs`):** `accept_local` seeds the add path
(`Inserted`); a superseding local edit adds the new row AND removes the predecessor
through one `react` (`Superseded`); an older edit is `Stale` and an identical body is
`Duplicate` (empty delta each); a local kind:5 is `Kind5Processed` — the pending
deletion row enters `inserted` (opens a deletions-by-`e`-tag `Derived`) while the
newly-hidden target enters `removed` (closes the follow atoms) in the SAME `react`;
`Derived` over kind:3 re-resolves; `Metrics` witness (`atoms_opened+atoms_closed ==
|symmetric diff|`) holds on every case.

### U3 — Engine core: rewire the write lifecycle through durable accept
**Files:** `nmp-engine/src/core/mod.rs` — `on_publish` (`:530`) makes ONE
`resolver.accept_local(accept)?` call (which internally owns the single
`store.accept_write` — NOT a separate `store.accept_write` + `resolver.accept_local`
pair, which would double-accept) and reads the store-allocated ids off the returned
`AcceptOutcome` (`journaled_intent_id()`/`journaled_receipt_id()`), emitting
`Accepted` only after the commit; `on_signed` (`:603`) calls `promote_signed` **before** `Effect::PublishEvent`
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

---

## Fable checkpoint (verdict)

- **Date:** 2026-07-11. Reviewer: Fable (delegated design checkpoint; decisions
  below are final calls, not questions back to the owner).
- **Verdict: GO, with required changes** (listed below). No load-bearing flaw
  requiring redesign. All code citations in §0–§6 were re-verified against the
  working tree (`on_publish`/`on_signed`/`PendingWrite` shapes, redb table set
  and per-call `begin_write`, `next_deadline`/`recv_timeout` driver, widened
  `Superseded { replaced: Box<StoredEvent> }`, `remove`/`RetractReason`) — the
  plan is honestly grounded.

### The five decisions

**Q1 — Sentinel/zeroed-sig `nostr::Event` as the pending body: APPROVED.**
Grep-verified there is **no** "every stored event is signature-verifiable"
invariant anywhere: `nmp-store`'s own module doc scopes signature verification
*out* of the crate (lib.rs:25-27, "Explicitly out of scope … signature
verification"); the only verify gates in the workspace are the transport pool's
relay-ingest gate (`nmp-transport/src/pool/verify.rs` — never sees engine-authored
rows) and `nmp-ffi/convert.rs:472` for caller-supplied `Signed` payloads (moving
under #52; see sequencing). Nothing re-verifies stored rows on query, decode, or
re-serve. `Filter::match_event` ignores `sig`; `StoredEventRecord` round-trips a
zeroed 64-byte sig through JSON without validation; schnorr `Signature` parsing is
length-checked only. The sentinel approach keeps the single query/supersession
path untouched, which is exactly the §4.1 "store citizen, not overlay" ruling.
Two conditions: (a) the row's `SigState` must be projected to the app surface
alongside the row (apps must never be given a sentinel-sig event without the
means to know it is pending); (b) *recommended, non-blocking hardening*: the
dedup-by-id arm may adopt a **verified** real signature into a row still in
`SigState::Pending` (cross-device same-id echo is the only path; negligible but
free to handle correctly).

**Q2 — Outbox journal in `nmp-store`'s redb `Database`: APPROVED.** This is the
load-bearing structural call. `docs/architecture/crate-boundaries.md` does not
exist in this repository (that path belongs to a different workspace); the
governing texts here are VISION §4 and bug-class ledger #1 (one mutating door, no
public index/storage setter). Three grounds: (1) **redb atomicity is a
per-`Database` property** — one `begin_write` spanning EVENTS/ADDR_INDEX/indexes
*and* OUTBOX_* is the only way to satisfy #3's "one crash-atomic commit", so
co-residency is forced, not chosen. (2) `nmp-store` is **already** the durable-
facts boundary, not a bare event table: COVERAGE watermarks, permanent
TOMBSTONES/ADDR_TOMBSTONES, and EXPIRATION_INDEX are all non-event durable facts
living in the same `Database` behind typed doors. The outbox journal is the same
shape of fact. (3) The alternatives are worse by this repo's own rules: an
engine-owned `Database` inverts the layering (`nmp-engine` grows a redb dep and
a second persistence door) and would force transaction-handle injection into the
store — a back-door index setter, a ledger-#1 violation; a separate outbox crate
cannot share one `Database` without one crate owning `begin_write`, which
recreates the same question. **Constraints that keep this legitimate:** the store
exposes only the typed doors (`accept_write`/`promote_signed`/compensation/
`recover_outbox`) — never raw table or transaction access; every policy decision
(retry ownership, scheduling, signer orchestration) stays in `nmp-engine`; the
`nmp-store` module doc is updated to say "event **and** durable-outbox store" so
the broadened remit is documented, not drifted into.

**Q3 — OutcomeUnknown / at-most-once split: CONFIRMED, with one modification
(required change R2).** Seeding the persisted attempt table, the write-through on
each transition, and the never-blindly-retry-on-reload rule here, with the
backoff/concurrency/policy engine as a follow-up issue, is the right cut. But do
**not** fold `min(next_eligible_at)` into `next_deadline()` in this frame: a
deadline that fires with no owner to consume/advance it re-arms as already-past on
the next loop iteration → zero timeout → **busy-loop spin** in the `recv_timeout`
driver. The fold is five lines whenever the retry owner lands (the
`next_deadline` doc already anticipates it); it ships *with* the follow-up.
Restart resend is covered without it — see R1's boot re-dispatch.

**Q4 — MemoryStore crash-safety = no-op: CONFIRMED.** MemoryStore implements the
same doors with the same atomic *semantics* in-memory (so U1a tests the door
contract cheaply); `recover_outbox` returns empty by construction. No contract
clause requires refusing durable writes on a volatile store — durability is a
property of the backend, and MemoryStore is the test/ephemeral backend. Document
this on the `EventStore` trait method, not just in the plan.

**Q5 — Pinned signing-identity ref placeholder: CONFIRMED.** `expected_pubkey`
is the *real* pinned identity and alone satisfies #43's "pins the chosen identity
at acceptance"; the opaque `signing_identity_ref` placeholder is the persistence
hook #47 will give meaning to. This frame must not grow provider/vault/selection
logic.

### Contract validation (the checks the checkpoint was asked to run)

- **One canonical store/reactivity path — HOLDS.** Pending rows enter EVENTS via
  `accept_write` (same supersession/tombstone logic), feed the resolver via
  `accept_local → react(inserted, removed)` — the one recompute engine — and exit
  through `refresh_all_handles`. No shadow tier, no second matcher, no overlay.
- **Dedup vs relay echo — CORRECT.** NIP-01 id = sha256 of
  `[0, pubkey, created_at, kind, tags, content]`; the signature is not an input,
  so the id is final at acceptance. Promotion writes the real sig *before*
  `Effect::PublishEvent`, so the echo always dedups by id against a row already
  carrying the real signature. `Duplicate { provenance_grew }` merges relay
  provenance; the "sending…" chip resolves off `seen`.
- **Atomic boundary — GENUINELY ALL-OR-NOTHING**, given R7: `accept_write` is one
  `begin_write` spanning event tables + OUTBOX_*, which requires the
  `AcceptWrite` argument to carry the full journal payload (frozen body, expected
  pubkey, identity ref, durability, routing, receipt id/state) so the *store*
  writes the displaced stash and journal rows in the same transaction the
  supersession happens in — not the engine after the fact.
- **Boot `recover_outbox` reconstructs a consistent EngineCore** — yes, given
  R1's corrected sig_state taxonomy: rows are already in the store (live from the
  first subscription), `pending`/`event_to_receipt` rebuild from the journal,
  `next_receipt` seeds past the max recovered id, recovered `PendingWrite.sink`
  is `None` until a caller reattaches (the field is already `Option`).

### Required changes (builder must incorporate; none needs a redesign)

1. **R1 — Fix the §2.3 recovery sig_state taxonomy.** §1.1 defines
   `Pending` = pre-signature, but §2.3 glosses `Pending` as
   "signed-but-not-fully-acked" — an internal contradiction. Correct recovery
   classes: `AwaitingSigner` **and** `Pending` (a sign request was in flight and
   the response is lost with the process) → re-emit the `RequestSign` path when a
   matching signer is attached (double-signing after a crash is harmless: same
   id, either valid signature promotes); `Signed` with non-terminal lanes →
   **boot-time re-dispatch**: re-emit `Effect::PublishEvent` (exact frozen bytes)
   per non-terminal Durable lane, writing a new attempt ordinal. AtMostOnce lanes
   that were `InFlight` at crash reload as `OutcomeUnknown` and are never resent.
   Boot re-dispatch is what satisfies U4's "exact-byte resend after restart"
   without a retry engine.
2. **R2 — Drop the `next_deadline` fold from this frame** (spin hazard, Q3
   above). Move the fold and U4's "persisted `next_eligible_at` fires via the
   deadline driver" test into the retry-owner follow-up issue; file that issue as
   part of this frame's landing.
3. **R3 — Define `AcceptOutcome::Refused` handling.** `accept_write` runs the
   door's tombstone/expiry refusal checks; a refused acceptance (e.g. composing
   into a tombstoned address, or an already-expired NIP-40 body) is a terminal
   typed failure to the caller with **no journal residue** — nothing to recover,
   all in the same transaction.
4. **R4 — Ephemeral is receipt-only.** Per the promoted VISION and landed U1
   contract, `Durability::Ephemeral` never enters the durable delivery journal
   and never gains a pending store row, but it still receives a store-allocated,
   reattachable receipt through `accept_ephemeral`. Durable **and** AtMostOnce
   use `accept_write`. An unfinished ephemeral receipt becomes `Abandoned` on
   restart; no publication obligation or retry survives with it.
5. **R5 — GC claim per open intent.** `gc` evicts regular events matched by no
   claim; an unsigned pending kind:1 row must not be GC-evictable before it ever
   signs. The engine's `ClaimSet` construction adds a claim per open
   `OUTBOX_INTENTS` row (retraction doc §4.1 already prescribes exactly this —
   the plan omitted it).
6. **R6 — `promote_signed` durably drops the stash.** §4.2's "on promotion: drop
   the stash" must be durable: the `promote_signed` transaction also deletes the
   `OUTBOX_DISPLACED` row (and `OUTBOX_INTENTS.sig_state → Signed`) so recovery
   after a promote never sees a stale displaced stash.
7. **R7 — `AcceptWrite` carries the full journal payload** (see atomic-boundary
   check above) — make the struct explicit in U1 so the single-transaction
   property is structural, not a calling convention.
8. **R8 — Journal row lifecycle.** State when an `OUTBOX_INTENTS` row is deleted:
   on compensation (§3.1, already specified), and on the intent reaching
   all-lanes-terminal with at least the receipt evidence written through
   (`OUTBOX_ATTEMPTS` rows may be retained as evidence per #3's append-only
   spirit — builder's choice — but the *intent* row's terminal deletion must be
   defined so `recover_outbox` has a bounded working set).

### Sequencing vs #52 (my call — sets the build order)

- **The single `Event::verify` for caller-supplied `Signed` payloads ultimately
  lives at THIS frame's acceptance boundary** — `on_publish`'s `Signed` arm
  verifies *before* `accept_write`/journal (U3). That is #52-Q2's "deeper-correct"
  home: every entry point (facade, FFI, `nmp-demo`, direct `Handle` embedders,
  in-crate tests) inherits it, and it composes with U3's existing obligation to
  validate signer *results* before `promote_signed` — one acceptance boundary,
  two verify sites (caller-supplied at accept; signer-result at promote), zero
  path that reaches the wire unverified.
- **Build order:** #52 Unit A (facade crate, *including* its interim facade-level
  verify — do not leave the guarantee unheld while U3 is in flight) lands first
  and in **parallel** with this frame's U1 (store) + U2 (resolver) — the file
  sets are disjoint. #52 B (FFI rethread; coordinate with
  `build-ffi-signed-publish`) and C follow A. **U3 lands after #52 A/B** and, in
  the same PR, moves the authoritative verify to the acceptance boundary and
  **deletes the facade/FFI duplicate**, threading the engine's typed rejection
  outward (hard-break-in-one-PR, no parallel verify paths left behind). Then U4,
  U5. #52 D (parity harness) runs last — its tampered-`Signed` parity test then
  proves the *engine-level* gate, which is a stronger falsifier than the
  facade-level one it was designed against.
- U3/U4 remain the only `core/mod.rs` writers in either frame; no co-owned
  worktree is needed if this order is kept — U3 is the serialization point.

### Residual risk

- The interim window where the verify lives in the facade (#52 A) while direct
  `Handle` embedders remain unguarded persists until U3 — accepted; it is
  today's status quo, shrunk to one frame.
- Journal growth: OUTBOX_ATTEMPTS retained as evidence is unbounded over a
  long-lived replica if never trimmed (same shape as the tombstone decision, but
  attempts are per-write × per-relay, not rare). The retry-owner follow-up must
  state a retention rule; flag it in that issue's body.
- Double-sign on crash recovery (R1) is protocol-harmless but may surface as a
  duplicate NIP-46 approval prompt to a human signer — cosmetic; #47's provider
  model is the place to correlate/replay signer RPCs (already its charter under
  #3's "one correlated signer RPC").
