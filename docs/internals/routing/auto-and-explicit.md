---
title: "Write routing: Auto and Explicit"
category: routing
slug: auto-and-explicit
status: designed
date: 2026-07-29
owns:
  - the app-facing routing contract (`WriteRouting { Auto, Explicit }`)
  - why routing is a durable strategy, not a resolved relay set
  - the reversal that made single-relay publishing first-class
  - routing's independence from authorship
  - what `Explicit` inherits from guarantee #6, and what dies with the old variants
related:
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/writes/event-builder.md
  - docs/internals/nip29/group-publication.md
issues:
  - "publishing before the first relay-list fetch terminally fails today (see resolution-lifecycle.md §8) — the design parks instead"
  - "the current three `WriteRouting` variants are all deleted by this design; no aliases survive"
---

# Write routing: Auto and Explicit

This is the app-facing contract for how a write chooses its relays. It was
settled in the design session of 2026-07-28/29 with the repository owner
(Pablo), and it replaces the entire current routing vocabulary with exactly two
values:

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

**Status is marked per section.** Sections marked BUILT describe master
behaviour at `b99f9d41`, verified against this worktree. Sections marked
DESIGNED describe the settled-but-unbuilt design.

---

## 1. What exists today — BUILT

`WriteRouting` on master (`crates/nmp-grammar/src/write.rs:207-228`) has three
variants:

- `AuthorOutbox` — resolves to the author's NIP-65 write relays and nothing
  else (`crates/nmp/src/core/write.rs:2591-2603`), erroring when none are
  known.
- `PrivateNarrow(PrivateRoute)` — guarantee #6's fail-closed narrow route. Its
  `NarrowOnly` set is populated once at construction and structurally exposes
  no widen/insert operation afterward (`crates/nmp-grammar/src/write.rs:230`
  onward); an empty set fails closed at resolution
  (`crates/nmp/src/core/write.rs:2605-2613`) and never falls back to a public
  relay.
- `RelayListBootstrap(RelayListBootstrapAuthority)` — a finite validated set
  minted only by the NIP-65 module, executed verbatim and directory-blind
  (`crates/nmp/src/core/write.rs:2615-2622`).

The routing value is serialized to a string and parsed back
(`routing_snapshot`/`parse_routing_snapshot`,
`crates/nmp/src/core/write.rs:2424-2485`) so durable writes replay after a
crash. At the FFI boundary, `FfiWriteRouting` has exactly ONE variant,
`AuthorOutbox` (`crates/nmp-ffi/src/types.rs:560-562`) — no Swift/Kotlin app
can construct either of the other two.

Every one of these three variants dies under this design. There is no
migration, no alias, and no deprecation window. Pablo's words:

> no backwards compatibility!!!! I told you this so many times!!!

---

## 2. Routing is a STRATEGY, re-executed at every send opportunity — DESIGNED

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

This is not a wholly new architecture. On master, `resolve_routes` is already
called from the SEND path, not at compose time — at boot recovery
(`crates/nmp/src/core/write.rs:875`) and at `on_signed` (`:2226`), reading
`self.directory` at that moment. Per-attempt re-resolution already IS the
architecture; the design extends it to more moments (see
`resolution-lifecycle.md` §5) rather than inventing it.

A consequence worth stating because it eliminated a whole design branch: the
strategy stored in the journal is a **label**, and resolvers are looked up at
send time. Nothing about resolution logic is ever serialized. The earlier
constraint "routing must be a closed, serializable value — never a closure,
trait object, or resolver callback" dissolves: the closed serializable value is
`Auto` or `Explicit(relays)`, and everything dynamic lives behind the resolver
registry (`resolvers.md`), never in the journal.

---

## 3. The app says one of exactly two things — DESIGNED

An earlier iteration of the design had the app (or protocol crate) naming a
strategy: `"nip17"`, `"outbox"`, `"nip29"`, `"draft"`. Pablo cut that down in
the same breath he proposed it:

> I don't know if these "nip17" or "nip29" or "drafts" label are even needed or not, ideally that's not something the app has to concern itself with and it's either "figure it out how to route whatever I'm publishing" or "use these exact relays"; but the idea is that at publish time things just work

That sentence is the whole surface. `Auto` is "figure it out how to route
whatever I'm publishing"; `Explicit(relays)` is "use these exact relays". The
strategy names are NMP's internal business — which resolver claims a kind is
decided by the resolver registry at send time (`resolvers.md`), never spelled
by the app. What that feels like from the app side, in Pablo's words:

> the app should be able to say "publish this event" and it would default to using outbox. "publish this dm" and it would use nip17 routing, without the app having to say which are the nip17 relays that should be used -> the nip17 crate provides the relays that need to be used for communication between two pubkeys. "publish this wiki" and nmp-wiki would provide an overridable relay that respects the underlying user's wishes. "publish this kind:1 to this nip29 group" and the nmp-nip29 crate would route to which group (h tag and relays) it belongs to. Or the app might say "publish this event to this exact relay I'm giving you". It just might be, for example, that the user sees a cool event from someone they follow and they right click in their app and decide to publish that exact signed event, as-is (i.e. signed by the user they follow) to their own personal archive relay.

Note what is absent from every clause: the app never carries a relay list it
did not itself originate, never names a NIP, never distinguishes "DM routing"
from "note routing". Protocol crates that own write policy (`nmp-nip29`'s
`Group`, a future `nmp-nip17`) mint one of the same two values — a `Group`
mints `Explicit([host])`, a DM flow rides `Auto` with the nip17 resolver
claiming the kind. There is no third vocabulary for crates; crates and apps
speak the same two words.

---

## 4. The reversal: single-relay publishing is a first-class general capability — DESIGNED

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

Consequences, so the dead reasoning stays dead: `GroupHostAuthority` is dead;
the "never let a route become app-visible" line of argument is dead; the
`check-nip29-ownership.sh` clauses guarding against a host-authority type were
guarding against a capability that should exist, and are revised deliberately
rather than evaded (see `nip29/group-publication.md`). `Explicit` is
app-constructible, on every platform, by design.

---

## 5. Routing is independent of authorship — DESIGNED

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
a resolver consuming a fact, not a coupling of the contract — `Explicit` never
reads the author at all, and `Auto` for a group-host or pinned-target shape
would not either.

---

## 6. What `Explicit` carries forward from guarantee #6 — DESIGNED

`PrivateNarrow` dies, but the structural discipline it embodied does not.
Guarantee #6's fail-closed property transfers to `Explicit` intact, and
*structurally* — as properties of the type and the acceptance path, not as
conventions:

- **Verbatim execution.** `Explicit(relays)` resolves to exactly those relays,
  every time, no matter what the directory knows or later learns. No
  augmentation, no substitution. This is today's `PrivateNarrow` execution
  rule (`crates/nmp/src/core/write.rs:2570-2613` — "never consults the
  directory at all") generalized to the non-private case.
- **No widen path.** There is no operation anywhere that adds a relay to an
  accepted `Explicit` route — the same absence of insert/extend/union that
  `NarrowOnly` enforces by construction today
  (`crates/nmp-grammar/src/write.rs:230` onward).
- **Empty refused pre-acceptance — owner ruling, and it deletes something
  deliberate.** An `Explicit` with zero relays is refused *before* acceptance:
  no intent, no journal row, no receipt lifecycle. Pablo's ruling was direct —
  "reject it immediately".

  This is stricter than master, and the difference is not an oversight being
  corrected. Today an empty `PrivateNarrow` is accepted and then fails closed
  at resolution with `WriteStatus::Failed`
  (`crates/nmp/src/core/write.rs:2605-2613`), and guarantee #6 built that on
  purpose — `NarrowOnly`'s own doc says an empty set "is exactly how an
  unroutable private recipient is expressed structurally"
  (`crates/nmp-grammar/src/write.rs:230-238`). Emptiness was a *sentence*, not
  a mistake: it said "I resolved this and there is nowhere safe to send it",
  and it said so through the receipt like any other outcome.

  That expression is removed here, so the meaning needs somewhere else to live,
  and it has two homes:

  - **On the resolver path**, `RouteResolution::Refused(reason)` says it, with
    a reason string the empty set never carried
    (`docs/internals/routing/resolvers.md`).
  - **On the app path**, `preview_route`'s `blocked` field says it *before* the
    app tries to publish (`docs/internals/routing/preview-and-observability.md`).

  So an app that resolves a recipient to nothing does not publish an empty
  route and wait for a receipt to fail — it learns from the preview and does
  not publish at all. The reason the stricter rule is safe is that the
  replacement channels are strictly more informative: an empty `Vec` cannot
  explain itself, and both of its successors can.

What `Explicit` deliberately does NOT carry: the *privacy* framing.
`PrivateNarrow`'s wording binds a fail-closed mechanism to a privacy
invariant, which is exactly why reusing it for group hosts was rejected in the
NIP-29 exploration — a group host is a public target, and journal snapshots
describing a group write as "private" would be lying. Fail-closed is a routing
property; privacy is one reason among several to want it. The full accounting
of what each removed variant's callers migrate to.

---

## 7. The failure this contract accepts, and where it surfaces — DESIGNED

Two-word contracts have a cost, and it was priced in explicitly rather than
discovered later. `Explicit` executes verbatim — including verbatim nonsense.
Pablo, ruling on exactly this:

> 4. Yeah, a DM that's been published while we were offline and we didn't know a dm relay list for that user doesn't exist it's a failure; there's no way to know that at acceptance time in the same way that we can't know if the user says "when you go online publish this to wss://non-existent.com" -- all we  can do is provide via the nip17 crate a "what will this compute to" so that apps can easily show which relays would be used for a certain communication, for example, such that they can disable the send button when a relay cannot be determined for one of the parties. This way of popping up a "we were trying to publish to relay X and it didn't work" or "we were trying to route this event but it didn't work" needs ato exist because there are many ways we'll find ourselves there

So: acceptance does not validate reachability or existence, because it cannot.
What the design provides instead is (a) a first-class preview
(`preview_route`) so an app can show — or disable send on — what a route
*would* compute to, and (b) durable, visible failure observability for the
writes that route or deliver into a wall. Both live in
`preview-and-observability.md`; they are the contract's other half, not
optional extras.

---

## 8. Rules of thumb

1. **The app says "figure it out" or "these exact relays." Nothing else,
   ever.** A third routing word appearing in an API review is a design
   regression against a settled ruling.
2. **A routing value is a strategy. Resolving it yields a moment's answer.**
   Never store the answer where the strategy belongs.
3. **Routing and authorship are separate axes.** The archive-republish case is
   the standing counterexample to any coupling.
4. **Single-relay publishing is general.** NIP-29 is one consumer of it, not
   its justification — and not its gatekeeper.
5. **Fail-closed transfers; privacy framing does not.** `Explicit` executes
   verbatim, cannot widen, and refuses empty pre-acceptance. Calling that
   "private" was `PrivateNarrow`'s category error; do not reintroduce it.
