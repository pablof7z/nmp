# Current implementation status

> **Shipping-truth appendix, last reviewed 2026-08-12.** The repository
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
- Transport connects to real relays, verifies inbound events, and replays
  demand on reconnect.
- Caller-supplied signed writes are verified at the engine acceptance boundary
  before `Accepted` or relay publication (#56).
- Durable and at-most-once writes are atomically accepted as one canonical
  pending row plus obligation/receipt, then sign, promote, route, and stream
  per-relay statuses from the durable ids; ephemeral writes are receipt-only.
- The current account's available signing provider can sign an exact event
  without accepting or publishing a write; the `nmp` facade validates the
  returned event and preserves bounded, cancellable ownership.
- The `nmp` facade exposes live queries, writes, and permanent diagnostics.
- The `nmp` facade exposes an idempotent destructive reset for a closed
  persistent store without changing the opaque session payload the app stores
  separately.
- The canonical `nmp` facade exposes a stable public API.

## Target contract not yet complete

| Contract area | Current gap | Queue |
|---|---|---|
| Canonical Rust product facade | facade and FFI are built; v2 remains provisional while the broader promoted contracts below are open | [#52](https://github.com/pablof7z/nmp/issues/52) |
| Durable acceptance and pending row | crash-atomic acceptance/promotion/cancellation are built; runtime restart recovery, receipt reattachment, and durable attempt resumption remain | [#2](https://github.com/pablof7z/nmp/issues/2), [#3](https://github.com/pablof7z/nmp/issues/3) |
| Signer lifecycle | frozen-pubkey selection, sign-only, and whole-session local-provider restoration are built; additional provider implementations and permanent signer diagnostics remain | [#47](https://github.com/pablof7z/nmp/issues/47), [#51](https://github.com/pablof7z/nmp/issues/51) |
| Query descriptor/evidence | full `Demand` identity and scoped `AcquisitionEvidence` are built; live handles report their current active plan, while coverage-satisfied `MaxAge` handles retain only the compact opening source facts and watermarks that justified suppression; broader permanent diagnostics remain | [#49](https://github.com/pablof7z/nmp/issues/49), [#714](https://github.com/pablof7z/nmp/issues/714) |
| Protocol modules | `nmp-nip29` exposes tolerant reading of NIP-51 kind:10009 as part of NMP's remembered-group product capability; `nmp::nip29` owns typed durable group and relay-in-use list operations, and the exact NIP-29 host-scoped group operations remain separate | [#1384](https://github.com/pablof7z/nmp/issues/1384), [#1552](https://github.com/pablof7z/nmp/issues/1552) |
| Bounded delivery | end-to-end queue, observer, ingress, and explicit-shortfall proof remains | [#46](https://github.com/pablof7z/nmp/issues/46) |
| Diagnostics | raw connection, AUTH, retry, error, and limit evidence remains incomplete | [#51](https://github.com/pablof7z/nmp/issues/51) |

The umbrella ordering and design-signoff trail live in
[#43](https://github.com/pablof7z/nmp/issues/43).

## Important current/target differences

| Concept | Current repository API | Provisional North Star |
|---|---|---|
| Query identity | `Demand(selection, read routing, authenticated identity, cache, freshness)` | same semantic descriptor; public spelling remains provisional |
| Nested derived query | `Derived(inner: Demand)` | explicit inner demand with independent source/access/cache/freshness policy |
| Query output | row deltas/current rows plus scoped `AcquisitionEvidence`; diagnostics retain exact intervals | richer descriptor-scoped cache/acquisition/shortfall evidence |
| Current identity | the whole session owns accounts, optional persistable providers, and current selection; accepted work pins its author and resumes only through a matching available provider | same whole-session model plus additional provider implementations |
| Accepted write | crash-atomic obligation, receipt, and canonical pending row; restart recovery remains | recovered/reattached durable work with exact attempt evidence |
| Explicitly non-durable write | receipt-only `Ephemeral` path, never journaled as a pending row | same observable non-resumable policy |
| Rust construction | one canonical `nmp::Engine` facade; mechanism crates remain internal/test seams | same facade, promoted to v2 compatibility |
| Protocol meaning | raw events/app code | optional exact NIP modules over the same facade |

Do not infer global completeness from `AcquisitionEvidence`. It is scoped to
one descriptor, read routing, and authenticated identity. For a coverage-satisfied
`MaxAge` handle it preserves the compact opening-time source proof and
watermarks; global diagnostics report exact per-relay/filter facts for actually
active wire work, not a hypothetical replan of that suppressed handle.

For terminology, use the [glossary](glossary.md). For the imagined product,
return to the [ten-minute embedding](04-ten-minute-timeline.md).

---

<sub>[Index](README.md) · Related: [Known gaps](../known-gaps.md) · [Glossary](glossary.md) · [Provisional API](33-versioning.md)</sub>
