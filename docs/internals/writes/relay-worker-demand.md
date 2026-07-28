---
title: Durable write relay-worker demand
category: writes
slug: relay-worker-demand
status: designed
date: 2026-07-28
investigated_revision: 057f59ef9f3a5cbc1a3ce4e3c88d189d55b28244
runtime_evidence_revision: 9a8ad1fd1a2999b067c19d842467a76a257f7f25
owns:
  - how durable outbox lanes determine which relay sessions the write plane must keep alive
  - why the current implementation repeatedly reads and decodes every lane
  - the rebuildable in-memory projection intended to remove those reads
  - the projection's ordering, restart, and degraded-state requirements
related:
  - docs/design-record.md
  - docs/bug-class-ledger.md
  - docs/known-gaps.md
issues:
  - https://github.com/pablof7z/nmp/issues/985
  - https://github.com/pablof7z/nmp/issues/975
  - https://github.com/pablof7z/nmp/issues/968
  - https://github.com/pablof7z/nmp/issues/771
  - https://github.com/pablof7z/nmp/issues/904
---

# Durable write relay-worker demand

This document records both the built behavior and the intended replacement for
NMP's repeated outbox-lane scans. It is deliberately not a backend-selection
record. The performance problem is caused by asking durable storage to
reconstruct already-known reducer state during ordinary dispatch; changing the
database while preserving that access pattern would leave the architectural
mistake intact.

The design is tracked by
[#985](https://github.com/pablof7z/nmp/issues/985). As of the date and
`investigated_revision` above, the repeated scan is **built** and the
in-memory projection is **designed, not implemented**.

## The questions that led here

Pablo first asked:

> is this related to the work we were doing in this session? is nmp still storing events as json and deserializing on every scan?

The precise answer is:

- canonical Nostr events are stored in a compact binary encoding;
- outbox-lane control records are stored as JSON values under string keys;
- the hot path scans and JSON-decodes those lane records repeatedly;
- therefore this is related to storage representation work, but it is not
  evidence that canonical events are stored as JSON.

The design question was:

> how do you propose we reduce the repeated scans?

The answer is to make relay-worker demand an explicit, reducer-owned,
rebuildable projection of durable lane state. Durable storage remains the
authority. The projection answers the frequent scheduling question without
re-reading the authority every time.

## System model

A durable write intent can own one outbox lane per destination relay. A lane is
the durable state machine for delivery to that relay: waiting, eligible,
attempting, retrying, suspended, or terminal. Nonterminal lanes require NMP to
keep the corresponding authenticated relay session available. Terminal lanes
do not.

The relevant ownership split is:

```text
durable outbox tables
  authority for intent, lane, attempt, and receipt state
          |
          | bootstrap/recovery and committed transition results
          v
EngineCore / PendingWrite
  rebuildable projection of nonterminal lane relays
          |
          | pure in-memory union
          v
required RelaySessionKey values
          |
          v
runtime opens, retains, or retires relay workers
```

The store answers “what survived?” The reducer answers “what workers are
needed now?” The runtime owns the workers themselves.

## Built behavior: reconstructing demand during dispatch

`EngineCore::relay_worker_requirements` combines read and write demand.
`EngineCore::write_relay_workers` computes the write half.

Today `write_relay_workers`:

1. starts with relay sessions referenced by in-flight attempt correlations;
2. walks every `PendingWrite`;
3. includes the pending write's already-known pending, unstarted, and
   route-blocked relays;
4. calls `EventStore::recover_outbox_lanes(intent_id)` for every pending intent;
5. range-scans the durable lane table, JSON-decodes every returned lane, and
   parses its relay URL;
6. retains the relay session for every nonterminal lane.

`runtime::dispatch_core_effects` calls this worker-requirement calculation
before dispatching effects. Worker retirement and relay-state pruning also ask
for the requirements. An unchanged reducer can therefore perform the same
store reads, B-tree comparisons, JSON decoding, and URL parsing many times.

This is not recovery in the operational sense even though the store method is
named `recover_outbox_lanes`. Recovery data is being used as a recurring query
interface.

### What is binary and what is JSON

Canonical event insertion calls `binary_event::encode_event` and stores the
resulting bytes. The representative ingest schema and its write amplification
are a separate concern.

Outbox lanes currently use:

```rust
TableDefinition<&str, &str>
```

The key contains the intent identifier and relay URL. The value is a
JSON-encoded `RecoveredLane`. `recover_outbox_lanes` performs a key-range scan,
decodes every matching value with `serde_json`, reconstructs relay URLs, and
sorts the result.

Changing the lane key or value encoding may reduce the cost of each scan, but
it cannot make a redundant scan necessary. The first change should remove the
repeated reads from the dispatch path.

### Why this can stall unrelated engine work

The engine facade and the engine thread are distinct pieces of the effect.
The scan is not simply “executed while holding the public `Engine` mutex.”

The facade holds its mutex while a synchronous command waits for the engine
thread's reply, protecting the handle from racing shutdown. If the single
engine thread is busy performing repeated lane scans from earlier dispatch
work, it cannot promptly service a later command. That later caller remains
inside the facade mutex while waiting, so other facade calls queue behind it.

The scan therefore amplifies visible mutex starvation without being the same
thing as the mutex design. Removing repeated scans should shorten engine-thread
occupation. Splitting or redesigning the facade mutex is separate work and
needs its own evidence.

## Runtime evidence and its limits

A profiler report supplied from a running Mosaico daemon pinned to NMP revision
`9a8ad1fd1a2999b067c19d842467a76a257f7f25` described the headline as:

> The headline: one thread is pegged, and it's blocking the whole runtime

During that sample, the NMP engine thread was on CPU for the full window.
Inclusive samples attributed about 68% of that thread to
`recover_outbox_lanes`, about 61% to `required_relay_workers`, about 31% to
redb range-iterator construction, and about 17% to lane JSON decoding. String
key comparison and UTF-8 validation were also visible.

Those percentages overlap because they are inclusive call-stack samples; they
must not be added together. The report supports the diagnosis that repeated
lane reconstruction dominates that captured engine window. It does not prove
the exact end-to-end gain on current `master`.

Current-source inspection at `investigated_revision` confirms that the
repeated-scan path still exists. A before/after profile on the same workload is
required before claiming a measured improvement. The expected result is to
remove most scan-related engine CPU from unchanged dispatch passes, not to
promise a specific whole-process percentage in advance.

The same older Mosaico process also had a large number of per-observation OS
threads. That is not owned by #985: current NMP `master` already contains the
observation-thread removal from #680. The old runtime sample remains valid for
the scan diagnosis because the path still exists, but its thread-count finding
must not be presented as current-master evidence.

## Designed behavior: a rebuildable projection

Each `PendingWrite` should carry an exact set such as
`nonterminal_lane_relays`. The name is illustrative; the semantic distinction
is load-bearing:

- `lane_relays` continues to mean every persisted lane learned for the intent,
  including terminal lanes needed by the reverse wake index;
- `nonterminal_lane_relays` means exactly the durable lanes that still require
  write-plane work.

The reducer populates the projection from the lanes returned by startup
recovery and bootstrap. Every successful durable lane mutation then feeds its
returned post-commit `RecoveredLane` through one projection update path:

```text
committed lane state is nonterminal -> insert relay
committed lane state is terminal    -> remove relay
```

`write_relay_workers` becomes a pure in-memory union of:

- the projected nonterminal lane relays;
- unstarted or conservatively route-blocked relay ownership that has not yet
  become a lane;
- sessions referenced by in-flight attempt correlations.

It maps each relay to a `RelaySessionKey` using the write's signing identity and
NIP-42 access context. The URL alone is not an identity: the same relay URL
under Public access or different authenticated accounts must not be
accidentally merged.

### One mutation boundary, not remembered call sites

The store API already returns post-commit lane state from bootstrap and most
lane transitions. Many current call sites discard those returned values.

The implementation must introduce a reducer-owned boundary that consumes all
returned `RecoveredLane` values and updates both lane projections. Merely
adding projection updates to today's visible call sites would turn correctness
into reviewer memory. A future transition or route-revision path could persist
a lane while forgetting to update worker demand.

The exact internal type is an implementation choice, but bypassing the
projection should be mechanically difficult. The relevant rule is:

> Any code path that makes a lane durably visible must also pass the committed
> lane state through the reducer projection before ordinary dispatch resumes.

The durable store remains the sole retry authority. The projection is derived
state and can always be rebuilt; it is not a second outbox.

### Route revisions and parked intents

[#975](https://github.com/pablof7z/nmp/issues/975) designs route revision:
re-running Auto routing may mint new durable lanes for an existing intent.
The projection must hook the lane-minting boundary, not only today's initial
bootstrap call, so route revisions cannot bypass it.

The future parked-write state from
[#968](https://github.com/pablof7z/nmp/issues/968) owns no lane while awaiting a
route. It therefore creates no relay-worker demand and must cause no
per-dispatch store read. A large parked population could still make an
O(pending) in-memory union expensive; the current production profile
under-predicts that future population.

The first implementation should keep the simple reducer-local union and
measure it. A global relay-session reference-count projection is a possible
follow-up only if the in-memory walk itself becomes material. It should not be
introduced speculatively.

## Atomicity, ordering, and recovery

### Normal committed transitions

For a successful store operation, ordering is:

```text
commit durable lane transition
  -> receive committed post-state
  -> update reducer projection
  -> compute and dispatch effects
```

The projection must never claim a transition that the store rejected.
Conversely, ordinary execution must not dispatch from stale pre-transition
state after the store returned a committed post-state.

Closing an intent is special only in shape, not authority. The current
`close_if_all_lanes_terminal` performs a recovery scan before calling
`close_terminal_intent`. The store's close operation already validates
transactionally that all lanes are terminal. The projection may decide that a
close attempt is plausible; the store remains the final authority and must
refuse an invalid close.

### Startup

Before ordinary query/effect service, startup reconstructs pending writes and
their exact nonterminal-lane sets from durable state. Once reconstruction
succeeds, unchanged dispatch must not consult lane storage merely to calculate
worker demand.

Restart equality is semantic, not a row-count check: reconstructed projection,
ordered worker requirements, lane states, and access identities must equal the
pre-close durable meaning.

### Persistence failures

This design must use the durability classification introduced by #904:

- `DurabilityOutcome::Absent` means the transition is known not to have
  committed. Keep the old projection.
- `DurabilityOutcome::Unknown` means the transition may be absent or durable.
  The reducer cannot safely pretend either outcome is known.

An unknown terminal transition can conservatively retain the previously
required worker until reconciliation. An unknown lane-creation transition is
harder: retaining the old projection is insufficient because the possibly
durable lane may be new. The reducer must conservatively retain or add every
candidate relay that the attempted mutation could have created, mark the
projection degraded, and reconcile from reopened durable state.

The degraded projection must be a conservative superset of possibly required
sessions. It may temporarily keep an unnecessary worker. It must not drop a
worker for a lane that may have committed.

Falling back to a full lane scan on every dispatch would recreate the original
problem and is not an acceptable degraded mode. Reconciliation is an explicit
recovery action.

[#771](https://github.com/pablof7z/nmp/issues/771) owns the broader contract for
preserving durable lane-transition evidence when store operations fail. Its
older premise that an error always means “committed none” is superseded for
`DurabilityOutcome::Unknown`. #985 must integrate with that contract rather
than silently deciding ambiguous durability.

If startup cannot read enough durable state to construct even a conservative
projection, there is no prior worker set to retain. The correct service-level
behavior for that case remains an open recovery decision owned with #771 and
#904; this document does not claim it is solved.

## Invariants

The implementation is correct only if all of these hold:

1. Durable lane state is the authority; the in-memory projection is
   reconstructible and never independently persisted as outbox truth.
2. After successful reconstruction, worker-demand calculation performs no
   lane-store read.
3. Every committed lane creation or transition updates the projection before
   ordinary effect dispatch resumes.
4. A relay is present in `nonterminal_lane_relays` exactly when its known
   committed lane is nonterminal, except during an explicitly marked degraded
   state where the set may only over-retain.
5. A persistence outcome classified Unknown never causes a possibly required
   relay worker to be dropped.
6. Public, NIP-42, account, and session identities do not alias merely because
   their relay URLs match.
7. Route revision uses the same lane-minting/projection boundary as initial
   bootstrap.
8. A parked intent with no lane contributes no worker and causes no
   per-dispatch store read.
9. The store transaction, not the projection, is the final authority for
   terminal intent closure.
10. Close/reopen reconstruction produces the same semantic projection and
    ordered worker requirements as the durable pre-close state.

## Falsifiers and performance evidence

The first implementation should include tests that would fail if the old
architecture or a partial projection survived:

- With many pending intents and no state changes, repeated dispatch performs
  zero `recover_outbox_lanes` calls after reconstruction.
- Every lane state transition updates the projected set, and reconstruction
  from the store produces the same exact set after each transition.
- A newly minted route-revision lane becomes worker demand without a global
  rescan.
- Many route-parked intents create zero worker demand and zero lane reads
  during repeated dispatch.
- The same URL under Public and NIP-42 access, and under distinct signing
  identities, remains isolated.
- A known-absent persistence failure leaves the old projection unchanged.
- An unknown terminal transition retains the old worker.
- An unknown lane-creation transition conservatively retains every candidate
  worker until explicit reconciliation.
- Exact state and ordered worker requirements survive close and multiple
  reopens; row counts alone are not sufficient.

Performance verification should replay the workload that exposed the problem
and compare:

- process and engine-thread CPU;
- calls to `recover_outbox_lanes`;
- lane rows and bytes decoded during unchanged dispatch;
- engine command latency or facade wait time;
- relay-worker counts, to prove the reduction did not come from dropping
  required work.

The expected architectural gain is exact: unchanged worker-demand calculation
goes from store reads proportional to pending durable writes and their lanes to
zero store reads. Whole-process CPU gain remains a measurement, not a design
claim.

## Backend independence and non-goals

Pablo explicitly warned:

> just fyi, I never "chose" redb, if that's biting us we can 100% consider using something else, ok?

and:

> I think the redb was choosen at some point because wasm is a target for nmp, but not sure

This design does not privilege redb. Any backend used by NMP should expose the
same semantic lane-transition and recovery contract. Native and WASM targets
do not need to use the same physical engine.

The first #985 implementation intentionally does not:

- select or migrate a production database;
- change the canonical event representation;
- replace outbox JSON or string keys;
- use unchecked UTF-8 to reduce comparison cost;
- redesign the public engine mutex;
- add a speculative global worker reference-count cache;
- implement #975's routing policy or #968's parked-write lifecycle.

Those can be evaluated independently after the repeated read amplification is
removed and re-profiled.

## Source guide

- `crates/nmp/src/core/mod.rs`
  - `PendingWrite`
  - `EngineCore::relay_worker_requirements`
  - `EngineCore::write_relay_workers`
  - relay-state pruning and wake indexes
- `crates/nmp/src/core/write.rs`
  - durable lane bootstrap and transition call sites
  - places currently discarding returned `RecoveredLane` values
- `crates/nmp/src/runtime/mod.rs`
  - `dispatch_core_effects`
  - worker retirement and retry
- `crates/nmp-store/src/lib.rs`
  - backend-independent lane transition and recovery API
  - persistence-error durability classification
- `crates/nmp-store/src/redb_store/schema.rs`
  - redb lane table representation
- `crates/nmp-store/src/redb_store/outbox.rs`
  - lane key and JSON codec
- `crates/nmp-store/src/redb_store/outbox_ops.rs`
  - lane range scans, transition commits, and terminal closure
- `crates/nmp-store/src/redb_store/canonical.rs`
  - binary canonical-event insertion
- `crates/nmp/tests/core_headless/persistence_failures.rs`
  - existing read-count coverage and explicit evidence that worker accounting
    remained globally scanned after the narrower wake-index optimization

## Open decisions

The implementation PR still needs to settle these internal details without
weakening the invariants above:

- the exact type boundary that makes committed lane projection updates
  mechanically unavoidable;
- the precise degraded-state representation and reconciliation trigger for
  `DurabilityOutcome::Unknown`;
- service behavior when startup cannot reconstruct durable lane state at all;
- whether performance counters live only in tests or also appear in runtime
  diagnostics;
- whether an O(pending) in-memory union remains sufficient once parked writes
  from #968 exist at production scale.

None of these requires choosing a database winner before implementation begins.
