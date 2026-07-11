# Threading, bounded delivery, and app lifecycle

**Status: CURRENT + TARGET.** The Rust engine uses blocking receive loops and
dedicated FFI drain threads. Swift query and diagnostics delivery is already
frame-coalesced with newest-value buffering. End-to-end bounded engine queues
and durable receipt replay are target work.

After this chapter you will know where values arrive, which observations may
coalesce, and why NMP requires no scene-phase integration.

## Current execution path

The runtime currently uses:

- one engine thread owning `EngineCore` and blocking on `recv()`;
- a transport bridge that forwards connection/frame events;
- a dedicated blocking drain thread for each FFI observer; and
- native reactive adapters such as Swift `AsyncSequence`.

No app code runs on the engine thread. SwiftUI consumers still update UI state
on the main actor:

```swift
.task {
    let query = try engine.observe(filter)
    for await snapshot in query {
        rows = snapshot.rows
    }
}
```

If a detached task consumes the sequence, it must use `MainActor.run` before
mutating UI-bound state. Kotlin will expose the same value stream as `Flow`; the
app chooses its UI collection scope and dispatcher.

## Query and diagnostic observations are latest-state streams

A query snapshot represents the newest complete **local** state incorporated
through its revision. It is not a durable log and it is not a claim about all
of Nostr. Intermediate deliveries may be coalesced when producers outrun the
consumer.

The current Swift bridge applies every row delta to its accumulator, then uses
`FrameCoalescer` and `AsyncStream.bufferingNewest(1)`. A slow UI may skip
intermediate frames but eventually receives the latest exact local rows and
evidence. Diagnostics uses the same Swift policy and a latest-wins Rust mailbox.

Current Rust row channels before the Swift bridge are still unbounded. The
target contract carries bounded newest-state delivery through every supported
facade, including Kotlin, rather than relying on a platform-specific final
adapter.

## Receipt observations are durable facts

Receipt transitions differ from query frames. `Accepted`, signer waiting,
signature promotion, attempts, ACKs, rejections, cancellation, expiry, and
at-most-once ambiguity are facts that must survive observer loss and restart.

The current receipt path uses an in-memory channel and streams each status in
this process. The target path persists receipt history/state and lets an
observer reattach by receipt id. Its in-memory delivery can then be bounded
because durable facts remain queryable; a full queue is never permission to
forget them.

## Backpressure belongs inside the engine boundary

Transport input, graph compilation, observer delivery, signer work, and retry
scheduling all need explicit limits. When pressure exceeds a limit, NMP must
backpressure, coalesce exact latest-state observations, reject with a typed
error, or disconnect an overwhelming source with a diagnostic reason. It must
not grow an invisible unbounded backlog or silently drop verified events.

See [Bounded delivery, overload, and shortfall](../design/bounded-delivery.md).

## Lifecycle is ownership, not an NMP framework

NMP requires no `onForeground`, scene-phase hook, provider container, or
background-task registration. Query demand exists while its handle/iterator is
owned. Dropping the final owner withdraws demand; explicit `cancel()` is only an
early teardown option.

If the OS suspends the process, app tasks cannot execute. If sockets are lost,
the transport reconnect path re-establishes still-live demand when execution
resumes. If the process is terminated, the app constructs the engine and
declares its queries again on next launch; persisted cache, source evidence,
pending writes, and receipts restore engine-owned durable state according to
their contracts.

The app chooses observation scope using ordinary SwiftUI task ownership,
Kotlin coroutine scope, or Rust `Drop`. NMP does not ask the app to mirror a
subscription lifecycle or keep an expanded relay/author set alive.

---

<!-- nav-footer -->
<sub>← [Diagnostics](22-diagnostics.md) · [Index](README.md) · [Cost & performance](24-performance.md) →</sub>
