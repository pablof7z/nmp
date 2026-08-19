---
title: Route resolvers — the contract, registration, and needs
category: routing
slug: resolvers
status: not built
date: 2026-07-29
owns:
  - why there is NO reserved-kind table, and the two honest consequences
  - the dependency direction for crates that ship routing
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/writes/event-builder.md
  - docs/internals/nip29/group-publication.md
  - docs/internals/conventions/naming-no-invented-categories.md
issues:
  - "a kind-1059 under Auto with no nip17 resolver routes by outbox rules — accepted, §3"
  - "the h-under-Auto refusal dies with the table — accepted, §3"
---

# Route resolvers — what is not built, and the rulings around it

NMP has no pluggable route-resolver contract. `Auto` routing
(`auto-and-explicit.md`) resolves through one hard-coded path:
`resolve_routes` (`crates/nmp-engine/src/core/write.rs:4494`) matches
`WriteRouting::Auto => self.resolve_outbox(event)` and nothing else — there is
no kind→resolver registry, no `RouteResolver` trait, no runtime registration
door for one. The design sketched in earlier drafts of this document was
never built.

Runtime registration of engine capabilities is not itself hypothetical —
`Engine::add_auth_policy` (`crates/nmp/src/engine.rs:507`) and
`Engine::add_private_key_account` (`crates/nmp/src/engine/session.rs:153`)
both ship today, registering capabilities on the engine at runtime the same
way a resolver registry was sketched to. No such registry exists for routing.

---

## 1. Resolvers declare needs; the engine drives the query

Pablo settled, in two steps, what a future routing crate's relationship to
the network should be — the second statement corrects an over-reading of the
first.

First, that routing crates genuinely do need the network:

> 2. the routing that happens in nip17, per this example, when the app comes online it sees "oh, we need to route this dm through nip17", nip17 the says "ok, which one are the relays of the parties? ah, ok, relay 1 and 2". So yes, in that sense, the nip17 crate must perform a query from the network; of course.

Then, when that was misread as "the crate performs its own querying":

> Yes, of course I didn't mean that nip17 should introduce a separate querying approach, parallel to the existing querying system! I meant that any routing crate should use the querying system to retrieve the data they need!

The shipped NIP-65 path is the concrete proof this was followed. Core emits
`Effect::AuthorRouteNeedsChanged` (`crates/nmp-engine/src/core/mod.rs:1251`,
fed by `author_route_needs.rs`); the feature-gated facade owns one ordinary
kind:10002 `LiveQuery` pinned to its operator-selected sources. Core and the
router contain no kind, tag-marker, winner, or source-selection knowledge.

---

## 2. Dependency direction

`nmp-nip65`'s only dependencies are `nostr` and `nmp-grammar`
(`crates/nmp-nip65/Cargo.toml`) — it is engine-free, owning composition,
demand, canonical-winner selection, marker parsing, and coordinator state
without depending on `nmp`. The optional `nmp/nip65` facade depends toward
that pure crate and binds its values to the ordinary engine doors.

`nmp-nip29` currently depends on `nmp` directly (`crates/nmp-nip29/Cargo.toml`
lists `nmp.workspace = true` as an ordinary, non-dev dependency). That crate's
own Cargo.toml comment records the rationale: since #1707 reversed #1033,
`nmp-nip29` owns all of NIP-29's meaning again, and `nmp` supplies custody,
storage, routing, signing, delivery, recovery and receipts underneath it — a
capability crate composing the engine it runs against, the same upward edge
restored for `nmp-nip02` by #1707. A group's host is not derivable from the
event (`h` carries the group id, never the relay); group routing is
`Explicit([host])` minted by the `Group` door
(`docs/internals/nip29/group-publication.md`).

---

## 3. There is no reserved-kind table

A reserved-kind table in `nmp-grammar` (1059/4 → DM, any `h` → group) was
proposed so that a DM would fail closed when its crate is not linked. Pablo
rejected it:

> 1. no table; if the crate isn't linked the app isn't sending DMs

`nmp-grammar` has zero NIP kind knowledge.

Two honest consequences follow, recorded here so nobody rediscovers them and
mistakes them for oversights:

1. **A kind-1059 under `Auto` with no nip17 resolver routes by outbox
   rules.** NMP will not recognise it as a DM, because NMP does not know
   what a DM is.
2. **The h-under-Auto refusal dies with the table.** An `h` tag is just a
   tag; nothing in the neutral plane treats it as a group marker.

Neither arises in practice through the supported doors: an app sending DMs
links the nip17 crate (which is where the gift-wrap crypto lives — the real
barrier is doing NIP-17 cryptography by hand, not the type system), and the
group workflow goes through `Group`, which mints its own `Explicit` route and
its own `h`.

An app can hand-roll a gift wrap with the builder (which must be able to
express anything — `writes/event-builder.md`) and publish it under `Auto`.
That was put to Pablo directly as a residual risk:

> are you saying that an app could hand roll their own giftwrap and yolo it? Yes, of course! preventing that would require making nmp impossible to work with! if a developer wants to shoot themselves on the foot we need to let them do that, we can provide guardrails, but at a certain point we'd be introducing more harm by adding restrictions than not.
