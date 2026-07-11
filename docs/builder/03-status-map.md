# Current implementation status

> **Shipping-truth appendix, last reviewed 2026-07-11.** The repository
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
- The current in-process write path signs, routes, publishes, and streams
  per-relay statuses.
- Rust/FFI/Swift expose live queries, writes, and permanent diagnostics.
- The Swift falsifier app runs against public relays as a normal SwiftUI app.
- A desktop-JVM Kotlin package proves cold `Flow` observation and cancellation.

## Target contract not yet complete

| Contract area | Current gap | Queue |
|---|---|---|
| Canonical Rust product facade | facade work is landing in units; FFI/demo/parity/governance remain | [#52](https://github.com/pablof7z/nmp/issues/52) |
| Durable acceptance and pending row | current writes are not yet one crash-atomic row + obligation + receipt boundary | [#2](https://github.com/pablof7z/nmp/issues/2), [#3](https://github.com/pablof7z/nmp/issues/3) |
| Signer lifecycle | default/override pinning, provider reattachment, and platform vaults remain | [#47](https://github.com/pablof7z/nmp/issues/47), [#6](https://github.com/pablof7z/nmp/issues/6) |
| Query descriptor/evidence | public query is still filter-centric and exposes aggregate coverage rather than full source/access evidence | [#49](https://github.com/pablof7z/nmp/issues/49), [#12](https://github.com/pablof7z/nmp/issues/12) |
| Protocol modules | exact module ownership and immutable contextual publication are designed, not shipped | [#45](https://github.com/pablof7z/nmp/issues/45) |
| Bounded delivery | end-to-end queue, observer, ingress, and explicit-shortfall proof remains | [#46](https://github.com/pablof7z/nmp/issues/46) |
| Diagnostics | raw connection, AUTH, retry, error, and limit evidence remains incomplete | [#51](https://github.com/pablof7z/nmp/issues/51) |
| Shared-cache logout | explicit destructive engine reset remains | [#53](https://github.com/pablof7z/nmp/issues/53) |
| Android | JVM Flow exists; AAR/Compose/Keystore falsification does not | [#40](https://github.com/pablof7z/nmp/issues/40) |

The umbrella ordering and design-signoff trail live in
[#43](https://github.com/pablof7z/nmp/issues/43).

## Important current/target differences

| Concept | Current repository surface | Provisional North Star |
|---|---|---|
| Query identity | `LiveQuery(Filter<Binding>)` | selection + source authority + access context |
| Query output | row deltas/current rows plus aggregate `Coverage` | snapshot rows + cache/acquisition/shortfall evidence |
| Current identity | `setActiveAccount` couples current pubkey and local signer selection | current-pubkey input plus registered providers and per-write override |
| Accepted write | in-memory pending bookkeeping | crash-atomic obligation, receipt, and canonical pending row |
| Explicitly non-durable write | current `Ephemeral` path has no status | observable, reattachable receipt with non-resumable obligation policy |
| Rust construction | callers still reach mechanism assembly in existing apps | one canonical `nmp::Engine` facade |
| Protocol meaning | raw events/app code | optional exact NIP modules over the same facade |

Do not infer global completeness from the current aggregate `Coverage` enum.
Use it only as current source/window evidence and inspect diagnostics for exact
relay facts.

## Runnable evidence

- [`apps/Falsifier`](../../apps/Falsifier) is the iOS library-vs-framework
  falsifier and permanent diagnostics screen.
- [`crates/nmp-demo`](../../crates/nmp-demo) exercises the current direct-Rust
  path.
- [`Packages/NMP`](../../Packages/NMP) is the Swift package.
- [`Packages/NMPKotlin`](../../Packages/NMPKotlin) is the desktop-JVM Flow
  projection.
- [`features/`](../../features) contains executable current behavior plus
  `@wip` target scenarios.

For terminology, use the [glossary](glossary.md). For the imagined product,
return to the [ten-minute embedding](04-ten-minute-timeline.md).

---

<sub>[Index](README.md) · Related: [Known gaps](../known-gaps.md) · [Glossary](glossary.md) · [Governed provisional API](33-versioning.md)</sub>
