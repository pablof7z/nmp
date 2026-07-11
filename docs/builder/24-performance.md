# Cost, coalescing, and boundedness

After this chapter you will know what an observation costs, when demand can
share, and how NMP reports a limit without silently changing the query.

## Pay for declared demand

NMP has no ambient content firehose. A live query costs:

1. binding resolution and recompilation when inputs change;
2. compiled wire demand against planned sources;
3. canonical store matching and snapshot production; and
4. compact evidence plus optional diagnostic detail.

Protocol modules add only the code and semantic operations an app enables. The
core remains usable without a preferred content module.

## Sharing follows semantic compatibility

NMP may refcount identical demand and widen wire filters only under a proved
rule such as author union. The semantic descriptor is:

```text
Demand = Selection + SourceAuthority + AccessContext
```

Equal selections may share local matching and binding-resolution work. Wire
demand and acquisition evidence may share only when source authority and access
context are compatible. Two equal filters under different AUTH contexts must
not accidentally borrow each other's proof.

Apps should declare the demand each view needs. They do not need to pass one
shared query object around merely to save wire subscriptions; the engine owns
deduplication, coalescing, reference counts, and last-observer teardown.

## Latest-state delivery is bounded

Every platform projection incorporates engine mutations into the latest local
state, then bounds observer delivery to the newest snapshot. The app does not
need to implement its own debounce to prevent historical replay from building a
render backlog.

Skipping an intermediate query or diagnostic frame is safe because the next
frame supersedes it and contains the newest complete local state. It would not
be safe to drop a durable receipt fact unless that fact was already persisted
and replayable, which is why receipt durability is a separate contract.

## Limits must be explicit

NMP may cap relay fan-out, connections, graph depth, derived cardinality, wire
filter size, observation windows, scheduler concurrency, and retained history.
Every cap must choose one honest outcome:

- exact semantics-preserving chunking/coalescing;
- cached/local results plus explicit shortfall evidence;
- typed rejection before acceptance; or
- backpressure/disconnection with a diagnostic reason.

It may never silently take the first N values and present them as the whole
result. A caller-requested `limit` is also distinct from an engine-imposed
shortfall.

## Protocol code is opt-in and exact

An enabled NIP module contributes only the exact schemas, builders, parsers,
queries, operations, and context facts defined by that protocol. It does not
bring a preferred timeline or broad content category into core. A minimal app
that enables no protocol module still has raw live query and write intent.

The exact Cargo, SwiftPM, and Kotlin packaging is provisional. Whatever shape
lands must preserve one invariant-enforcing facade and pay-for-what-you-enable
without a module-registration lifecycle in the app.

## Cost summary

| Action | Cost / bound |
|---|---|
| Observe compatible demand twice | shared resolution/wire work; two native observers |
| Drop the final observer | demand withdrawal and debounced wire close |
| Receive a large replay in Swift | every delta incorporated; newest snapshots frame-delivered |
| Hit a router/graph/result cap | explicit shortfall, exact chunking, or typed rejection |
| Enable a protocol module | only that protocol's semantic surface and dependencies |
| Detach from a target receipt | no write cancellation; persisted facts remain reattachable |

The performance rule is the correctness rule: bound work explicitly, coalesce
only superseded state, and expose every semantic shortfall.

See [Current implementation status](03-status-map.md) for the bounds proven by
the shipping projections today.

---

<!-- nav-footer -->
<sub>← [Threading & lifecycle](23-threading-lifecycle.md) · [Index](README.md) · [Testing](25-testing.md) →</sub>
