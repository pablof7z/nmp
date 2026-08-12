---
title: "Outbox — the built-in Auto write resolver"
category: routing
slug: outbox
status: built
date: 2026-07-29
owns:
  - Auto write routing from neutral author facts
  - author and p-tag directionality
  - operator app and fallback contributions
  - verified reply-parent provenance contribution
  - settlement versus ignorance
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/resolution-lifecycle.md
---

# Outbox — the built-in Auto write resolver

`Auto` consumes protocol-neutral facts. It does not parse kind:10002, choose
indexers, or mutate a directory.

For a signed event:

1. the event author's `outbound` relays contribute destinations;
2. operator app relays contribute independently;
3. each decoded `p` tag contributes that recipient's `inbound` relays;
4. a reply contributes one relay where NMP actually observed its direct
   parent, when that parent exists in the canonical store with relay
   provenance;
5. operator fallback relays may top up a settled thin recipient contribution.

The reply contribution has two deliberately separate inputs:

- `ThreadPosition::read` determines the direct parent event id from the signed
  event's NIP-10/NIP-22 rows;
- `EventStore::query(id)` supplies the canonical row, and only
  `row.provenance.seen` may supply a destination.

The relay cell authored into an `e` tag is never a routing fact. A signature
proves who authored the hint; it does not prove the named relay carried the
parent. If the canonical row has several verified observations, Auto takes
the first relay in normalized sorted order — exactly one, using the same
temporary deterministic choice the canonical `Row` uses for an emitted hint.
Choosing the best among several verified sources remains #1243's policy; a
widely replicated parent must not silently fan one reply out to every source.

A canonical-store read failure is not a canonical miss. The resolver returns
the author/app/recipient destinations it already knows, keeps the route open,
and reports the typed persistence error to the store supervisor. Those known
lanes may connect and publish immediately. Once store recovery succeeds, the
ordinary queue rewrite reruns the same strategy and appends the parent lane if
the verified source is then readable.

The route remains incomplete while any required author fact is `Unknown`, or
while a failed parent-provenance read has not been retried successfully.
`Present` with empty sets and `Absent` are both settled answers and contribute
no destinations for that direction.

There is one deliberate liveness distinction:

- if at least one destination exists, routing completes when no input remains
  `Unknown`;
- if the complete answer contains zero destinations, the write remains
  actionably parked and every contributing author remains an author-route
  provider need, even when its current state is `Present(empty)` or `Absent`.

The latter set is not called `unknown authors`: those facts are settled.
Keeping their provider need alive is what lets a later positive replacement
unpark the write instead of stranding it behind a retired `Auto`.

The complete current provider-need set is emitted as
`AuthorRouteNeedsChanged`. Generic core never translates it into a protocol
query. An optional component may satisfy it; `nmp/nip65` is the current Rust
assembly.

## Atomic replacement

Both directional sets belong to one `AuthorRoutes` value. A component can
replace:

```text
Present { outbound, inbound }
Absent
```

but cannot write `Unknown`; only cold start creates ignorance. Replacement,
recompile, and open-route rewrite occur in one reducer turn, so no write can
observe a new outbound set paired with an old inbound set.

## Exact writes

`WriteRouting::Explicit` remains the protocol-owned exact route. It bypasses
author facts and is the correct value for operations such as first-publication
bootstrap, where the destination set is part of the operation itself.
