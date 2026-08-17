---
title: "Outbox — the built-in Auto write resolver"
category: routing
slug: outbox
status: built
date: 2026-07-29
owns:
  - **the owner ruling that the routing lanes are additive, not alternatives**
  - Auto write routing from neutral author facts
  - author and p-tag directionality
  - operator app and fallback contributions
  - verified reply-parent provenance contribution
  - settlement versus ignorance
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/builder/17-relays.md
---

# Outbox — the built-in Auto write resolver

## The lanes are additive — owner ruling, 2026-08-17

This is the ruling the rest of this document executes, and it has had to be
given more than once. Verbatim:

> by default: publish per outbox
> app relays ALWAYS FUCKING READ AND FUCKING WRITE IN THEM
> indexers: always publish kind:0, kind:3, kind:1xxxx events in them

Stated as rules:

1. **Outbox is the default publish lane, per author.**
2. **App relays always apply, for both reads and writes.** Not a choice, not
   a fallback, not a top-up — always.
3. **Indexers always receive kind:0, kind:3 and kind:1xxxx events.**

**These three are additive lanes, not alternatives.** There is no lane an app
picks instead of another, and no lane that only applies when an earlier one
came up short. Any document, type, or API that presents an author's outbox and
the operator-configured relays as two options to choose between is wrong and
gets corrected, not annotated.

Two consequences, and they are the whole app-facing story:

- **An app that says nothing gets every lane that applies.**
- **An app that names relays gets exactly those, never widened.**

Nothing in between, and no third thing to say. This is why the read and write
surfaces carry no vocabulary for naming a lane: the lanes are NMP's business,
and the only app-visible distinction is whether the app named relays or did
not.

### What is built, and what the ruling asks for that is not

Recorded honestly, because two of the three rules describe shipped behaviour
and one does not.

- **Rule 1 is built.** The registry dispatches on kind and falls through to
  this resolver (`resolvers.md`); item 1 below is the author's outbox lane.
- **Rule 2 is built on both sides.** Writes: item 2 below. Reads: operator app
  relays are added to every non-exact route unconditionally
  (`crates/nmp-router/src/facts.rs`, `crates/nmp-router/src/route.rs` —
  `operator_app_routes`, no condition on the other lanes having produced
  anything).
- **Rule 3 is NOT built.** There is no indexer write lane. This resolver
  explicitly does not choose indexers, indexers appear in the shipped lane
  vocabulary only as a read/discovery input, and
  `docs/internals/writes/durable-replaceable-operations.md` states the
  converse rule for the read direction — that reading a kind:0 from an indexer
  does not make that indexer a write relay, which remains true and is not what
  rule 3 says. Rule 3 is the owner's ruling on where NMP is going, and the
  work to get there is not designed here. Tracked in `docs/known-gaps.md`.

An operator **fallback** relay is a fourth thing and is not covered by the
ruling. It is conditional by construction: on the read side
`operator_fallback_routes` returns nothing at all when any app relay is
configured, and on the write side this resolver decides when a settled thin
recipient contribution makes fallbacks eligible (item 5 below). Do not read
rule 2 as covering it.

## What `Auto` consumes

`Auto` consumes protocol-neutral facts. It does not parse kind:10002, choose
indexers, or mutate a directory.

For a signed event:

1. the event author's `outbound` relays contribute destinations;
2. operator app relays contribute, always and independently — rule 2 above,
   not a fallback;
3. each decoded `p` tag contributes that recipient's `inbound` relays;
4. a reply contributes one relay where NMP actually observed its direct
   parent, when that parent exists in the canonical store with relay
   provenance;
5. operator fallback relays may top up a settled thin recipient contribution.

The reply contribution has two deliberately separate inputs:

- `ThreadPosition::read` determines the direct parent event id from the signed
  event's NIP-10/NIP-22 rows;
- `RedbStore::query(id)` supplies the canonical row, and only
  `row.provenance.seen` may supply a destination.

The relay cell authored into an `e` tag is never a routing fact. A signature
proves who authored the hint; it does not prove the named relay carried the
parent. If the canonical row has several verified observations, Auto takes
the first relay in normalized sorted order — exactly one, using the same
temporary deterministic choice the canonical `Row` uses for an emitted hint.
Issue #1243's tagging-door record deliberately deferred choosing among several
verified sources; #1378 now owns that policy. A widely replicated parent must
not silently fan one reply out to every source.

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
query. The `AuthorRouteProvider` the application constructed may satisfy it;
`nmp-outbox` is the NIP-65 implementation this workspace ships, and a
third-party algorithm satisfies the same need through the same three
moments. With no provider installed, the need is simply dropped and every
author stays `Unknown`.

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
