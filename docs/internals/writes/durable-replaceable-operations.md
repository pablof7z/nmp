---
title: Replaceable operations — protocol facts, current behavior, experiment record
category: writes
slug: durable-replaceable-operations
status: built
date: 2026-08-19
owns:
  - the protocol reasoning for why an app action on replaceable state is not
    necessarily one Nostr event
  - what NMP actually does with configured semantic operations today
  - the measured evidence from the rejected #1412 prototype
related:
  - docs/design/durable-write-signing-and-retry.md
  - docs/internals/writes/publish-queue.md
  - docs/internals/routing/outbox.md
  - docs/known-gaps.md
issues:
  - "#1380 — offline-safe semantic operations epic"
  - "#1412 — rejected bodyless materializer experiment"
  - "#1432 — body-complete semantic operation acceptance"
  - "#1433 — complete successor rematerialization"
  - "#1624 — construction-time capability input, direct in-thread materialization"
---

# Replaceable operations

This document records three things: the protocol problem replaceable events
create, what NMP does about it today, and what one rejected experiment
measured. It is not a specification for future work.

---

## 1. The problem in Nostr terms

Every Nostr event is immutable. A regular event such as kind `1` remains an
independent stored assertion rather than competing for one NIP-01 replacement
winner. If an app asks to publish it, the event body can be fixed once. A relay
later receiving another kind `1` does not change what the first event meant.

A replaceable event is different. [NIP-01](https://github.com/nostr-protocol/nips/blob/master/01.md)
defines kinds `0`, `3`, and `10000...19999` as replacing another event at
`(author, kind)`. Kinds `30000...39999` replace another event at
`(author, kind, d)`. The current value is selected by NIP-01 ordering: greater
`created_at` wins and the lexicographically smaller event id breaks an
equal-timestamp tie.

Suppose a user changes only their profile name while offline. The device can
construct a complete kind `0` from the profile it currently knows. But while it
is offline, another device may change the profile picture. Publishing the first
device's frozen whole event later can make the name change succeed by erasing
the newer picture change.

The durable fact the first device should retain is therefore not necessarily:

```text
publish these exact kind-0 bytes
```

It may instead be:

```text
set the name field to "Pablo" on the best qualified profile state
```

That operation can be replayed over a newer profile event. The profile module
knows how profile fields map to JSON and which unknown or malformed content it
must preserve. Generic publishing infrastructure does not.

This is not special to profiles or NIP-02. The same distinction appears in a
follow or unfollow represented by a kind `3`, in adding one relay to a relay
list without overwriting a concurrent change, in changing a long-form article
title while preserving a body edit from another device, in adding or removing
one item in an addressable public or private list, and in applying a
protocol-specific migration while preserving fields the current implementation
does not understand. Kind `3` is one useful example, not a privileged case.

### 1.1 Exact whole-event replacement stays useful

Not every schema has a meaningful or implemented semantic operation. An app may
already possess the complete desired next event and deliberately want an
all-or-nothing compare-and-swap against the event it read:

```text
replace source E7 with this complete body, but only if E7 is still current
```

If E8 has become current, NMP refuses. It does not guess how to merge a body
whose semantics it does not own. Semantic replay extends the write model; it
does not turn every whole-event replacement into a guessed field merge.

### 1.2 "Online" is the wrong abstraction

A Nostr client is not simply online or offline. At one moment it may have a
signer but no relay connection; be connected to one of an author's three write
relays; have reconciled two selected sources and still be waiting on a third;
know four recipients' inbox relays and still be resolving a fifth; have a
complete unsigned event but be waiting on a remote signer; have one relay
accept the current event while another is in backoff; or discover a newer
source event after a predecessor was already accepted by a relay.

NMP therefore tracks exact knowledge and work — which source has been checked,
which event is currently materialized, which signer result belongs to it, and
which relay has answered for which event id — rather than one connectivity bit.

---

## 2. Source plan and destination plan are different

The **source plan** answers: where may the current replaceable value be learned
from? The **destination plan** answers: where should the resulting signed event
be published?

They may overlap, but one is not inferred from the other. Reading a kind `0`
from an indexer does not automatically make the indexer a write relay.
Publishing to an author's write relay does not prove that every selected source
has been reconciled.

"Selected current source" means the NIP-01 winner among the source evidence
admitted by the chosen plan. It never means the newest event on all Nostr
relays, the newest event any device has ever produced, an event that cannot
later be replaced, or a compare-and-swap lock held on the network.

These states stay distinct: no event cached yet; selected sources not reached;
a selected source reachable but not settled; every required selected source
settled with no event at the coordinate; selected sources disagreeing with
NIP-01 ordering picking one observed winner; a cached base provisionally usable
under an availability-first policy.

NIP-01 has no standalone "absence" message. **Qualified absence** means the
declared source plan completed its required initial query evidence — normally
EOSE for each required subscription — without yielding a surviving event at the
coordinate, after applying deletion and expiry rules. It stays scoped to those
sources and that observation interval. A connection failure and an EOSE are
different evidence.

---

## 3. Materialization time

A successor must outrank the source it replaces and any prior local generation
at the same coordinate, and reconnecting must not continually change the event
id. For an automatically stamped successor:

```text
created_at = max(
    latest logical time of every still-contributing accepted operation,
    selected source created_at + 1,
    previous local materialization created_at + 1
)
```

Operation time keeps the successor from being stamped before a contributing
accepted action. Source `+ 1` beats the observed source without relying on the
event-id tie-break. Previous local generation `+ 1` stops a later
rematerialization from losing to an earlier local event.

Worked example on the kind-3 case — `t+0 add Alice`, `t+1 add Bob`, `t+5`
source containing Carol, `t+8` reconnect — gives `created_at = 6`. `1` would
lose to the source at `5`; `8` would make wall-clock reconnection mutate
identity even though no user or source fact changed; `6` is the smallest
timestamp expressing the required ordering. If no newer source exists, `1` may
remain sufficient. If a previous local generation had timestamp `7`, a
successor uses at least `8` even if the newly selected source is older.

Closing and reopening the app, reconnecting a socket, restoring replay
capability, or retrying a relay produces no new `created_at` when the selected
source and compact operation program are unchanged. The same meaning recovers
to the same current materialization — which is what makes restart
deterministic, relay deduplication effective, history bounded, and receipt
evidence honest.

---

## 4. What NMP does today

NMP accepts configured semantic operations only after producing a complete
optimistic event, retains their ordered operation meaning under ordinary
receipts, and atomically replays active operations over each newer verified
relay source. That source and its complete successor commit together, so a live
query never exposes the raw source between complete local generations.

Falsifiers covering this live in
`crates/nmp-engine/src/core/semantic_settlement_falsifier_tests.rs`
(`every_member_of_a_published_generation_settles`,
`every_member_of_n_generations_settles_under_mixed_outcomes`,
`a_stale_ack_for_a_superseded_generation_cannot_settle_its_successor`,
`settled_generations_leave_no_durable_state_and_are_not_resurrected`) and in
`crates/nmp-nip02/tests/body_complete_semantic_acceptance.rs`
(`alice_then_bob_keep_two_receipts_and_one_complete_pending_event`,
`invalidated_registration_and_materializer_refusal_leave_no_custody`).

The materializer contract lives in
`crates/nmp-grammar/src/replaceable_materializer.rs`; the engine-side operation
state in `crates/nmp-engine/src/core/write/replaceable_operation.rs`.
Capabilities are engine construction input supplied before the state owner
starts store recovery. Capability code runs directly on that state thread
between a closed read transaction and one short compare-and-commit transaction.
There is no executor, worker slot, detached thread, panic translation,
completion command, timeout, or shutdown contract around it — #1624 deleted the
plugin shape that had one.

Generation-qualified signing and relay publication are tracked separately in
`docs/known-gaps.md`.

Two facts about current behavior contradict older write documentation still
quoted elsewhere: a replaceable conflict does allocate a terminal refusal
receipt (older docs said it allocated none), and durable restart retries the
exact same frozen event bytes rather than offering an at-most-once mode with
`OutcomeUnknown`.

### 4.1 One interoperability rule stronger than NIP-01's examples

An exact correlated `OK false` with the machine-readable `duplicate:` prefix is
classified as publication evidence for that event id, because the relay reports
that it already has the event. NIP-01 shows a duplicate example with `OK true`;
NMP accepts the widely encountered false-plus-duplicate form narrowly.
Free-form text containing "duplicate" does not qualify, and every result stays
relay-, session-, identity-, and event-id-specific.

---

## 5. What Nostr cannot do here

- **No global head.** Every "current source" claim is scoped to a declared
  source plan. Other relays or devices may know a newer event.
- **No network compare-and-swap.** A relay does not reserve the replaceable
  coordinate between this client's read and write. The final fetch/publish race
  remains.
- **No generic causal order.** Two signed events provide timestamp ordering,
  not necessarily causal ordering of the underlying user actions. Same-item
  conflict is capability policy unless the schema carries stronger metadata.
- **No winner permanence.** `OK true` is evidence that one relay accepted one
  event. It cannot prove that the event will remain current there or elsewhere.
- **Availability versus source confidence is a policy choice, not a fact.**
  Waiting for every planned source reduces stale-base risk and may prevent all
  progress when one relay is unreachable. Publishing from currently qualified
  sources improves availability and may require successor re-fanout later.
- **Replay preserves only meaning the capability models.** Unknown bytes can
  often be preserved exactly, but a capability cannot merge two concurrent
  semantic changes it cannot identify. Exact replacement is the honest fallback.

---

## 6. The #1412 prototype: what it measured

The #1412 experiment's bodyless lifecycle and parallel persistence were
rejected for production. Its measurements are retained because they are the
decision evidence behind that rejection and behind the current shape.

Its first isolated registry prototype showed that two independently packaged
capability implementations can be supplied through one NMP-owned contract; that
the NMP mechanism depends on neither capability; that exact materializer and
format identity survive restart; that missing implementation, mismatched
format, typed refusal, and stale completion can be represented; and that
capability code can run outside the store lock and commit through an exact
source/revision/generation fence.

Nine samples of 100,000 stateless dispatch iterations (not a backlog of
unpublished operations) measured:

```text
direct transform                 221 ns median
registry lookup + dynamic call   244 ns median
lookup/Arc portion                23 ns median
extra transform allocations        0
```

Nanosecond microbenchmarks are host- and load-sensitive; these are evidence for
feasibility, not a performance contract.

The second layer routed a genuinely bodyless semantic payload through the real
public Rust `Engine::publish(WriteIntent)` door. Release measurements on an
Apple M3 Max used nine fresh-process batches. The final experiment head is
`566b5ef246152267a94728bd31517beceb3156a3`; its raw artifact, SHA-256
`fd350429f85a5a947639dec0174bc761cf09c05cd0f3702da2659a74e80b30b0`, retains
every iteration.

| Workload | Median | p95 |
|---|---:|---:|
| Ordinary in-memory event acceptance | 66 µs | 220 µs |
| Bodyless semantic in-memory acceptance | 28 µs | 226 µs |
| Ordinary redb event acceptance | 5.84 ms | 11.52 ms |
| Bodyless semantic redb acceptance | 10.37 ms | 20.05 ms |
| Ready semantic acceptance through installed body | 78 µs | 1.11 ms |
| Reopen until exactly 100 receipts are inspection-ready | 16.1 ms | 48.8 ms |
| Late registration through all 100 reopened redb installs | 1.66 s | 1.99 s |
| Reopen, register, and install one source-driven successor | 26.7 ms | 29.7 ms |
| One hundred 5 ms handler jobs, end to end | 156.8 ms | 179.5 ms |

Exact ordered sequences of 1, 10, and 100 retained operations all passed their
materialization oracle; median reopen-to-install time was 18.8, 17.4, and 20.2
milliseconds respectively. Those sizes revealed no preparation cliff.

The ordinary and bodyless acceptance rows perform different work and end in
different states; the difference between them is not an architecture tax
measurement.

Bodyless acceptance, missing-handler persistence, late registration, and a
nullable initial event id appear above only so the measurements stay
interpretable. The #1412 decision gate rejected all four, and #1624 deleted the
plugin shape the prototype ran on. Every row naming registration or a handler
job measures a path that no longer exists.

### 6.1 What it did not prove

Using the real acceptance door proved the front half of one public lifecycle,
not the ordinary lifecycle after a body is installed. The experiment stored
semantic operation, resource, and receipt projections in parallel experimental
JSON tables inside the same redb database; sharing a database and id allocators
does not make those records the canonical pending event, signing obligation,
delivery lanes, or terminal receipt state.

It did not prove a production binary schema or migration, canonical optimistic
query transitions, the full source-plan and access-context qualification model,
atomic replacement of canonical source and effective query state, current
publish-queue successor delivery, signing or routing of an installed semantic
materialization, event-qualified relay attempts or acknowledgements or retry or
settlement, cancellation with shared materializations, removal or compaction of
semantic receipts and operations, complete Rust/FFI/Swift/Kotlin projection, or
capability-defined normalization and production storage bounds.

Two known limits at the time of writing: relay ingest extracts the changed
replaceable coordinate and prepares only that target, but this is
code-inspected rather than proven by a two-target runtime falsifier. Restart
recovery visits every retained semantic coordinate to re-establish its source
owner rather than paging that work, so a very large retained backlog lengthens
store recovery. The 1, 10, and 100-operation measurements above are the
evidence that this has not mattered so far.
