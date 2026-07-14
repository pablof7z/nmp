# Bounded delivery, overload, and shortfall

- **Status:** TARGET CONTRACT - Swift snapshot coalescing and some router caps
  exist, but boundedness is not yet enforced as one end-to-end contract.
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

Query snapshots and diagnostics are latest-state streams. Intermediate
deliveries are not durable facts; a slow observer may skip them as long as the
next delivery represents the newest complete **local** state and includes the
current evidence/shortfall revision.

Each platform bridge therefore uses a bounded newest-value buffer and may frame
coalesce bursts. Cancellation/drop withdraws observation according to the query
refcount contract; it does not leave an unbounded producer queue.

"Complete local state" means all mutations incorporated through that revision.
It does not mean globally complete Nostr state.

## 3. Durable fact streams

Write receipt transitions are persisted facts. Observer buffering may be
bounded because a consumer can reattach and replay/inspect durable state. A
receipt implementation must not rely on an unbounded in-memory channel or lose
facts merely because no observer was attached.

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

Blocking native receiver drains use the zero-queue admission mechanism in
`native-task-executor.md`. They never enter a conventional worker queue:
capacity or OS-thread refusal is known before stream/write ownership transfers,
and one admitted task preserves its receiver's FIFO order.

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
- receipt observers may detach/restart without losing durable transitions;
- an oversized derived set either chunks exactly or reports shortfall;
- relay fan-out caps never masquerade as complete acquisition;
- a requested result limit is distinguishable from an engine-imposed limit;
- an overwhelming relay cannot grow memory unboundedly and leaves a diagnostic
  reason when disconnected;
- scheduler load remains bounded and fair without polling;
- native task saturation refuses before ownership transfer, and cancellation
  returns the join-backed census to its exact baseline without a timeout;
- no test can obtain silent first-N truncation at any limit boundary.
