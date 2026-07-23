# Bounded delivery, overload, and shortfall

- **Status:** PARTIAL - ordinary query, window, diagnostics, Swift, and Kotlin
  observation delivery are bounded; ingestion pressure and several graph,
  wire, cache, and result limits are not yet enforced end to end.
- **Owns:** observer buffering, durable fact delivery, graph/wire/result limits,
  ingestion backpressure, and explicit shortfall.

## 1. Governing rule

NMP may bound work, memory, fan-out, and delivery latency. It may never silently
truncate demand or present a bounded subset as a complete answer.

Every limit outcome is one of:

- semantics-preserving chunking or coalescing;
- cached/local results plus explicit shortfall evidence;
- typed rejection before work is accepted;
- backpressure or disconnection with a diagnostic reason.

Exact numeric defaults are implementation policy and remain provisional. Limit
categories and their observable consequences are architectural contract.

## 2. Latest-state observations

Windowed query snapshots and diagnostics are latest-state streams. Unwindowed
queries retain their incremental contract: unchanged rows are not redelivered,
but the producer may compose skipped reducer deltas into one exact transition
rebased onto the observer's last delivered state. Folding that transition must
produce the newest complete **local** state and it must carry the current
evidence/shortfall revision. Intermediate reducer deliveries are not durable
facts in either mode.

The Rust ordinary-row producer therefore holds one pending transition per event
id in a single mailbox slot, while windowed queries and diagnostics hold one
complete latest snapshot. Swift issues one native pull per app pull and owns no
second delivery queue; its snapshot iterators cadence-limit returns without
prefetching. Kotlin also pulls serially from the native handle. Cancellation or
drop withdraws observation according to the query refcount contract; it does
not leave an unbounded producer queue.

This bounds delivery backlog, not the semantic cardinality of an unwindowed
query result: the pending transition can still be proportional to the change
between the last delivered state and current state, and the app's accumulated
full result can still grow with the query. Apps requiring a bounded result set
use an explicit window.

"Complete local state" means all mutations incorporated through that revision.
It does not mean globally complete Nostr state.

## 3. Durable fact streams

Write receipt transitions are persisted facts. Observer buffering may be
bounded because a consumer can reattach and replay/inspect durable state. A
receipt implementation must not rely on an unbounded in-memory channel or lose
facts merely because no observer was attached.

Receipt and follow-action live delivery uses a fixed-capacity FIFO of 32 facts.
If a paused consumer falls behind, the producer retains the buffered prefix,
disconnects that sink, and the consumer receives typed `FactStreamLagged`
after draining it; later facts are never silently reported as delivered.
Receipt reattachment traverses the canonical persisted history in deterministic
pages of at most 32 delivery facts and attaches to live work only after the
final page. The replay cursor bounds each delivery page, not the store's total
retained attempt history: retention/GC for that durable history remains open
under #46 and must not be confused with retry-concurrency limits.

Diagnostics counters may aggregate, but exact current plan/filter/error facts
must remain available. Aggregation policy is itself visible in diagnostics.

## 4. Demand and wire limits

Limits may apply to graph nodes, derived-set cardinality, nesting, compiled
atoms, filters per relay, filter size, relay fan-out, concurrent connections,
and acquisition history.

- A derived set may be chunked only when unioning the chunks is semantically
  identical and local re-filtering remains exact.
- Coalescing may widen only; it cannot hide an unsupported filter.
- A relay fan-out cap produces explicit uncovered/shortfall evidence naming
  what the plan could not cover.
- Exceeding a hard graph or serialization limit returns a typed error or
  limited snapshot state, never first-N substitution.
- Limit application is deterministic for the same inputs so diagnostics and
  tests can reproduce it.

## 5. Result and cache bounds

Observation windows and store retention are distinct:

- a bounded result window does not advance or truncate source acquisition
  implicitly;
- presentation cursors cannot stand in for ingest/source cursors;
- GC may evict according to explicit policy, but must update cache evidence and
  cannot retain a completeness claim contradicted by eviction;
- pending durable writes and unresolved receipt facts are pinned until their
  owning contract permits removal.

An app may request a bounded selection. NMP must distinguish that requested
bound from an engine-imposed shortfall.

## 6. Ingestion and scheduler pressure

Transport feeds a bounded engine queue. When capacity is exhausted, NMP applies
backpressure where the transport permits it; otherwise it closes the offending
connection and records the reason. It does not grow memory without bound or
drop verified events invisibly.

One deadline scheduler arbitrates expiry, liveness, signer operations, durable
retry eligibility, and other real deadlines. It sleeps until the earliest
deadline, has explicit concurrency limits, and contains no fixed-rate poll loop
or per-intent thread.

Fairness policy must prevent one relay, query, or outbox lane from permanently
starving unrelated work. The precise algorithm is provisional; starvation and
queue pressure must be measurable.

Observation delivery is pull-based: a foreign consumer awaits `next()` on an
observation handle, which parks a waker on the engine-owned bounded mailbox
rather than blocking a dedicated OS thread (`async-observation-handles.md`,
#680). Live observations therefore do not consume a native thread each and
there is no app-visible native-task ceiling. Genuinely-blocking *transient*
foreign/reactor adapters (NIP-11 flights, remote-signer/AUTH waiters, the
follow-action worker) run on a fixed-capacity internal pool that ordinary
observations never touch; engine-associated NIP-46 sessions own their own
executor.

## 7. Diagnostics

Diagnostics exposes at least:

- configured/effective limits and current utilization;
- dropped intermediate observation-frame counts;
- graph, wire, relay, and result shortfalls;
- queue pressure, backpressure, and forced disconnect reasons;
- scheduler backlog and next eligible deadlines;
- any aggregation or history-retention boundary.

These are local mechanism facts, not a synthesized health score.

## 8. Falsification

Required proofs include:

- a burst into a slow Swift/Kotlin observer has bounded memory and eventually
  yields the latest exact local state;
- a paused receipt observer across more retry transitions than the live FIFO
  can hold receives typed lag, retains a finite prefix, and can traverse the
  canonical durable history through bounded pages without silent loss;
- an oversized derived set either chunks exactly or reports shortfall;
- relay fan-out caps never masquerade as complete acquisition;
- a requested result limit is distinguishable from an engine-imposed limit;
- an overwhelming relay cannot grow memory unboundedly and leaves a diagnostic
  reason when disconnected;
- scheduler load remains bounded and fair without polling;
- opening thousands of live observations creates O(1) engine threads, not one
  per observation, and no operation is refused for a native-task-capacity reason
  (#680); a parked `next()` wakes deterministically on value, close, cancel, or
  shutdown;
- no test can obtain silent first-N truncation at any limit boundary.
