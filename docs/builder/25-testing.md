# Testing an app that embeds NMP

Most application tests should not start a relay or reach through NMP's FFI
layer. Test app-owned state transitions with plain public snapshot, row,
receipt, and diagnostic values; reserve a smaller integration tier for the real
engine.

The public test-support package is still provisional. Do not build application
tests around `@testable import`, generated FFI observer protocols, or mechanism
crates.

## Test the app's fold as app code

NMP snapshots are values. A reducer or view model can consume a scripted
sequence without knowing how the engine produced it:

```swift
let first = QuerySnapshot(
    rows: [row(id: "a", kind: 9999)],
    cache: .persistent,
    acquisition: [.connecting(relayA)],
    shortfall: [])

let second = QuerySnapshot(
    rows: [row(id: "a", kind: 9999), row(id: "b", kind: 9999)],
    cache: .persistent,
    acquisition: [.reconciled(relayA, through: timestamp)],
    shortfall: [.sourceUnavailable(relayB)])

model.apply(first)
model.apply(second)

XCTAssertEqual(model.visibleIDs, ["a", "b"])
XCTAssertEqual(model.unavailableSources, [relayB])
```

The spelling is illustrative. The important boundary is that the test asserts
the app's interpretation of local rows and scoped evidence. It never fabricates
`synced`, `healthy`, or global completeness.

If a feature directly consumes an asynchronous observation, the app may inject
its own narrow interface or async sequence. That is an ordinary application
testing decision, not an NMP container:

```swift
protocol LibrarySnapshots {
    func snapshots() -> AsyncStream<QuerySnapshot>
}
```

Keep this interface shaped around the feature's needs. Do not mirror the entire
NMP facade merely to make a large mock.

## Script receipt facts, not a success boolean

A write-facing feature should be tested against the transitions it presents:

```text
Accepted
AwaitingSigner(pubkey)
Signed(eventId)
Routed(relayA, relayB)
Acked(relayA)
Rejected(relayB, reason)
```

Useful app tests include:

- the pending row renders immediately after durable `Accepted`;
- signer absence is presented without hiding or rolling back the row;
- one relay ACK is not displayed as universal convergence;
- an at-most-once `OutcomeUnknown` is not offered a blind retry button;
- detaching the UI does not imply cancellation; and
- reattaching to a receipt reconstructs the same durable facts.

The app does not inject a row through a write mock. Its test sequence models the
store snapshot and receipt as independent observations, matching the production
ownership boundary.

## Deterministic engine integration

NMP needs a supported test surface that can instantiate the canonical facade
with:

- an in-memory or temporary persistent store;
- an injected clock and deterministic randomness where time/jitter matter;
- scripted relay and signer capabilities;
- bounded observation channels; and
- the same validation, resolver, router, and outbox code used in production.

That surface must remain a test harness, not a public mechanism-assembly API.
Apps should be able to drive relay frames, signer results, disconnects, AUTH
challenges, and clock advances without importing FFI records or constructing an
`EngineCore` themselves.

Until this target surface lands, treat internal SDK fakes as repository tests,
not a stable consumer contract. The honest shipping state is tracked in
[Current implementation status](03-status-map.md).

## Diagnostics make good golden evidence

The permanent diagnostic snapshot is a structured projection, so it is useful
for deterministic assertions:

- per-relay subscription count;
- exact wire-filter JSON;
- lane and reverse-coverage counts;
- events received by relay and kind;
- source status, EOSE, AUTH, errors, and watermarks; and
- explicit graph, route, or result shortfall.

Golden tests should compare semantic records or canonical JSON, not screenshots
of a health score. NMP deliberately exposes no health score.

## Crash and restart proofs belong to NMP

An app fake cannot prove the durable write contract. NMP's owning test suites
must kill and reopen the real persistence boundary at every relevant point and
prove:

- failed acceptance exposes no pending row or `Accepted` receipt;
- successful acceptance restores the frozen body, pending row, receipt, signer
  identity, routing state, and displaced candidates together;
- signer detachment and reattachment resume the same obligation;
- signature promotion preserves exact body, author, id, and valid signature;
- cancellation, deletion, expiry, and replaceable supersession cannot strand an
  open intent;
- attempts are persisted before dispatch and retain ordinal/eligibility;
- at-most-once ambiguity is never blindly resent; and
- receipt facts remain reattachable after open work becomes terminal.

These are facade-level contract tests, not tests of table helpers in isolation.

## Bound every asynchronous test

Tests must never poll with `sleep` and check loops. Use the platform's timeout
or task-race primitive so a missing event fails with a bounded diagnostic.

A small live-relay tier may prove the real network path. Keep it separate from
deterministic CI evidence, cap every wait, and report an unavailable public
relay as an environmental skip rather than training the suite to tolerate
random failures. Live tests supplement, but never replace, scripted transport
and crash-recovery falsifiers.

## Suggested test matrix

| Tier | What it proves | Network |
|---|---|---|
| App fold | Product interpretation of public values | none |
| Scripted async | Observation and receipt UI behavior | none |
| Facade integration | Real store, graph, routing, signing, and cancellation | deterministic harness |
| Crash/restart | Atomicity and durable recovery | local process/database |
| Platform parity | Rust behavior projects identically to Swift and Kotlin | none |
| Live smoke | Packaging through a real relay | bounded, optional |

The testing rule matches the architecture rule: test through the narrow public
contract, and keep NMP's internal machinery out of application code.

---

<!-- nav-footer -->
<sub>[Index](README.md) · [Troubleshooting](26-troubleshooting.md) →</sub>
