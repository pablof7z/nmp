# Troubleshooting from evidence

Debug NMP by reading structured state, not by inferring a global health verdict.

## Empty local rows

An empty row array says only that the canonical local store currently has no
matching row. Read these facts in order:

| Read | Meaning |
|---|---|
| No planned relays | The descriptor resolved to no routable wire work. Inspect bindings and source authority. |
| `uncoveredAuthorCount > 0` | Current router facts/cap could not place part of the author demand. |
| Exact wire filters | Confirm the compiled selection is what the app declared. |
| Connection/AUTH state | A planned source may be offline, connecting, challenged, or rejected. |
| Per-relay watermark/EOSE | That relay finished that request window; not global completeness. |
| `eventsByKind` | Shows what this engine actually received from each relay. |
| Limit/shortfall | Distinguishes caller-requested bounds from engine-imposed limits. |

If events arrived and canonical matching rows exist but the UI is empty, inspect
the app's fold, sort, and presentation code. Do not conclude "nothing exists"
from an empty cache or a scoped relay watermark.

## A publish that is not progressing

Reattach the receipt by its stable receipt id. Its facts distinguish acceptance,
signing, routing, attempts, ACK, rejection, terminal policy, and failure.

- `AwaitingSigner(pubkey)` means the pinned provider is absent/offline. Attach a
  matching provider or cancel; changing `$currentPubkey` must not reassign it.
- No eligible relay lane means inspect route/source/context diagnostics.
- AUTH-blocked means the attempt has not consumed retry budget.
- A transient delivery failure advances persisted logical backoff.
- `OutcomeUnknown` for at-most-once work is terminal ambiguity, never permission
  to resend blindly.
- Relay rejection after signature changes receipt evidence only; the valid
  signed row remains in the canonical store.

## An unexpected relay

Inspect the lane and exact context that contributed it:

- NIP-65 author outbox;
- indexer discovery policy;
- hint/provenance/operator policy; or
- typed protocol context such as a NIP-29 group host.

Generic observe/publish has no raw route-list argument. A legitimate typed
protocol host is not a manual override. If a route is wrong, fix the owning
fact/module/compiler rule rather than hard-coding another relay downstream.

## High CPU or memory

Swift query and diagnostics streams already frame-coalesce and buffer newest
state. Check:

- expensive app work performed for every delivered snapshot;
- excessive live query handles / wire subscriptions;
- result windows that exceed the product's need; and
- target diagnostics for graph, ingress, observer, and scheduler pressure.

Do not add an app polling loop or unbounded queue. Remaining end-to-end bounds
belong in NMP and must produce explicit diagnostics/shortfall.

## Signer and account confusion

Remember the three distinct questions:

1. Which pubkey does a reactive query read?
2. Which identity was pinned to this write?
3. Which identity/context is used for AUTH or crypto?

One engine has one shared cache; changing current pubkey is not a privacy wipe.
Use explicit destructive reset before handing the engine to an untrusted local
user. Shut down every engine using the path first, then call
`Engine::reset_persistent_store(path)` in Rust,
`NMPEngine.resetPersistentStore(at:)` in Swift, or
`NMPEngine.resetPersistentStore(path)` in Kotlin. The operation destroys the
canonical store but does not remove a separately configured platform account
checkpoint.

## Diagnostics delivery

Diagnostics is pushed; never poll it. The initial snapshot may correctly show
an empty plan before any demand or connection exists. Keep the permanent screen
available in development and production so routing/source facts remain
inspectable.

See [Current implementation status](03-status-map.md) when a target diagnostic
field or durable transition is not yet present in the shipping facade.

---

<!-- nav-footer -->
<sub>← [Testing](25-testing.md) · [Index](README.md) · [Reusable declarations and protocol operations](27-recipes-and-choosing.md) →</sub>
