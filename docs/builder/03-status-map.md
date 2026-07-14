# Current implementation status

> **Shipping-truth appendix, last reviewed 2026-07-13.** The repository
> [README](../../README.md), [known gaps](../known-gaps.md), and live GitHub
> issues take precedence when implementation moves after this review.

The rest of the builder guide describes a coherent provisional v2 product.
This page answers the separate question: what can a developer exercise in the
repository today?

## Proven today

- The four-case `Binding` grammar and closed selectors resolve live demand.
- Current-pubkey changes re-root dependent graphs.
- The compiler/router produces per-relay plans, refcounts shared demand,
  coalesces compatible wire filters, and caps fan-out.
- The store applies id dedup, provenance merge, replaceable/addressable winner
  semantics, NIP-09 deletion, NIP-40 expiry, coverage persistence, and GC rules.
- Transport connects to real relays, verifies inbound events, replays demand on
  reconnect, and supports probed negentropy.
- Caller-supplied signed writes are verified at the engine acceptance boundary
  before `Accepted` or relay publication (#56).
- Durable and at-most-once writes are atomically accepted as one canonical
  pending row plus obligation/receipt, then sign, promote, route, and stream
  per-relay statuses from the durable ids; ephemeral writes are receipt-only.
- NIP-46 supports bunker and client-initiated connections, independent relay
  ownership, NIP-42 AUTH, exact correlation, authorization URLs, relay
  switching, signing, NIP-44 crypto, local Primal discovery, and restart
  reattachment of a durable pending write through relay ACK.
- The active registered signer can sign an exact event without accepting or
  publishing a write; Rust/FFI/Swift/Kotlin validate the returned event and
  preserve bounded, cancellable ownership.
- Rust/FFI/Swift/Kotlin expose live queries, writes, and permanent diagnostics.
- Rust/FFI/Swift/Kotlin expose an idempotent destructive reset for a closed
  persistent store without deleting a separate platform account checkpoint.
- The canonical `nmp` facade and UniFFI component have pinned reproducible
  surface snapshots with an append-only governance gate.
- The Swift falsifier app runs against public relays as a normal SwiftUI app.
- A desktop-JVM Kotlin package proves cold `Flow` observation and cancellation.

## Target contract not yet complete

| Contract area | Current gap | Queue |
|---|---|---|
| Canonical Rust product facade | facade, FFI, demo, direct-vs-FFI parity, surface snapshots, and append-only governance are built; v2 remains provisional while the broader promoted contracts below are open | [#52](https://github.com/pablof7z/nmp/issues/52) |
| Durable acceptance and pending row | crash-atomic acceptance/promotion/cancellation are built; runtime restart recovery, receipt reattachment, and durable attempt resumption remain | [#2](https://github.com/pablof7z/nmp/issues/2), [#3](https://github.com/pablof7z/nmp/issues/3) |
| Signer lifecycle | frozen-pubkey selection, governed sign-only, remote NIP-46 reattachment, and local Primal handoff are built; per-write override, NIP-55 execution, platform vault restore, and permanent signer diagnostics remain | [#47](https://github.com/pablof7z/nmp/issues/47), [#51](https://github.com/pablof7z/nmp/issues/51) |
| Query descriptor/evidence | query output now carries current-plan `AcquisitionEvidence` distinct from diagnostic intervals; descriptor identity is still filter-centric and lacks full source/access context | [#49](https://github.com/pablof7z/nmp/issues/49) |
| Protocol modules | exact module ownership and immutable contextual publication are designed, not shipped; NIP-51 kind 10009 composition into NIP-29 remains queued | [#45](https://github.com/pablof7z/nmp/issues/45), [#63](https://github.com/pablof7z/nmp/issues/63) |
| Bounded delivery | end-to-end queue, observer, ingress, and explicit-shortfall proof remains | [#46](https://github.com/pablof7z/nmp/issues/46) |
| Diagnostics | raw connection, AUTH, retry, error, and limit evidence remains incomplete | [#51](https://github.com/pablof7z/nmp/issues/51) |
| Android | desktop-JVM Flow and a narrow relay Compose proof exist; Android AAR/runtime/Keystore falsification does not | [#40](https://github.com/pablof7z/nmp/issues/40) |

The umbrella ordering and design-signoff trail live in
[#43](https://github.com/pablof7z/nmp/issues/43).

## Important current/target differences

| Concept | Current repository surface | Provisional North Star |
|---|---|---|
| Query identity | `LiveQuery(Filter<Binding>)` | selection + source authority + access context |
| Nested derived query | `Derived(inner: Filter)` has selection only | explicit inner demand with independent source/access context |
| Query output | row deltas/current rows plus scoped `AcquisitionEvidence`; diagnostics retain exact intervals | richer descriptor-scoped cache/acquisition/shortfall evidence |
| Current identity | `setActiveAccount` supplies the default; accepted work pins its author and resumes only through a matching local or NIP-46 capability | registered providers plus explicit per-write identity override |
| Accepted write | crash-atomic obligation, receipt, and canonical pending row; restart recovery remains | recovered/reattached durable work with exact attempt evidence |
| Explicitly non-durable write | receipt-only `Ephemeral` path, never journaled as a pending row | same observable non-resumable policy across platform projections |
| Rust construction | one canonical `nmp::Engine` facade; mechanism crates remain internal/test seams | same facade, promoted to v2 compatibility |
| Protocol meaning | raw events/app code | optional exact NIP modules over the same facade |

Do not infer global completeness from `AcquisitionEvidence`. It is scoped to
the current source plan; inspect diagnostics for exact per-relay/filter facts.

## Runnable evidence

- [apps/Falsifier](https://github.com/pablof7z/nmp/tree/master/apps/Falsifier) is the iOS library-vs-framework
  falsifier and permanent diagnostics screen.
- [crates/nmp-demo](https://github.com/pablof7z/nmp/tree/master/crates/nmp-demo) exercises the current direct-Rust
  path.
- [Packages/NMP](https://github.com/pablof7z/nmp/tree/master/Packages/NMP) is the Swift package.
- [Packages/NMPKotlin](https://github.com/pablof7z/nmp/tree/master/Packages/NMPKotlin) is the desktop-JVM Flow
  projection.
- [features](https://github.com/pablof7z/nmp/tree/master/features) contains executable current behavior plus
  `@wip` target scenarios.

For terminology, use the [glossary](glossary.md). For the imagined product,
return to the [ten-minute embedding](04-ten-minute-timeline.md).

---

<sub>[Index](README.md) · Related: [Known gaps](../known-gaps.md) · [Glossary](glossary.md) · [Governed provisional API](33-versioning.md)</sub>
