# Guarantees and the bugs they exclude

This chapter is the builder-facing map of the provisional v2 guarantees. The
[bug-class ledger](../bug-class-ledger.md) and
[current implementation status](03-status-map.md) record which proofs ship.

The old NMP relied on broad doctrine and lints. The rewrite closes a bug class
only when the supported facade makes the bad path unreachable and a falsifier
proves it.

## Core structural guarantees

### #1: one canonical store mutation path

**Excludes:** stale replaceable winners and duplicate ids with lost provenance.

Exact-id dedup, provenance merge, and replaceable arbitration happen behind the
store door. Apps do not maintain a second profile/list/event cache or decide
which candidate wins.

### #2: demand-derived subscription lifetime

**Excludes:** app-opened REQs that leak or close while another observer still
needs them.

Apps own query-handle scope. NMP owns reference counts, compilation, REQ open /
close, and surgical dependency updates. There is no public open-REQ verb.

### #3: typed relay authority

**Excludes:** a generic `relays:` escape hatch that bypasses routing policy.

Raw reads and writes do not take app-expanded relay arrays. Indexers are typed
operator policy. A protocol operation may contribute a closed contextual fact,
such as a NIP-29 group host relay, but cannot register an arbitrary route
closure.

### #4: capped fan-out with visible shortfall

**Excludes:** unioning every discovered relay into an unbounded connection set.

Every whole-demand cap and uncovered portion is explicit; a two-relay objective
is never presented as met when available facts or the cap prevented it.

### #5: dedup with provenance

**Excludes:** one visual row per relay or loss of the evidence describing where
an event was observed.

The canonical event id identifies one row. Duplicate arrival merges source
provenance before downstream semantics.

### #6: private routes cannot widen

**Excludes:** falling back from a private/narrow protocol route to public
relays.

Narrow route types have no widen operation. A protocol module that cannot
resolve a required private route fails closed with typed evidence.

### #7: source evidence cannot claim global truth

**Excludes:** treating an empty cache or one relay's EOSE as proof that no
matching event exists anywhere.

The snapshot carries rows plus compact per-planned-source acquisition and
shortfall facts. Apps interpret those facts; NMP exposes no
`synced`, `syncHealth`, global `complete`, or `authoritativeEmpty` state.

### #8: negentropy requires a proved capability

**Excludes:** sending NIP-77 messages to an unprobed relay.

Only the prober can mint `ProbedRelay`; the negentropy effect requires that
token. Other relays use REQ.

### #9: durable acceptance is not convergence

**Excludes:** a publish return value being mistaken for relay success.

`Accepted` is emitted only after atomic persistence of the frozen body, expected
author, intent, receipt, and canonical pending row. ACK, rejection, and retry
remain separate facts.

### #10: accepted writes cannot drift to another signer

**Excludes:** an account/current-pubkey change reassigning an already accepted
unsigned write.

Publish defaults to the signer registered for `$currentPubkey`, permits an
explicit identity override, and pins the selected expected author at
acceptance. Missing capability becomes durable `AwaitingSigner`, not silent
reassignment.

### #11: apps do not own derived expansion

**Excludes:** app code watching one query, caching its projected set, and
manually repairing another subscription.

`Derived` and `SetOp` remain inside the engine's closed graph. Reusable helpers
return the same printable graph; they do not receive expanded-set callbacks.
Changing `$currentPubkey` reroots only dependent graphs. Literal multi-account
queries remain live.

### #12: core has no presentation policy

**Excludes:** one app's date, name, truncation, ranking, or plaintext display
policy becoming shared infrastructure.

Core and modules emit raw protocol-semantic values. Crypto providers may
decrypt protocol data, but presentation remains downstream in the app/UI.

## Extended v2 guarantees

### #13: acquisition and presentation cursors stay distinct

**Excludes:** a late-arriving old-timestamped event being skipped because a UI
pagination cursor already passed it.

The exact windowed/collection API remains unsettled; the cursor-ownership rule
is the requirement.

### #14: schema ownership is not contextual authority

**Excludes:** a module claiming a foreign content kind merely because its
protocol publishes that draft in a context.

A NIP module owns only its exact schemas. NIP-29 may add its `h` tag and group
host context to a NIP-68 photo draft without owning the photo kind. Core
validates the immutable composition and signs once.

### #15: pending writes use ordinary query semantics

**Excludes:** an optimistic overlay or direct write-to-observer lane diverging
from the store.

The canonical row carries `Pending(intentId) | Signed(signature)` and
participates in normal filters, derived bindings, replacement, delete, expiry,
persistence, and invalidation.

### #16: exactly one retry owner per domain

**Excludes:** transport, signer adapter, and outbox independently resending the
same obligation.

Transport reconnects sockets; a signer adapter owns one correlated operation;
the durable outbox owns each `(intent, relay)` attempt; one deadline scheduler
owns time and concurrency.

### #17: limits cannot silently truncate

**Excludes:** first-N substitution presented as the requested result.

Every graph, wire, relay, observer, and result limit must preserve exact
semantics, return explicit shortfall, reject with a type, or backpressure with a
diagnostic reason. Every projection and interior queue must prove the bound end
to end.

### #18: source/access contexts cannot borrow evidence incorrectly

**Excludes:** equal filters under different AUTH or source authority sharing a
watermark as though they were the same request.

Descriptor identity is `Selection + SourceAuthority + AccessContext`.
Selection work may share; wire demand and evidence share only after a
compatibility proof. Every nested `Derived` demand carries its own explicit
source/access context; it cannot inherit or borrow the outer demand's evidence.

### #19: event/outbox persistence cannot become a secret vault

**Excludes:** raw signing material being stored beside event and retry state.

Rust persists obligations and expected pubkeys. Standard platform providers
own secure secret storage; apps own identity policy and may supply custom
providers.

## Builder rule

Treat this list as the North Star, not evidence that every mechanism ships. A
design doc or passing adjacent test does not promote a guarantee; the supported
Rust facade and platform projections must be falsified end to end. Check the
status appendix and owning issue before relying on one in a shipping app.

---

<!-- nav-footer -->
<sub>← [Reusable declarations and protocol operations](27-recipes-and-choosing.md) · [Index](README.md) · [What NMP does NOT do](29-not-do.md) →</sub>
