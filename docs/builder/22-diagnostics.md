# Diagnostics is the permanent proof surface

NMP owns machinery the app deliberately does not. Diagnostics makes every
invisible decision inspectable without becoming a control API.

```swift
for await snapshot in engine.observeDiagnostics() {
    diagnosticsModel.apply(snapshot)
}
```

Observing diagnostics cannot change demand, routes, retry, limits, or store
state.

## Demand and source plan

For every active descriptor, show:

- selection plus expanded binding graph;
- source authority and access context;
- descriptor/plan revision;
- graph nodes and reference counts;
- compiled atoms and route reasons;
- authors/protocol objects served by each source; and
- explicit uncovered/shortfall reasons.

## Exact relay work

For every relay, show:

- connection generation and connecting/open/disconnected state;
- exact wire filter JSON and subscription id/count;
- coalescing rule and participating descriptors;
- AUTH challenge, identity/policy reference, result, and error;
- EOSE and negentropy session facts;
- per-filter watermark intervals;
- events received by kind; and
- backpressure or forced-disconnect reason.

The wire filter is the actual serialized request, not a reconstruction in the
app.

## Store and query state

Show:

- canonical row/revision counts;
- local versus relay provenance;
- pending versus signed local rows;
- replaceable supersession, deletion, expiry, and GC counters;
- cache/watermark invalidation caused by eviction; and
- the compact evidence revision emitted to each ordinary query.

## Write state

For every retained receipt/open intent, show:

- stable intent and receipt ids;
- durability/retention policy;
- expected and selected signer identity reference;
- pending/signed state and cancellation/compensation;
- current route revisions and their typed reasons;
- per-relay attempt ordinal, eligibility, outcome, and retry deadline;
- AUTH/offline blocking that does not consume an attempt; and
- terminal receipt retention/aggregation boundary.

Raw secret keys, bearer tokens, plaintext private messages, and decrypted
content never appear.

## Limits and scheduler

Show configured/effective limits, current utilization, graph/wire/result
shortfall, dropped intermediate snapshot counts, ingress queue pressure,
scheduler backlog, concurrency, and next real deadlines.

Diagnostics reports facts. It does not synthesize `syncHealth`,
`globallySynced`, `authoritativeEmpty`, or a single success score.

## Debugging order

1. Did the descriptor produce a source plan?
2. What exact authority and access context produced each lane?
3. What exact filter reached each relay?
4. Did the connection or AUTH state prevent the request?
5. What arrived and what per-source EOSE/watermark evidence exists?
6. Did a local cap/queue produce explicit shortfall?
7. If canonical rows exist but the UI is empty, inspect the app's fold,
   product policy, ordering, and rendering.

This order finds the owning layer instead of guessing from an empty screen.

## Delivery contract

Diagnostics is a bounded latest-state stream. A slow screen may skip
intermediate frames and must eventually receive the newest complete local
diagnostic revision. Durable receipt history remains independently
reattachable; diagnostics is not its only storage.

Every NMP falsifier renders this as a permanent screen; that is the acceptance
test made visible. A production app may keep the raw surface behind support or
developer UI, and most product screens will use only the compact evidence in
their query snapshots. The diagnostic data itself remains available and
testable even when ordinary users never open it.

---

<sub>[Index](README.md) · Related: [Evidence without completeness](11-coverage.md) · [Source and routing context](17-relays.md) · [Troubleshooting](26-troubleshooting.md)</sub>
