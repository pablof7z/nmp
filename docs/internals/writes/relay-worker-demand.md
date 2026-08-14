---
title: Durable write relay-worker demand
category: writes
slug: relay-worker-demand
status: built
date: 2026-07-28
audience: llms
scope: publish queue lane projection into relay-worker ownership
investigated_revision: 057f59ef9f3a5cbc1a3ce4e3c88d189d55b28244
runtime_evidence_revision: 9a8ad1fd1a2999b067c19d842467a76a257f7f25
implementation_pr: https://github.com/pablof7z/nmp/pull/988
owns:
  - how publish queue lanes determine which relay sessions the write plane must keep alive
  - why the investigated implementation repeatedly read and decoded every lane
  - the rebuildable in-memory projection that removes those reads
  - the projection's ordering, restart, and degraded-state requirements
related:
  - docs/design-record.md
  - docs/bug-class-ledger.md
  - docs/known-gaps.md
  - docs/internals/writes/publish-queue.md
issues:
  - https://github.com/pablof7z/nmp/issues/1027
  - https://github.com/pablof7z/nmp/issues/985
  - https://github.com/pablof7z/nmp/issues/1000
  - https://github.com/pablof7z/nmp/issues/975
  - https://github.com/pablof7z/nmp/issues/968
  - https://github.com/pablof7z/nmp/issues/771
  - https://github.com/pablof7z/nmp/issues/904
---

# Durable write relay-worker demand

This document records the repeated delivery-lane scans present at
`investigated_revision` and the reducer-owned replacement built in
[#988](https://github.com/pablof7z/nmp/pull/988). It is deliberately not a
backend-selection record. The performance problem was caused by asking durable
storage to reconstruct already-known reducer state during ordinary dispatch;
changing the database while preserving that access pattern would have left the
architectural mistake intact.

The design is tracked by
[#985](https://github.com/pablof7z/nmp/issues/985). The source sections below
label the former behavior explicitly. The replacement is **built and
verified** on the implementation PR; future route-revision and parked-write
behavior remains owned by #975 and #968 rather than being claimed here.

## The questions that led here

Pablo first asked:

> is this related to the work we were doing in this session? is nmp still storing events as json and deserializing on every scan?

At `investigated_revision`, the precise answer was:

- canonical Nostr events are stored in a compact binary encoding;
- delivery-lane control records were stored as JSON values under string keys;
- the hot path scanned and JSON-decoded those lane records repeatedly;
- therefore this is related to storage representation work, but it is not
  evidence that canonical events are stored as JSON.

Current schema version 12 replaces that physical shape with the explicit
binary `publish_queue_*` namespace documented in
[`publish-queue.md`](publish-queue.md). The reducer-owned projection
remains the architectural fix for repeated dispatch reads; binary makes
recovery and any remaining scan cheaper, but does not make a scan unnecessary.

The design question was:

> how do you propose we reduce the repeated scans?

The answer is to make relay-worker demand an explicit, reducer-owned,
rebuildable projection of durable lane state. Durable storage remains the
authority. The projection answers the frequent scheduling question without
re-reading the authority every time.

## System model

A durable write intent can own one delivery lane per destination relay. A lane is
the durable state machine for delivery to that relay: waiting, eligible,
attempting, retrying, suspended, or terminal. Nonterminal lanes require NMP to
keep the corresponding authenticated relay session available. Terminal lanes
do not.

The relevant ownership split is:

```text
publish queue tables
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

## Previous behavior: reconstructing demand during dispatch

`EngineCore::relay_worker_requirements` combines read and write demand.
`EngineCore::write_relay_workers` computes the write half.

At `investigated_revision`, `write_relay_workers`:

1. starts with relay sessions referenced by in-flight attempt correlations;
2. walks every `PendingWrite`;
3. includes the pending write's already-known pending, unstarted, and
   route-blocked relays;
4. calls `RedbStore::recover_publish_queue_lanes(intent_id)` for every pending intent;
5. range-scans the durable lane table, JSON-decodes every returned lane, and
   parses its relay URL;
6. retains the relay session for every nonterminal lane.

`runtime::dispatch_core_effects` calls this worker-requirement calculation
before dispatching effects. Worker retirement and relay-state pruning also ask
for the requirements. An unchanged reducer can therefore perform the same
store reads, B-tree comparisons, JSON decoding, and URL parsing many times.

This was not recovery in the operational sense even though the store method is
named `recover_publish_queue_lanes`. Recovery data was being used as a recurring
query interface.

### What is binary and what is JSON

Canonical event insertion calls `binary_event::encode_event` and stores the
resulting bytes. The representative ingest schema and its write amplification
are a separate concern.

At the investigated revision, delivery lanes used:

```rust
TableDefinition<&str, &str>
```

The key contained the intent identifier and relay URL. The value was a
JSON-encoded lane. `recover_publish_queue_lanes` performed a key-range scan, decoded
every matching value with `serde_json`, reconstructed relay URLs, and sorted
the result. Current code uses `intent:u64-be | relay_id:u32-be` and an explicit
versioned binary value.

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
`recover_publish_queue_lanes`, about 61% to `required_relay_workers`, about 31% to
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

## Built behavior: a rebuildable projection

Each `PendingWrite` now owns a `LaneWorkerProjection` with three sets:

- `persisted`: every persisted lane learned for the intent, including terminal
  lanes needed by the reverse wake index and exact removal cleanup;
- `nonterminal`: durable lanes whose latest committed state still requires
  write-plane work;
- `uncertain`: conservative ownership after an indeterminate commit outcome.
  This set may temporarily over-retain a worker but may never under-retain one.

The reducer populates the projection from the lanes returned by startup
recovery and bootstrap. Every successful durable lane mutation then feeds its
returned post-commit `PublishQueueLane` through one projection update path:

```text
committed lane state is nonterminal -> insert relay
committed lane state is terminal    -> remove relay
```

`write_relay_workers` is now a pure in-memory union of:

- the projected nonterminal lane relays;
- unstarted or conservatively route-blocked relay ownership that has not yet
  become a lane;
- sessions referenced by in-flight attempt correlations.

It maps each relay to a `RelaySessionKey` using the write's signing identity and
NIP-42 access context. The URL alone is not an identity: the same relay URL
under Public access or different authenticated accounts must not be
accidentally merged.

### One mutation boundary, not remembered call sites

The store API already returned post-commit lane state from bootstrap and lane
transitions, but many former call sites discarded those returned values.
`core/lane_projection.rs` now owns wrappers for bootstrap and every lane-writing
store door. Each wrapper commits first and applies the returned
`PublishQueueLane` before returning to ordinary reducer flow.

A recursive source-census falsifier scans every production module under
`core/` and fails if any module outside `lane_projection.rs` invokes a raw
lane-writing store method. This combines an internal API boundary with a
mechanical proof rather than relying on reviewer memory. The rule is:

> Any code path that makes a lane durably visible must also pass the committed
> lane state through the reducer projection before ordinary dispatch resumes.

The durable store remains the sole retry authority. The projection is derived
state rebuilt from complete bootstrap results; it is not a second delivery authority.

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

The implementation keeps the simple reducer-local union. A global
relay-session reference-count projection remains a possible follow-up only if
the in-memory walk itself becomes material under the future parked population.

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

Closing an intent is special only in shape, not authority.
`close_if_all_lanes_terminal` now asks whether the projection has at least one
persisted lane and no nonterminal, uncertain, or route-blocked ownership. It
then calls `close_terminal_intent` without a preceding lane scan. The store
still validates transactionally that every lane is terminal, so the projection
only decides that a close attempt is plausible; the store remains the final
authority and refuses an invalid close.

### Startup

Before ordinary query/effect service, startup reconstructs pending writes and
their exact nonterminal-lane sets from durable state. Once reconstruction
succeeds, unchanged dispatch must not consult lane storage merely to calculate
worker demand.

Restart equality is semantic, not a row-count check: reconstructed projection,
ordered worker requirements, lane states, and access identities must equal the
pre-close durable meaning.

### Persistence failures

The projection wrappers use the durability classification introduced by #904:

- `DurabilityOutcome::Absent` means the transition is known not to have
  committed. Keep the old projection.
- `DurabilityOutcome::Unknown` means the transition may be absent or durable.
  The reducer cannot safely pretend either outcome is known.

An unknown terminal transition retains the previously required worker until
reconciliation. An unknown lane-creation transition is harder: retaining the
old projection is insufficient because the possibly durable lane may be new.
Bootstrap therefore records every candidate relay as `uncertain`; the
individual transition wrapper records the exact lane key as uncertain.

The degraded projection must be a conservative superset of possibly required
sessions. It may temporarily keep an unnecessary worker. It must not drop a
worker for a lane that may have committed.

Falling back to a full lane scan on every dispatch would recreate the original
problem and is not an acceptable degraded mode. When the reducer cannot prove
even a conservative projection, worker reconciliation returns `None` and the
runtime retains its existing workers.

### The way out of retention

Conservative retention is only safe if it is temporary. The first #985
implementation had no exit:
[#1000](https://github.com/pablof7z/nmp/issues/1000) found that `uncertain` is
cleared solely by a committed `PublishQueueLane` for that exact relay, while an
intent whose bootstrap failed owns no lane rows at all — so `schedule_ready`,
the deadline sweep and the wake index all find nothing for it and no committed
lane fact can ever arrive. Its relay workers stayed pinned and its receipt
stayed in `pending` for the life of the process.

A failed bootstrap therefore records a **retryable gap** for that intent
(`EngineCore::lane_bootstrap_retries`). The gap:

- carries the route candidates held as `uncertain`, or `None` when the
  durable route set itself could not be read;
- arms a deadline through the existing `next_deadline`/`Tick` machinery, with
  the same capped exponential backoff shape as the lane retry schedule, so
  nothing new scans and steady state pays one empty-map probe;
- is closed by exactly one event — a committed `bootstrap_publish_queue_lanes`, whose
  exact rebuild supersedes every conservative guess it stood in for — or by
  the pending write leaving `pending`.

Projection availability is derived from that state rather than latched:

```text
available = not unprovable
            and every outstanding gap names at least one candidate relay
```

`lane_projection_unprovable` covers only gaps that no in-process
reconciliation can close: a committed lane fact for an intent this reducer
does not track, or a boot that could not read the pending set at all. Those
still wait for the next `recover_on_boot`. A gap with known candidates is
already covered by `uncertain` and keeps the projection available, so exact
worker reconciliation — including #598's cap-refused worker retry — keeps
working while the gap stands.

A lane set established late is indistinguishable from one established at boot,
so the retry drives it through the same door boot uses, and additionally
replays the wake for any session that is already connected: mid-process, the
`RelayConnected` a fresh `WaitingConnection` lane is waiting for may already
have happened.

[#771](https://github.com/pablof7z/nmp/issues/771) owns the broader contract for
preserving durable lane-transition evidence when store operations fail. Its
older premise that an error always means “committed none” is superseded for
`DurabilityOutcome::Unknown`. #985 integrates only the worker-retention part of
that contract; #771 still owns complete durable transition evidence and
user-visible recovery policy.

If startup cannot read enough durable state to construct even a conservative
projection, there is no prior worker set to retain. The correct service-level
behavior for that case remains an open recovery decision owned with #771 and
#904; this document does not claim it is solved.

## Invariants

The implementation is correct only if all of these hold:

1. Durable lane state is the authority; the in-memory projection is
   reconstructible and never independently persisted as delivery truth.
2. After successful reconstruction, worker-demand calculation performs no
   lane-store read.
3. Every committed lane creation or transition updates the projection before
   ordinary effect dispatch resumes.
4. A relay is present in `LaneWorkerProjection::nonterminal` exactly when its
   known committed lane is nonterminal; `uncertain` may only over-retain.
5. A persistence outcome classified Unknown never causes a possibly required
   relay worker to be dropped.
5b. Every conservative retention has a path out of it that does not require
   another process: a store that becomes usable again must return the
   projection to what a canonical rebuild yields, without any per-dispatch
   lane scan.
6. Public, NIP-42, account, and session identities do not alias merely because
   their relay URLs match.
7. Future route revision must use the same lane-minting/projection boundary as
   initial bootstrap.
8. A future parked intent with no lane must contribute no worker and cause no
   per-dispatch store read.
9. The store transaction, not the projection, is the final authority for
   terminal intent closure.
10. Close/reopen reconstruction produces the same semantic projection and
    ordered worker requirements as the durable pre-close state.

## Falsifiers and performance evidence

The implementation carries these permanent falsifiers:

- `unchanged_worker_demand_reads_zero_delivery_lanes` first failed on the base
  revision with 6 recovery reads for 3 pending intents, then passed with 0.
  It also proves that the required worker is still recognized; eliminating
  reads by forgetting demand would fail.
- `projection_matches_durable_state_after_every_normal_delivery_transition`
  compares projected worker sessions with an independent store reconstruction
  after bootstrap, connection/auth wake, attempt start, handoff, and terminal
  ACK.
- `same_url_keeps_distinct_signing_identities_in_worker_demand` proves 2
  NIP-42 identities at one URL remain distinct and do not alias the Public
  session.
- `close_reopen_rebuilds_the_same_exact_worker_projection` compares exact
  worker sets before close and after redb reopen/boot recovery.
- `durability_unknown_marks_the_lane_uncertain_and_retains_its_worker` proves
  an indeterminate transition cannot drop the possibly durable worker.
- `durability_absent_leaves_the_exact_projection_unchanged` proves a rejected,
  known-absent transition cannot fabricate projected state.
- `every_core_lane_mutation_uses_the_projection_door` recursively scans the
  production core source and rejects a raw lane-writing store call outside the
  projection module.
- `transient_redb_bootstrap_failure_is_fully_reversible` drives one
  construction-armed Redb pre-commit refusal through recovery to a terminal
  receipt and proves the worker set returns to the canonical durable oracle.
- `an_unresolved_bootstrap_keeps_retaining_and_backs_off` proves the fix did
  not buy that exit with under-retention: while a real persisted attempt row
  remains undecodable, every route candidate stays owned and the retry backs
  off instead of spinning.
- `corrupt_route_lane_evidence_is_unreadable_not_absent` proves a real
  persisted route-revision schema violation refuses the boot projection,
  fabricates no receipt fact, and retains the parent durable obligation.
- `persistent_engine_recovers_latched_store_and_resolves_ambiguous_acceptance_once`
  and `persistent_engine_recovers_after_precommit_acceptance_io_once`
  separately prove the reopen-required case: a real Redb handle is closed,
  the runtime supervisor installs a new generation, reconstructs the existing
  public `Engine`, and accepts later work. They prove handle-generation
  recovery, not a route-specific same-handle heal.

Future #975 route-revision minting and #968 route-parking still require their
own positive falsifiers when those states exist. The projection boundary is
already the bootstrap/lane-minting boundary they must use, but this PR does not
claim unimplemented states were tested.

### Measured magnitude

`relay_worker_projection_redb_benchmark` exercises the real redb lane
representation and the same `RelayOpenFailed` ownership path used by the
zero-read regression. Its fixed workload has 64 pending intents and 200
unchanged ownership passes.

Five release-mode samples were run from separate clean Cargo target
directories for the base and candidate, preventing cross-worktree artifact
reuse:

```text
base 057f59e median:       51,624 microseconds
projection median:         5,162 microseconds
median reduction:             90%
median speedup:                10x
```

The behavioral count moves from reads proportional to pending durable writes
to exactly 0 lane-store reads during worker calculation. The benchmark keeps
relay-worker outcomes equal and measures the actual redb representation, so the
speedup did not come from dropping work or replacing redb.

This is strong evidence for the isolated hot path, not a claim that the whole
Mosaico process is 90% faster. A production rollout should still compare
process CPU, engine-thread CPU, command latency, and facade wait time against
the original workload.

## Backend independence and non-goals

Pablo explicitly warned:

> just fyi, I never "chose" redb, if that's biting us we can 100% consider using something else, ok?

and:

> I think the redb was choosen at some point because wasm is a target for nmp, but not sure

This design does not privilege redb. Any backend used by NMP should expose the
same semantic lane-transition and recovery contract. Native and WASM targets
do not need to use the same physical engine.

The first #985 implementation intentionally did not:

- select or migrate a production database;
- change the canonical event representation;
- replace the legacy execution JSON or string keys (now closed by #1027);
- use unchecked UTF-8 to reduce comparison cost;
- redesign the public engine mutex;
- add a speculative global worker reference-count cache;
- implement #975's routing policy or #968's parked-write lifecycle.

Those can be evaluated independently after the repeated read amplification is
removed and re-profiled.

## Source guide

- `crates/nmp/src/core/mod.rs`
  - `PendingWrite`
  - `LaneWorkerProjection`
  - `EngineCore::relay_worker_requirements`
  - `EngineCore::write_relay_workers`
  - relay-state pruning and wake indexes
- `crates/nmp/src/core/lane_projection.rs`
  - the one projected bootstrap/transition boundary
  - exact rebuild, uncertainty, retryable-gap registration, and the recursive
    bypass falsifier
  - store-oracle, identity, restart, and failure tests
- `crates/nmp/src/core/lane_bootstrap_retry_tests.rs`
  - the retention-with-an-exit falsifiers
- `crates/nmp/src/core/write.rs`
  - durable lane lifecycle call sites using the projection boundary
  - `retry_lane_bootstraps` and the shared post-bootstrap lane opening
  - store-validated terminal intent closure
- `crates/nmp/src/runtime/mod.rs`
  - `dispatch_core_effects`
  - worker retirement and retry
- `crates/nmp-store/src/lib.rs`
  - backend-independent lane transition and recovery API
  - persistence-error durability classification
- `crates/nmp-store/src/redb_store/schema.rs`
  - redb lane table representation
- `crates/nmp-store/src/redb_store/delivery.rs`
  - binary lane/deadline mutation helpers
- `crates/nmp-store/src/redb_store/publish_queue_codec.rs`
  - explicit versioned values, bounds, and fixed-width key constructors
- `crates/nmp-store/src/redb_store/publish_queue_ops.rs`
  - lane range scans, transition commits, and terminal closure
- `crates/nmp-store/src/redb_store/canonical.rs`
  - binary canonical-event insertion
- `crates/nmp/tests/core_headless/persistence_failures.rs`
  - the red/green zero-read falsifier
  - the repeatable redb before/after benchmark

## Open decisions

The following adjacent decisions remain without weakening the built invariants:

- service behavior when startup cannot reconstruct durable lane state at all;
- whether performance counters live only in tests or also appear in runtime
  diagnostics;
- whether an O(pending) in-memory union remains sufficient once parked writes
  from #968 exist at production scale.

None requires choosing a database winner or reverting to per-dispatch scans.
