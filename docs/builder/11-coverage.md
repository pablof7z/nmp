# Query evidence: cache state and per-source acquisition

**Status: CURRENT + TARGET.** Persisted per-(filter, relay) watermarks and the
current aggregate `Coverage` type are built. The target public contract does
not interpret those facts as global completeness. It returns cached rows plus
small, source-scoped acquisition evidence; full per-relay proof remains in
diagnostics.

## The fact NMP cannot know

No client knows the complete global Nostr result for a filter. A matching event
may exist on a relay the engine has never heard of, on a private relay, or on a
LAN relay that is currently offline. EOSE means one relay finished answering
one request; it does not certify the network.

Therefore NMP must not publish `synced`, `complete`, `syncHealth`, or
`authoritative empty` as interpretations of the global result.

## What a query snapshot can say honestly

A query delivery consists of:

1. **Rows currently accepted by the canonical store.** These may be useful
   immediately from cache.
2. **Cache evidence.** Whether rows came from persisted state and the relevant
   last-observed/watermark facts.
3. **Acquisition evidence for the current source plan.** Which planned sources
   are connecting, requesting, waiting for AUTH, at EOSE, unavailable, limited,
   or failed.

The app interprets those facts for its UI. NMP does not collapse them into a
promise that the result is globally complete.

Illustrative target shape, not frozen API:

```swift
struct QuerySnapshot {
    let rows: [Row]
    let cache: CacheEvidence
    let acquisition: AcquisitionEvidence
}
```

The query snapshot should remain small. Exact wire filters, relay URLs,
connection generations, AUTH challenges, lane provenance, and raw watermarks
belong on the diagnostics stream.

## Empty remains evidence-dependent

An empty row set means only "the canonical local store currently has no
matching row." The surrounding evidence can explain that:

- there was no persisted match and sources have not answered;
- every currently planned relay reached EOSE for the requested window;
- one planned relay is offline;
- another is blocked on AUTH;
- the query was locally limited;
- no source could be planned for part of the demand.

Those are useful and different facts. None licenses the stronger statement
"no matching event exists anywhere on Nostr."

Apps may render an empty state after applying their own product policy to the
evidence. They should be able to explain that policy, for example "nothing was
found on the three relays currently checked" rather than "nothing exists."

## Watermarks remain valuable

Removing the global-completeness interpretation does not remove watermarks.
NMP still owns persisted per-source coverage facts because they support:

- immediate offline cache delivery;
- avoiding redundant acquisition for already-examined source windows;
- resuming after restart;
- showing what each planned relay has or has not answered;
- diagnostics and deterministic sync planning.

The correction is semantic: a watermark proves something about one source and
one request shape, not about the whole network.

## Cold-start offline

On an offline relaunch, NMP should immediately return cached matching rows and
the persisted evidence that produced them. The app can show useful stale data
without a spinner if that fits its product. It can also show that no current
source is reachable. NMP does not label the cache "the truth" or upgrade an
empty cache into an authoritative empty result.

## Current implementation gap

The shipping SDK exposes `Coverage.unknown | completeUpTo` on `RowBatch` and
some builder examples still branch on it. That shape must be replaced across
the canonical Rust facade, FFI, Swift, and Kotlin. Until then,
`completeUpTo(t)` must be read narrowly: all sources in that current plan met
the engine's existing aggregation rule up to `t`; it is not global
completeness.

---

<!-- nav-footer -->
<sub>← [Consuming results](10-consuming-results.md) · [Index](README.md) · [Feeds & the Collection mode](12-collection-mode.md) →</sub>
