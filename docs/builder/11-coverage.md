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

## Access context matters

Two requests with equal filters but different AUTH identities or visibility
grants may receive different answers. Their evidence is not interchangeable.

NMP may share local matching and any wire work proven safe to share. It must
retain source/access attribution so one identity's EOSE or watermark cannot
silently prove acquisition for another.

## Shortfall is local and explicit

Shortfall records the intended work NMP could not perform:

- uncovered authors because relay discovery produced no route;
- planned relay unavailable or AUTH-blocked;
- fan-out cap prevented an otherwise known source;
- derived set or wire filter exceeded an exact limit; or
- result/cache bound was engine-imposed rather than requested by the app.

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
