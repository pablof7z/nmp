# Threading, bounded delivery, and app lifecycle

NMP owns its interior concurrency. An app observes native asynchronous values
and uses its platform's ordinary UI and task scopes. It does not run an engine
loop, poll state, or forward scene-phase events.

## No app code runs in the reducer

The Rust runtime serializes engine decisions over explicit messages and typed
effects. Network frames, clocks, signer results, AUTH callbacks, and other
nondeterministic inputs enter through explicit capability boundaries.

App callbacks and presentation code do not execute inside the engine reducer.
A SwiftUI consumer still follows normal main-actor rules:

```swift
.task {
    for await snapshot in try engine.observe(demand) {
        rows = snapshot.rows
    }
}
```

Kotlin consumers collect a `Flow` in their chosen coroutine scope. Rust callers
use the facade's observation type. Platform projections may differ in syntax,
not in ownership or ordering semantics.

## Query and diagnostic observations are latest state

A query delivery is a replaceable snapshot of the newest complete **local**
state incorporated through its revision. Diagnostics is likewise a recomputed
projection. When a consumer is slower than the engine, intermediate snapshots
may coalesce as long as the consumer eventually receives the newest exact state.

This is not permission to skip ingestion or row mutations internally. NMP must
incorporate every accepted change before producing the later snapshot. It is
only permission to avoid an unbounded observer backlog of states that have
already been superseded.

The snapshot says nothing about complete global Nostr state. It carries local
rows and scoped source evidence.

## Receipt transitions are durable facts

Receipt facts are not disposable UI frames. Acceptance, signer waiting,
signature promotion, route revisions, attempts, ACKs, rejections, cancellation,
expiry, and ambiguity must remain inspectable after observer loss and restart.

In-memory notification may still use bounded latest delivery because the
durable receipt can be re-read by id. A full observer queue is never permission
to forget the underlying transition.

Explicitly non-durable publication weakens delivery-resume behavior, not receipt
observability. After process loss its receipt reattaches to an explicit policy
terminal such as abandonment or unknown handoff; NMP does not silently erase it.

## Backpressure remains inside the boundary

Transport ingestion, graph compilation, signer work, observation delivery,
retry scheduling, and retained history all need explicit limits. At a limit NMP
must do one of four honest things:

- apply backpressure;
- coalesce superseded latest-state observations;
- reject or report explicit shortfall; or
- disconnect an overwhelming source with a diagnostic reason.

It must not grow an invisible unbounded queue, silently truncate a demand, or
drop a verified event without accounting for the loss. See
[Cost, coalescing, and boundedness](24-performance.md).

## Lifecycle follows ownership

Demand exists while at least one observation owner exists. Dropping the final
owner withdraws demand; an explicit cancel operation is an early teardown tool,
not a required application lifecycle protocol.

If a socket disconnects, transport reconnects and restores the wire work for
still-live demand. If the operating system suspends the process, no app task can
run and NMP requires no foreground polling callback. After process termination,
the app constructs its engine and declares its queries again; the engine
restores its persistent replica, source evidence, accepted write obligations,
attempts, and receipts.

The app may keep the engine in its own model, dependency container, or service
registry. Normal teardown is resource ownership: dropping the facade must close
and join interior workers safely. Calling an explicit shutdown method must not
leave a public object whose remaining methods panic or silently disconnect.

Relay-information acquisition follows the same owner. Each distinct NIP-11
flight reserves one immediately runnable slot from the engine's finite native
executor; there is no separate worker pool or accepted-work queue. Same-relay
callers join a finite waiter set. Shutdown closes that set with a typed terminal
result, cancels Hickory DNS or HTTP body work, and joins the admitted task even
if a cloned engine handle, live subscription, or cancellation token survives.
The independent 250 ms capability-decision grace remains an engine-loop
deadline, so a slow HTTP endpoint cannot hold the WebSocket NIP-77 fallback.

Hickory DNS resolution for this acquisition path is runtime-qualified on iOS,
not only compile-qualified: a repo-owned iOS Simulator test host (`apps/
Falsifier`'s `FalsifierTests` target) executes a hostname NIP-11 fetch
through `NMPEngine.relayInformation(for:policy:)` on an actual iOS Simulator
process as a required CI gate, proving the governed resolver initializes and
resolves inside the iOS runtime with no blocking-GAI fallback
([#465](https://github.com/pablof7z/nmp/issues/465)).

## Current implementation

Some platform adapters already coalesce newest snapshots, while end-to-end
bounded queues and durable receipt replay are still being completed. Use
[Current implementation status](03-status-map.md) rather than inferring shipping
behavior from the target contract above.

---

<!-- nav-footer -->
<sub>[Index](README.md) · [Cost and boundedness](24-performance.md) →</sub>
