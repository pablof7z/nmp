# Evidence without global completeness

NMP cannot know the complete result for all of Nostr. It does not know every
relay that exists, every private/LAN relay a user operates, or whether an
offline relay would answer differently tomorrow.

The useful contract is narrower: show cached rows and report what happened at
the sources NMP actually planned.

## What a source can prove

For one exact demand, source, access context, request, and time window, NMP may
observe facts such as:

- cached-only: no current acquisition is planned;
- connecting or disconnected;
- blocked on AUTH or access policy;
- request sent with a known filter;
- EOSE observed for that request;
- a negentropy/watermark interval reconciled;
- error or forced disconnect;
- limited by a local cap; or
- not planned because required routing facts are missing.

Those facts remain scoped. EOSE from relay A says nothing about relay B, an
unknown relay C, or a different AUTH identity.

## Example: two planned relays

```text
rows: 12 cached canonical rows

acquisition:
  relay-a:
    request: sent
    eose: observed
    watermark: through 2026-07-10T12:00:00Z

  relay-b:
    connection: offline
    request: not sent

shortfall:
  - planned source relay-b unavailable
```

The snapshot is useful without a health score. An app might show cached data and
a small offline-source indicator, or show nothing at all. NMP reports facts and
does not decide that UX.

It must not label the twelve rows complete, globally synced, or authoritative.

## Empty rows are still useful

An empty `rows` array means:

> At this local revision, the canonical cache has no matching row.

The evidence explains the acquisition context. Empty rows with all planned
sources still connecting differs from empty rows after each planned source
reached EOSE, but neither proves that no matching event exists anywhere.

Apps can decide whether to render:

- an ordinary empty state;
- cached/offline wording;
- an AUTH action;
- source-specific retry information; or
- no evidence at all.

NMP does not manufacture `authoritativeEmpty` to simplify presentation.

## Cache evidence

Cache evidence ties rows to local replica state. It may include revision,
provenance summary, retained window, and whether GC or an app-requested result
bound affects what is held.

A persisted watermark helps NMP avoid redundant work and explain prior
acquisition. If GC removes data within its proven interval, the store must
shrink or remove that watermark in the same correctness path.

## Choose freshness per observation

Freshness is a closed policy on the existing query handle:

```swift
let feedAvatar = NMPDemand(
    selection: profileFilter,
    source: .authorOutboxes,
    freshness: .maxAge(seconds: 4 * 60 * 60)
)

let profilePage = NMPDemand(
    selection: profileFilter,
    source: .authorOutboxes,
    freshness: .live
)

let preview = NMPDemand(
    selection: eventFilter,
    source: .pinned(explicitRelays),
    cache: .strict,
    freshness: .cacheOnly
)
```

- `live` serves cached rows immediately and keeps ordinary remote acquisition
  open until the handle is dropped.
- `maxAge(seconds:)` checks existing coverage once when the handle opens. Every
  currently assigned relay for every atom in the query subtree must cover the
  requested floor and be recent enough. If so, this handle opens no wire work;
  otherwise it becomes ordinary `live` and its EOSE/NEG completion refreshes
  coverage.
- `cacheOnly` always opens zero wire work, with or without cached rows or
  coverage.

`maxAge` is deliberately conservative for a filter whose `until` is already
older than the freshness cutoff. Coverage attribution cannot honestly advance
past that sent `until`, so the handle becomes `live`; NMP does not currently
apply a separate "historical bounded query" suppression rule.

An empty cache can still be fresh under `maxAge`: coverage proves that the
scoped question was recently checked. Conversely, the timestamp of a cached or
incoming event does not prove freshness. Coverage is capped by the engine's
wall clock when the relay check completes, so a future-dated event cannot fake
a recent check.

The choice belongs to the component or app observation that owns the handle.
Equal `live`, `maxAge`, and `cacheOnly` demands may share graph/cache state, but
one handle's policy never opens, closes, or lends evidence to another.

## Durable history and resident bounds

NMP retains fetched, verified events in its durable store by default. Ordinary
engine startup, shutdown, local queries, bounded result projection, and memory
pressure do not delete durable rows. A bound on a query result, delivery
mailbox, worker pool, or other resident working set is a RAM bound; it is not a
durable-history retention policy.

Correctness mutations still change the canonical store. Replaceable events
supersede prior winners, authorized NIP-09 deletions retract their targets,
NIP-40 expiration removes expired rows, and a rejected pre-signature local
write may be compensated. Those governed mutations are distinct from
retention policy.

Durable storage is not promised to be literally infinite. An operator or user
may choose a quota, disk-pressure, or time-based retention policy, but that
choice must be inspectable and explicit. The current engine ships no automatic
retention policy. Its store-level `EventStore::gc` door is the explicit
claim-based eviction operation: it reports what it evicted and lowers or
removes every affected coverage interval in the same transaction as the row
deletion. A future engine-facing policy must preserve that governed operation;
it must never turn a RAM ceiling or an implicit maintenance pass into silent
durable deletion.

## Access context matters

Two requests with equal filters but different AUTH identities or visibility
grants may receive different answers. Their evidence is not interchangeable.

NMP may share local matching and any wire work proven safe to share. It must
retain source/access attribution so one identity's EOSE or watermark cannot
silently prove acquisition for another.

## Shortfall is local and explicit

Shortfall records the intended work NMP could not perform:

- a subtree atom or resolved demand with no covering source candidate;
- a zero-atom or zero-planned-source result that would otherwise look
  vacuously settled;
- a fan-out, graph, derived-set, wire-filter, or result bound that prevented
  intended acquisition; or
- another explicit engine-imposed local limit, distinct from a caller-requested
  result bound.

A planned relay that is disconnected, awaiting AUTH, denied, or in error remains
present in `SourceAcquisition.status`; it is not moved into shortfall. Its
persisted `reconciledThrough` evidence may coexist with that current status.

NMP may chunk or coalesce only when semantics remain exact. It never takes the
first N values and presents them as the whole result.

## Ordinary evidence versus diagnostics

Snapshots carry compact facts suitable for app UX. Diagnostics carries the raw
proof trail:

- exact descriptor and graph expansion;
- source-plan revision and lane reasons;
- exact per-relay wire filter JSON;
- connection generation, AUTH, EOSE, negentropy, and errors;
- events received per relay/kind;
- watermarks, caps, and queue pressure; and
- the local reason for each shortfall.

Both surfaces project the same engine state. The app does not reconstruct one
from transport callbacks.

---

<sub>[Index](README.md) · Related: [Snapshots and evidence](10-consuming-results.md) · [Writing and receipts](14-writing.md) · [Diagnostics](22-diagnostics.md)</sub>
