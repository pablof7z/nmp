---
title: Removed routes — a tombstone record
category: routing
slug: removed-routes
status: designed
date: 2026-07-29
owns:
  - every WriteRouting variant and route authority the settled design deletes
  - the never-built GroupHostAuthority / GroupHost, and why it died twice
  - the honest history of #838's PinnedHost removal and its deliberate reversal
  - the check-nip29-ownership.sh ban whose premise is now dead, and what replaces it
  - check-routing-vocabulary.sh, the cross-API owner of the two-word rule
  - AuthorRelayList(Kind) — proposed, died unbuilt
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/nip29/group-publication.md
  - docs/internals/conventions/no-backwards-compatibility.md
  - docs/internals/writes/payload-and-replaceable-edits.md
issues:
  - "#838 removed PinnedHost(HostAuthority); its conclusion is deliberately reversed here"
  - "#1105 moved the tombstones into a cross-API gate and proved the group door end to end"
---

# Removed routes — a tombstone record

This document exists so nothing here comes back by accident. Every name below
was either shipped and is deleted by the settled routing design, or was
designed during the 2026-07-28/29 session and killed before it was built.
Each entry records the reasoning, because a deletion whose reasoning is lost
is a deletion waiting to be re-litigated — and one of these (#838's) was a
CORRECT decision whose premise later changed, which is exactly the history a
tombstone has to keep straight.

Per `conventions/no-backwards-compatibility.md`, deletions are total: no
aliases, no deprecation window, no compatibility decoders. Pablo:

> no backwards compatibility!!!! I told you this so many times!!!

**Status markers:** BUILT-TODAY means the name still exists on master
(`b99f9d41`) and the design deletes it; NEVER-BUILT means it existed only in
design; HISTORICAL means it was already removed from master before this
session.

---

## 1. The three shipped `WriteRouting` variants — BUILT-TODAY, deleted by the design

`WriteRouting` on master (`crates/nmp-grammar/src/write.rs:209-227`) is:

- `AuthorOutbox` (`write.rs:209`)
- `PrivateNarrow(PrivateRoute)` (`write.rs:211`)
- `RelayListBootstrap(RelayListBootstrapAuthority)` (`write.rs:227`)

with their carrier types `NarrowOnly` (`write.rs:240`), `PrivateRoute`
(`write.rs:264`), and `RelayListBootstrapAuthority` (`write.rs:277`), and
their journal spellings in `routing_snapshot` / `parse_routing_snapshot`
(`crates/nmp/src/core/write.rs:2424`, `:2446` — `"author-outbox"`,
`"private-narrow-hex:"`, `"nip65-bootstrap-hex:"`).

All of it goes. The replacement grammar is two variants — `Auto` and
`Explicit(relays)` (`auto-and-explicit.md`) — because every one of the three
was a special case of something the resolver model expresses generally:

- **`AuthorOutbox`** becomes the built-in outbox resolver under `Auto`
  (`outbox.md`) — and gains the p-tag fan-out and app relays the enum
  variant never had.
- **`PrivateNarrow`** was "exactly these relays, directory-blind, fail
  closed when empty." That IS `Explicit`. What survives is not the variant
  but its invariants — ledger #6 carries over structurally: `Explicit` is
  verbatim execution, there is no widen path anywhere in its resolution, and
  an empty set is refused pre-acceptance rather than resolved to nothing.
- **`RelayListBootstrap`** existed to deliver a kind:10002 before the
  author's relay list was known — a route minted by the NIP-65 module from a
  validated finite set. Under the new grammar that is `Explicit` minted by
  `nmp-nip65`, the same "protocol crate mints an exact route" pattern the
  `Group` door uses; no dedicated variant earns its place.

Do not reintroduce a per-policy enum variant. The whole point of the
resolver registry is that new routing policies arrive as registered
resolvers or minted `Explicit` sets, never as grammar surgery.

---

## 2. `GroupHostAuthority` / `WriteRouting::GroupHost` — NEVER-BUILT, killed twice

Mid-session design produced `WriteRouting::GroupHost(GroupHostAuthority)`: a
single-host route whose authority newtype was mintable only from a validated
`GroupPublication`, so an arbitrary write could not be aimed at an arbitrary
relay. It died twice, and the two deaths matter separately.

**First death — the newtype.** Asked "`GroupHostAuthority` newtype vs bare
`GroupHost(RelayUrl)`", Pablo answered with the passage that reversed the
whole premise, quoted in full because it is the charter for `Explicit`:

> bare. It is not only overengineering; it's wrong for many other reasons; it's not just nip29 that should say "publish event x to relay y", an app might offer a "publish this event to relay: [_user_input_here_]" -- there are many many reasons to publish to a specific relay instead of outbox. Or for example a nmp-wiki crate that implements wikis might publish to the user's preferred "wiki relays" kind instead of their nip-65/outbox kind. Or a nip-17 crate should publish to the user's DM relays. There's tons and tons of reasons for this besides nip29!

The newtype's entire value was making single-relay routing hard to reach.
Once single-relay routing is a first-class general capability, guarding it is
not protection, it is a defect.

**Second death — the variant itself.** With `Explicit(relays)` general,
`GroupHost` is a redundant spelling of `Explicit([host])`. The group host is
not derivable from the event (`h` carries the group id, never the relay), so
`nmp-nip29` needs no resolver either: the `Group` door mints
`Explicit([host])` and appends `h` (`nip29/group-publication.md`). Pablo, on
where that logic lives:

> that OBVIOUSLY belongs in the nmp-nip29 crate! nmp doesn't know about ANY of this, but the nip29 crate does!

Nothing group-shaped exists in the routing grammar. Keep it that way.

---

## 3. `PinnedHost(HostAuthority)` — HISTORICAL, removed by #838, and the honest story

### 3.1 What #838 did and why it was right

PR #838 records that `nmp-nip29` had collapsed three owners into itself, and
that "The universal write plane then retained a `PinnedHost(HostAuthority)`
route whose only production constructor was that invalid composer." #838
deleted the composer family and took `PinnedHost`/`HostAuthority` with it,
concluding:

> With `PinnedHost` removed and supported native routing limited to
> `AuthorOutbox`, no supported general-purpose or NIP-29 operation can
> currently route an arbitrary write to one selected relay.

**That conclusion was correct at the time.** The only producer of the route
was a composer that hardcoded the wrong schema and minted transplantable
authority; a single-host route reachable solely through an invalid door was
a liability, and deleting the door meant the route had no legitimate reason
to remain. #838 was not wrong about anything it could see.

### 3.2 What changed

The premise — that "route an arbitrary write to one selected relay" is a
capability with no legitimate general demand — is what Pablo reversed in §2's
quote. The user-typed relay, the wiki crate, the DM crate, and the
archive-republish case (someone else's already-signed event, unchanged, to
your own relay — which also proves routing is independent of authorship) are
all legitimate single-relay writes with nothing NIP-29 about them. So the
capability returns as `Explicit`, deliberately, as a general primitive — not
as a NIP-29 concession, and not as a quiet un-doing of #838: what #838
removed (the invalid composer, the mintable authority, the collapsed
ownership) stays removed.

### 3.3 The gate clause with a dead premise

`scripts/check-nip29-ownership.sh` enforces #838's conclusion with a ban
whose comment states its premise outright (`check-nip29-ownership.sh:90-92`):

> With no supported NIP-29 write operation, the universal write plane must
> not retain a speculative single-host route.

and greps `HostAuthority|PinnedHost` across grammar/nmp/ffi
(`check-nip29-ownership.sh:103-108`), plus requires the legacy
`pinned-host-hex` snapshot spelling to survive exactly once, in the restart
falsifier that proves an old obligation is retained unreadable
(`check-nip29-ownership.sh:94-101`,
`crates/nmp/tests/durable_accepted_restart.rs`).

The premise is now dead: there IS a supported NIP-29 write operation, and
the write plane deliberately carries a general single-relay route. The
`HostAuthority|PinnedHost` clause and its comment are deleted with the
design — those names must simply never return (§2), and a grep for names
nobody proposes is not a tripwire, it is sediment. **What replaces it is
positive pins on what the reversal must NOT have loosened:**

- group publication crosses the app API only through the `Group` door —
  the app never spells `Explicit([host])` for a group and never touches `h`
  (`nip29/group-publication.md`);
- `nmp-nip29` still cannot depend on the engine crates (the ownership half
  of the gate survives untouched);
- `Explicit` is verbatim, widen-free, and refuses empty pre-acceptance
  (ledger #6's structure, §1).

The rest of the gate — chat-schema ownership, `previous` minting, the
removed native API — is untouched by routing and stays.

### 3.4 Who owns the tombstones now (#1105)

`check-nip29-ownership.sh` carried the routing-vocabulary clause for one
release as a name-only grep over three Rust source trees plus the two SDK
sources. That was never enough for what §1–§4 claim: a grep proves a name is
absent, not that the surviving vocabulary is exactly two words, and it did
not scan `GroupHost` or `AuthorRelayList` at all — the two never-built names
this document tombstones hardest.

`scripts/check-routing-vocabulary.sh` (#1105) owns the whole contract, for
the whole domain, in one place:

- **cardinality by enumeration, per projection** — the `nmp-grammar` enum, the
  `nmp-ffi` mirror, BOTH public FFI conversion directions, the Swift enum and
  the Kotlin sealed class each declare exactly `Auto` and `Explicit`. Because
  the sets are exact, "it names no NIP and no strategy" needs no rule of its
  own: there is no third name left to be one. A third word appearing on ONE
  SDK — the failure a Rust-only test cannot see — is caught here;
- **every retired spelling**, including `GroupHost` and `AuthorRelayList`,
  absent from every tree an app or SDK can reach, with the failure message
  naming the replacement (`Auto`, or `Explicit` minted by whichever crate);
- **the group door** — no group write operation takes a relay or a routing
  value, stated as a signature.

The gate has its own falsifier, `scripts/test-check-routing-vocabulary.sh`,
which restores a retired spelling and adds a third variant to each projection in
turn and requires each mutation to go red. The runtime half — the app
supplies content only, the host alone receives, the author's discovered
outbox is never contacted — is
`crates/nmp/tests/group_publication_door.rs`, because no static check can
observe a delivery.

---

## 4. `AuthorRelayList(Kind)` — NEVER-BUILT, died on the merits

A proposed middle variant: "resolve the author's list of kind K at send
time" (`AuthorRelayList(10002)` for outbox, a wiki-relays kind for wikis).
It bought exactly two things: resolution deferred to send time against
engine-owned directory state (cold start), and re-resolution for a write
that parks while the relay list changes.

It died unbuilt because `Auto` plus resolvers delivers both, without a third
grammar variant: deferred resolution is the entire `Auto` lifecycle
(strategy re-executed at every drain, `resolution-lifecycle.md`), and
"which kind's list" is resolver policy — the wiki resolver reads the wiki
kind, outbox reads 10002 — not routing grammar. Every case it covered was
the author's OWN lists; every two-party or foreign-host case needed
`Explicit` anyway, so the variant was a partial spelling of `Auto` with the
kind hoisted into the enum. Do not resurrect it when a new "the user's
preferred X relays" kind appears: that is a resolver, or a crate minting
`Explicit`.
