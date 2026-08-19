---
title: "Write routing: Auto and Explicit"
category: routing
slug: auto-and-explicit
status: built
date: 2026-07-29
owns:
  - the app-facing routing contract (`WriteRouting { Auto, Explicit }`)
  - why routing is a durable strategy, not a resolved relay set
  - the reversal that made single-relay publishing first-class
  - routing's independence from authorship
  - what `Explicit` inherits from guarantee #6
related:
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/writes/event-builder.md
  - docs/internals/nip29/group-publication.md
  - docs/internals/conventions/no-backwards-compatibility.md
issues:
  - "the current three `WriteRouting` variants (`AuthorOutbox`, `PrivateNarrow`, `RelayListBootstrap`) were deleted with no aliases, per conventions/no-backwards-compatibility.md; see migration commit 0b6996bb (#1105/#1163)"
---

# Write routing: Auto and Explicit

This is the app-facing contract for how a write chooses its relays. It was
settled in the design session of 2026-07-28/29 with the repository owner
(Pablo), and it replaces the entire earlier routing vocabulary with exactly
two values, shipped in commit `0b6996bb` (#1105/#1163):

```rust
pub enum WriteRouting {
    Auto,                      // "figure out how to route whatever I'm publishing"
    Explicit(Vec<RelayUrl>),   // "use these exact relays and that is that"
}
```

Nothing else. No `nip17` label, no `nip29` label, no `draft` label, no
bootstrap variant, no privacy variant. The mechanics of *executing* these two
values — resolvers, the queue-rewriting lifecycle, knowledge settlement — live
in the sibling documents; this one records why the surface is exactly this
shape and no other, because the shape was arrived at through a genuine
reversal and the argument must not be re-litigated.

---

## 1. What exists today

`WriteRouting` (`crates/nmp-grammar/src/write.rs:384-408`, referenced from 74
files) has exactly two variants:

- `Auto` — "figure out how to route whatever I'm publishing." NMP derives the
  route from the event at send time; the caller names no relay and no
  strategy.
- `Explicit(Vec<RelayUrl>)` — "use these exact relays and that is that, no
  matter what else happens."

At the FFI boundary, `FfiWriteRouting` mirrors it exactly
(`crates/nmp-ffi/src/types.rs:656-661`): `Auto` and `Explicit { relays: Vec<String> }`.
`Explicit` is app-constructible on every platform — Swift and Kotlin apps can
build it directly.

Three earlier variants — `AuthorOutbox`, `PrivateNarrow(PrivateRoute)`, and
`RelayListBootstrap(RelayListBootstrapAuthority)` — are gone. No trace of any
of the three remains in `crates/`. There was no migration, no alias, and no
deprecation window — see `conventions/no-backwards-compatibility.md`, and
Pablo's ruling that put this in motion:

> no backwards compatibility!!!! I told you this so many times!!!

---

## 2. Routing is a STRATEGY, re-executed at every send opportunity

The single most important property: **the intent stores *how to decide*, not
*where to send*.** A routing value is durable; a relay set is a moment's
answer. At every send opportunity — the first attempt, after a crash, when the
app comes back online and the unpublished queue drains — the strategy is
executed fresh against whatever the engine knows at that moment. Pablo, in
full:

> I think it's absolutely not required, all that should we might say is what routing to use: "nip17", "outbox", "nip29", "explicit" (meaning the app says explicitly which relays to use and that is that no matter what else happens),  "draft" (yes, nmp-draft should provide its own routing)... whatever and at publish time, including after a crash, when the app is restarted or whatever and we decide that we want to publish the stuff or when the app comes back online and the unpublished event queue starts getting drained calculations according to whatever routing has been decided is performed, and, for example, the nip17 crate might be able to publish one part of the DM (i.e. publish to one of the required relays but not the two of them) but at least it knows which one is the relay that's missing (i.e. the user sent the DM while the app was totally offline so the app didn't even know the relay of the sender or receiver of the nip17 dms) so it might add to the unpublished queue the missing event with the explicit relay of the missing relay.

Why this matters concretely: a relay set snapshotted at compose time is wrong
in exactly the cases that matter most. On cold start the user's own relay list
has not been fetched yet when the app publishes. A write that parks for hours
while offline outlives the relay list it would have been resolved against.
Resolving late, every time, against engine-owned directory state, is the only
behaviour that is correct in both cases.

This is not a wholly new architecture bolted on afterward — it is what
shipped. `resolve_routes` (`crates/nmp-engine/src/core/write.rs:4494`) is
called from the SEND path, not at compose time: at boot recovery
(`crates/nmp-engine/src/core/write.rs:1903`) and at `on_signed`
(`crates/nmp-engine/src/core/write.rs:4163`), reading engine-owned directory
state at that moment. Per-attempt re-resolution is the architecture (see
`resolution-lifecycle.md` §5).

A consequence worth stating: the strategy stored in the journal is a
**label**. Nothing about resolution logic is ever serialized. The closed
serializable value is `Auto` or `Explicit(relays)`; how `Auto` is answered
lives in the engine, never in the journal.

---

## 3. The app says one of exactly two things

An earlier iteration of the design had the app (or protocol crate) naming a
strategy: `"nip17"`, `"outbox"`, `"nip29"`, `"draft"`. Pablo cut that down in
the same breath he proposed it:

> I don't know if these "nip17" or "nip29" or "drafts" label are even needed or not, ideally that's not something the app has to concern itself with and it's either "figure it out how to route whatever I'm publishing" or "use these exact relays"; but the idea is that at publish time things just work

That sentence is the whole surface. `Auto` is "figure it out how to route
whatever I'm publishing"; `Explicit(relays)` is "use these exact relays". The
strategy names are NMP's internal business, never spelled by the app. Today
`Auto` has exactly one answer, the author's outbox; no per-kind resolver
plane exists to claim a kind (`resolvers.md`). What that feels like from the app side, in Pablo's words:

> the app should be able to say "publish this event" and it would default to using outbox. "publish this dm" and it would use nip17 routing, without the app having to say which are the nip17 relays that should be used -> the nip17 crate provides the relays that need to be used for communication between two pubkeys. "publish this wiki" and nmp-wiki would provide an overridable relay that respects the underlying user's wishes. "publish this kind:1 to this nip29 group" and the nmp-nip29 crate would route to which group (h tag and relays) it belongs to. Or the app might say "publish this event to this exact relay I'm giving you". It just might be, for example, that the user sees a cool event from someone they follow and they right click in their app and decide to publish that exact signed event, as-is (i.e. signed by the user they follow) to their own personal archive relay.

Note what is absent from every clause: the app never carries a relay list it
did not itself originate, never names a NIP, never distinguishes "DM routing"
from "note routing". Protocol crates that own write policy (`nmp-nip29`'s
`Group`, a future `nmp-nip17`) mint one of the same two values — a `Group`
mints `Explicit([host])`, a DM flow rides `Auto` with the nip17 resolver
claiming the kind. There is no third vocabulary for crates; crates and apps
speak the same two words.

---

## 4. The reversal: single-relay publishing is a first-class general capability

This design was reached through a reversal, and the reversal is the part most
worth recording, because a future reader will find the *old* reasoning
persuasive — it was persuasive enough to shape a whole afternoon of API
elimination.

The old premise: letting an app route a write to a chosen relay is a dangerous
primitive that #838/#827 deliberately removed; `FfiWriteRouting = {
AuthorOutbox }` is an invariant to protect; NIP-29's need for host-directed
delivery should therefore be smuggled through a non-app-constructible authority
newtype (`GroupHostAuthority`) so the general capability never exists. Several
API shapes were eliminated on that premise.

Pablo rejected the premise outright, answering the "newtype vs bare variant"
question:

> bare. It is not only overengineering; it's wrong for many other reasons; it's not just nip29 that should say "publish event x to relay y", an app might offer a "publish this event to relay: [_user_input_here_]" -- there are many many reasons to publish to a specific relay instead of outbox. Or for example a nmp-wiki crate that implements wikis might publish to the user's preferred "wiki relays" kind instead of their nip-65/outbox kind. Or a nip-17 crate should publish to the user's DM relays. There's tons and tons of reasons for this besides nip29!

The named cases, spelled out so nobody rebuilds the ban one consumer at a
time:

1. **User-input relay** — an app offering "publish this event to relay:
   `[user input]`".
2. **`nmp-wiki`** — publishing to the user's preferred wiki-relays kind
   instead of their NIP-65/outbox kind.
3. **`nmp-nip17`** — publishing to the DM relays of a *pair* of pubkeys.
4. **`nmp-nip29`** — routing to a group's host relay.
5. **Archive republish** — republishing someone else's already-signed event,
   unchanged, to your own personal archive relay (the right-click case quoted
   in §3).

Consequences: `GroupHostAuthority` has zero references anywhere in `crates/`
— dead, confirmed; the "never let a route become app-visible" line of
argument is dead; `Explicit` is app-constructible, on every platform, as
built.

---

## 5. Routing is independent of authorship

State it plainly, because the old premise quietly assumed the opposite:
**where an event goes has nothing to do with who signed it.**

The archive-republish case is the proof. The user sees an event *signed by
someone they follow*, and publishes that exact signed event, as-is, to their
own archive relay. The route (`Explicit([archive])`) is chosen by the
publishing user; the signature belongs to a different pubkey entirely; no
identity of the publishing user appears anywhere in the payload. Routing
consumed no fact about the author, and authorship consumed no fact about the
route. Any design that derives routing *from* identity, or gates routes *by*
identity, breaks this case — and this case is one of Pablo's own first-class
examples, not an edge.

The two axes meet only in one place: an `Auto` resolver MAY read the author as
an input (outbox resolves the author's write relays; see `outbox.md`). That is
a resolver consuming a fact, not a coupling of the contract. This is exactly
what `resolve_routes`'s `Explicit` arm shows structurally
(`crates/nmp-engine/src/core/write.rs:4499-4507`): it reads only `relays` and
never touches `event.pubkey` — `Explicit` never reads the author at all.

---

## 6. What `Explicit` carries forward from guarantee #6

The structural discipline of the earlier `PrivateNarrow` variant (guarantee
#6's fail-closed property) transfers to `Explicit` intact, and *structurally*
— as properties of the type and the acceptance path, not as conventions:

- **Verbatim execution.** `Explicit(relays)` resolves to exactly those
  relays, every time, no matter what the directory knows or later learns. No
  augmentation, no substitution. `resolve_routes`'s `Explicit` arm
  (`crates/nmp-engine/src/core/write.rs:4499-4507`) reads only `relays` and
  never consults engine-owned directory state.
- **No widen path.** There is no operation anywhere that adds a relay to an
  accepted `Explicit` route — `WriteRouting::Explicit` holds a plain
  `Vec<RelayUrl>` fixed at construction, with no insert/extend/union exposed
  afterward (`crates/nmp-grammar/src/write.rs:384-408`).
- **Empty refused pre-acceptance.** An `Explicit` with zero relays is refused
  *before* acceptance: no intent, no journal row, no receipt lifecycle. This
  is built as `PublishError::EmptyExplicitRoute`, checked first in
  `prepare_publish` ahead of every other door check
  (`crates/nmp-engine/src/core/write.rs:2965-2980`). Pablo's ruling was
  direct — "reject it immediately" — and the code comment at the check site
  quotes it verbatim.

  This is stricter than the design `Explicit` replaced. The old
  `PrivateNarrow` accepted an empty set and only failed closed later, at
  resolution, with `WriteStatus::Failed` — emptiness was a *sentence*, not a
  mistake, saying "I resolved this and there is nowhere safe to send it"
  through the receipt like any other outcome. That expression is gone:
  `Explicit([])` never becomes a durable write at all, so there is no receipt
  for it to arrive on. The reason the stricter rule is safe is that the door
  refusal is at least as informative as the receipt it replaces — the caller
  learns synchronously, before anything durable exists, rather than via a
  parked failure later.

What `Explicit` deliberately does NOT carry: the *privacy* framing.
`PrivateNarrow`'s wording bound a fail-closed mechanism to a privacy
invariant, which is exactly why reusing it for group hosts was rejected in the
NIP-29 exploration — a group host is a public target, and journal snapshots
describing a group write as "private" would be lying. Fail-closed is a routing
property; privacy is one reason among several to want it.

**On resolution failure more generally:** `resolve_routes` returns
`RouteAnswer` directly — there is no error arm. A route that cannot yet be
resolved (for example, publishing before the first relay-list fetch
completes) does not terminally fail; it parks, carrying `author_route_needs`
and `complete == false`, and re-resolves on the next send opportunity (§2).
Pablo's ruling on why acceptance cannot validate reachability up front, and
what an app needs instead, applies here:

> Yeah, a DM that's been published while we were offline and we didn't know a dm relay list for that user doesn't exist it's a failure; there's no way to know that at acceptance time in the same way that we can't know if the user says "when you go online publish this to wss://non-existent.com" -- all we can do is provide via the nip17 crate a "what will this compute to" so that apps can easily show which relays would be used for a certain communication, for example, such that they can disable the send button when a relay cannot be determined for one of the parties. This way of popping up a "we were trying to publish to relay X and it didn't work" or "we were trying to route this event but it didn't work" needs to exist because there are many ways we'll find ourselves there

No route-preview API (`preview_route` or equivalent) exists in `crates/`
today — the "what will this compute to" capability Pablo describes above is
unbuilt. There is no `RouteResolution::Refused` type either; the only refusal
surface that exists is the pre-acceptance `EmptyExplicitRoute` check above.
