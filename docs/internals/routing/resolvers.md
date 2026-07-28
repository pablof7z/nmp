---
title: Route resolvers — the contract, registration, and needs
category: routing
slug: resolvers
status: designed
date: 2026-07-29
owns:
  - the RouteResolver trait contract and its result vocabulary
  - how a strategy is chosen (kind at resolution time, registry, outbox fallback)
  - runtime registration on the engine, and its shipped precedents
  - why resolvers DECLARE needs and the engine drives the query
  - the dependency direction for crates that ship resolvers
  - why there is NO reserved-kind table, and the two honest consequences
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/routing/removed-routes.md
  - docs/internals/writes/event-builder.md
  - docs/internals/nip29/group-publication.md
  - docs/internals/conventions/naming-no-invented-categories.md
issues:
  - "a kind-1059 under Auto with no nip17 resolver routes by outbox rules — accepted, §6.2"
  - "the h-under-Auto refusal dies with the table — accepted, §6.2"
---

# Route resolvers — the contract, registration, and needs

`Auto` routing (`auto-and-explicit.md`) means "figure out how to route
whatever I'm publishing." Resolvers are the figuring: pluggable strategies,
owned by protocol crates, executed fresh at every resolution moment. This
document is the contract — what a resolver is, how one is chosen, how it gets
data, and what NMP deliberately refuses to police.

**Status is marked per section.** BUILT sections describe current master
(`b99f9d41`); DESIGNED sections are settled design from the 2026-07-28/29
session, not yet implemented.

---

## 1. The contract — DESIGNED

```rust
pub trait RouteResolver {
    /// The kinds this resolver claims under Auto.
    fn claimed_kinds(&self) -> Vec<Kind>;

    fn resolve(&self, subject: RouteSubject, ctx: &RouteContext) -> RouteResolution;
}

pub enum RouteResolution {
    /// Zero unknowns remain. The Auto obligation retires (settled
    /// RESOLUTION, not successful delivery — resolution-lifecycle.md).
    Resolved { relays: BTreeSet<RelayUrl> },
    /// Some relays known now (they get lanes immediately), some knowledge
    /// still missing. The Auto entry stays; `needs` feeds discovery.
    Partial { relays: BTreeSet<RelayUrl>, needs: Vec<RouteNeed> },
    /// Nothing routable yet. The intent parks as AwaitingRoute; `needs`
    /// feeds discovery so it can unpark.
    Unresolved { needs: Vec<RouteNeed> },
    /// This event must not be routed by this resolver at all (typed reason).
    Refused,
}
```

`RouteSubject { kind, tags, author: Option<PublicKey> }` — deliberately not
`&Event`, so preview and send share one derivation and cannot drift
(`preview-and-observability.md` §2). `RouteNeed` is a full `Filter`, not a
`{author, kind}` pair, so future resolvers are not forced into
relay-list-shaped needs. `RouteContext` exposes the directory's three-valued
knowledge (`Known` / `KnownAbsent` / `Unknown`) and a
`NeedState::{Pending, Settled}` per prior need — `Settled` means the
discovery sources reached EOSE, so a miss is definitive absence
(`knowledge-and-settlement.md`).

Resolvers are pure functions of `(subject, ctx)`. They hold no state between
drains: everything durable lives in the intent's journal and lanes, so a
crash loses nothing and a restart simply re-resolves
(`resolution-lifecycle.md`).

---

## 2. Strategy selection: the KIND, at resolution time — DESIGNED

At each resolution moment the engine takes the event's kind and asks the
registry. Registry hit → that resolver. No hit → the built-in outbox resolver
(`outbox.md`). That is the entire dispatch.

Selection happens at **resolution time, not acceptance time** — the intent
durably stores only that it is `Auto` (the journal stores a label, never a
resolved set), and the strategy is re-derived and re-executed at every send
attempt: first try, after a crash, when the offline queue drains. Pablo:

> I think it's absolutely not required, all that should we might say is what routing to use: "nip17", "outbox", "nip29", "explicit" (meaning the app says explicitly which relays to use and that is that no matter what else happens),  "draft" (yes, nmp-draft should provide its own routing)... whatever and at publish time, including after a crash, when the app is restarted or whatever and we decide that we want to publish the stuff or when the app comes back online and the unpublished event queue starts getting drained calculations according to whatever routing has been decided is performed

And the app never names the strategy:

> I don't know if these "nip17" or "nip29" or "drafts" label are even needed or not, ideally that's not something the app has to concern itself with and it's either "figure it out how to route whatever I'm publishing" or "use these exact relays"; but the idea is that at publish time things just work

---

## 3. Registration: runtime, on the engine — DESIGNED, with BUILT precedent

Resolvers register on the engine at runtime, mirroring the two capability
doors that already exist and ship:

- `Engine::add_signer` (`crates/nmp/src/engine.rs:619`)
- `Engine::add_auth_policy` (`crates/nmp/src/engine.rs:647`)

`add_route_resolver` follows the same shape: the protocol crate constructs
the resolver, the app hands it to the engine, the engine owns it thereafter.
Nothing is discovered by linkage magic; if the app never registers a
resolver, the registry is empty and everything routes by outbox. That is not
an oversight — it is §6.

---

## 4. Resolvers declare needs; the ENGINE drives the query — DESIGNED

A resolver that lacks knowledge (a recipient's kind:10050, a relay list) does
not fetch it. It returns the need, and the engine folds it into the existing
discovery machinery. This was settled in two steps, and both of Pablo's
statements matter because the second corrects an over-reading of the first.

First, resolvers genuinely do need the network:

> 2. the routing that happens in nip17, per this example, when the app comes online it sees "oh, we need to route this dm through nip17", nip17 the says "ok, which one are the relays of the parties? ah, ok, relay 1 and 2". So yes, in that sense, the nip17 crate must perform a query from the network; of course.

Then, when that was misread as "the crate performs its own querying":

> Yes, of course I didn't mean that nip17 should introduce a separate querying approach, parallel to the existing querying system! I meant that any routing crate should use the querying system to retrieve the data they need!

The querying system's own contract already forbids the alternative.
`sync_discovery` — the engine-owned internal kind:10002 discovery
subscription — documents itself (`crates/nmp/src/core/query.rs:449-457`):

> Deliberately reuses the ordinary resolver subscribe/unsubscribe
> machinery rather than hand-rolling a parallel subscription system:
> the discovery atom this produces (`kinds:[10002], authors:{covered}`)
> is just another entry in `resolver.active_demand()`, so the router's
> EXISTING discovery-kind eligibility is what routes it to the
> configured indexers -- no router-side change was needed for that half
> at all.

Route needs simply widen that mechanism's needed set:
`needed = f(wire_demand) ∪ route_needs`. Three properties make declared
needs strictly better than resolver-driven fetching, and each is a failure
mode avoided:

- **Statelessness survives crashes.** Needs are re-derived from the intent on
  every drain. A resolver holding an in-flight fetch across a crash would
  need its own durability story; a declared need has none to lose.
- **Dedup across intents is free.** Ten parked DMs to one recipient
  contribute one entry to the discovery set. Ten resolver-owned fetches are
  ten subscriptions — exactly the fan-out pathology the subscription plane
  spent a week killing (`docs/internals/subscriptions/identity-grouping-and-limits.md` §3.4).
- **One subscription system.** Every REQ the engine emits flows through the
  demand/router/budget pipeline, so relay subscription budgets, coalescing,
  and diagnostics see discovery traffic like any other. A parallel path would
  be invisible to all three.

---

## 5. Dependency direction — DESIGNED, with BUILT precedent

Crates that ship resolvers must read engine-adjacent state shapes
(`RouteContext`, directory knowledge), so they depend on `nmp`. That is the
`nmp-nip65` direction, already shipped: `crates/nmp-nip65/Cargo.toml`
declares `nmp.workspace = true`, and `nmp` does not depend back — no cycle.

Pure schema crates that only compose events stay engine-free: `nmp-nipc7`
depends on `nostr` alone (`crates/nmp-nipc7/Cargo.toml`), `nmp-nip22` on
`nostr` + `nmp-grammar` (`crates/nmp-nip22/Cargo.toml`). The dividing line is
lookups: a crate that must READ state to compute routing (`nip17`, a future
wiki crate) takes the `nmp` edge; a crate that owns only event shape returns
builders and takes none.

`nmp-nip29` needs neither a resolver nor the `nmp` edge: a group's host is
not derivable from the event (`h` carries the group id, never the relay), so
group routing is `Explicit([host])` minted by the `Group` door — see
`docs/internals/nip29/group-publication.md`.

---

## 6. There is NO reserved-kind table — DESIGNED, deliberately

### 6.1 The ruling

A reserved-kind table in `nmp-grammar` (1059/4 → DM, any `h` → group) was
proposed so that a DM would fail closed when its crate is not linked. Pablo
rejected it:

> 1. no table; if the crate isn't linked the app isn't sending DMs

`nmp-grammar` gets zero NIP kind knowledge. `Auto` is: registered resolver
claiming the kind, else outbox. Nothing else exists.

### 6.2 The two honest consequences

Stated here so nobody rediscovers them and mistakes them for oversights:

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

### 6.3 The governing principle

An app CAN hand-roll a gift wrap with the builder (which must be able to
express anything — `writes/event-builder.md`) and publish it under `Auto`.
That was put to Pablo directly as a residual risk, and his answer is the
principle that governs every "should NMP prevent this?" question in the
routing plane:

> are you saying that an app could hand roll their own giftwrap and yolo it? Yes, of course! preventing that would require making nmp impossible to work with! if a developer wants to shoot themselves on the foot we need to let them do that, we can provide guardrails, but at a certain point we'd be introducing more harm by adding restrictions than not.

NMP provides guardrails — typed doors, previews, refusals with reasons — and
does not police. Restrictions that would require NMP to know every NIP's
semantics to enforce are the harm, not the protection. Do not reintroduce
kind tables, tag bans, or "safety" refusals in the neutral plane on the
strength of a scenario this ruling already covers.
