# Windowed observation is an unsettled design edge

NMP needs bounded result delivery for large or long-lived selections. The
public windowing/pagination API is not yet settled and is deliberately outside
the main North Star path.

This page records constraints, not a promised `observeCollection` spelling.

## Two windows must not collapse

There are at least two independent concerns:

1. **Acquisition window:** what time/id range the demand asks sources to
   acquire and retain.
2. **Presentation window:** which subset of already represented local rows the
   app currently renders.

A `loadMore` button moving a presentation cursor must not silently rewrite the
source/acquisition cursor. Conversely, a wider acquisition demand does not
require every observer to retain or render every row.

The descriptor and snapshot must make any app-requested bound explicit. An
engine-imposed cap appears as shortfall rather than masquerading as the same
thing.

## Ordering ownership

App-specific ranking remains after delivery. An app comparator cannot determine
engine acquisition, persistence, dedup, or source cursor correctness.

If NMP eventually offers an engine-maintained order/window, its keys must be a
small closed vocabulary with deterministic cursor semantics. That feature would
remain a mode of the live-query noun, not a feed manager or third workload.

## Heterogeneous views

An app may observe several independent demands and combine their snapshots in
app state. NMP may later expose a semantics-preserving composite result value,
but it must not hide several unrelated source/access descriptors behind one
opaque subscription or invent a favored home-feed composition.

## What builders do meanwhile

- Put explicit `since`, `until`, and `limit` values in the demand when they are
  truly source/acquisition requirements.
- Keep presentation paging/windowing in app state after delivery.
- Use platform collection/stream operators for app ranking and grouping.
- Inspect `shortfall` to distinguish requested bounds from engine limits.
- Do not claim an empty page or local window represents the global result.

The public shape should be promoted only after it proves separate acquisition
and presentation cursors, bounded memory, stable identity, and parity across
Rust, Swift, and Kotlin.

---

<sub>[Index](README.md) · Related: [Snapshots and evidence](10-consuming-results.md) · [Bounded delivery](23-threading-lifecycle.md) · [Cost and limits](24-performance.md)</sub>
