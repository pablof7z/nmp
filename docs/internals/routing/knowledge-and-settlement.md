---
title: "Routing knowledge and settlement: absence versus ignorance"
category: routing
slug: knowledge-and-settlement
status: designed
date: 2026-07-29
owns:
  - the three-valued knowledge model (`Known` / `KnownAbsent` / `Unknown`)
  - the EOSE ruling — how "we don't know yet" becomes "we know there is nothing"
  - where absence is derived (discovery, once) and where it is read (directory facts)
  - "`RouteNeed(Filter)`" and "`NeedState { Pending, Settled }`"
  - why needs union into `sync_discovery`, never a parallel query system
  - why absence facts are session-scoped and never persisted
  - the fail-closed consequence of zero configured indexers
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/routing/removed-routes.md
issues:
  - "an author with no relay-list event at all stays Unknown forever on master — `knows_write_relays` flips only on ingest (§4); this is the precise gap the EOSE rule closes"
  - "with zero indexers configured, nothing ever settles and Auto entries park durably (§9) — fail-closed by design, but an app-configuration foot-gun that preview-and-observability.md must keep visible"
---

# Routing knowledge and settlement: absence versus ignorance

The queue rewriter (`resolution-lifecycle.md`) retires an `Auto` when its
resolver has no remaining unknowns. That sentence quietly relies on the
hardest fact in the whole routing design: the engine must be able to say
**"user X has no relay list"** as a settled, positive piece of knowledge,
distinct from **"we have not looked yet."** If absence and ignorance are
indistinguishable, no `Auto` with an absent input ever retires, and the
3-p-tag outbox case — Pablo's own worked example of retirement — is
unreachable.

This document is the account of how "we don't know yet" becomes "we know
there is nothing": what already distinguishes the two on master, where the
gap is, exactly which mechanism closes it, and the boundaries deliberately
drawn around that mechanism so it stays small.

**Status is marked per section.** Sections marked BUILT describe master
behaviour at `b99f9d41`, verified against this worktree. Sections marked
DESIGNED describe the settled-but-unbuilt design.

---

## 1. Three-valued knowledge — DESIGNED

For any routing input (an author's kind:10002, a party's kind:10050, any
future fact a resolver reads), the directory's answer is one of exactly three
values:

| value | meaning | resolver behaviour |
|---|---|---|
| `Known` | the fact exists and here it is | use it |
| `KnownAbsent` | discovery ran to completion and the fact does not exist | proceed without it — this input is *resolved*, contributing nothing |
| `Unknown` | discovery has not completed for this input | keep the `Auto` alive; declare a need (§6) |

The load-bearing distinction is `KnownAbsent` vs `Unknown`. Both look
identical from a collapsed `Vec` (empty either way), and a two-valued model
inevitably encodes one of them as the other: either absence looks like
ignorance (nothing ever retires) or ignorance looks like absence (a cold-start
resolver silently under-routes, which for a DM is a privacy failure). Three
values, or wrong.

Pablo's 3-p-tag retirement example maps exactly: author `Known` + p1 `Known` +
p2/p3 `KnownAbsent` → zero `Unknown`s → the resolver's answer is final → the
`Auto` retires (`resolution-lifecycle.md` §7.1).

## 2. The EOSE ruling — DESIGNED

What earns the transition from `Unknown` to `KnownAbsent`? Pablo's ruling,
exactly:

> and "do we have a 10002 for these three users" is very knowable: the moment we receive EOSE from the indexer relays we use we know, one way or another, whether we have a 10002 or not.

EOSE is NIP-01's "end of stored events": the relay asserts it has sent
everything matching the subscription's filter. When every indexer relay the
engine queries for a discovery filter has EOSE'd and no matching event
arrived, the engine holds a *positive*, relay-attested fact: **these sources
have nothing**. That is the definition of `KnownAbsent` — not a timeout, not a
retry budget, not a heuristic. The raw material already exists on master:
per-relay EOSE is observed and surfaced as `ObservationFact::RelayEose`
(`crates/nmp/src/core/observation.rs:74`, emitted at `:363`).

Scope honesty, stated once: `KnownAbsent` means "absent from the indexer
relays we use", not "absent from the universe" — no relay protocol can attest
the latter. That bound is acceptable because the same indexer set is what
discovery would keep querying anyway; a fact those sources don't hold is a
fact this engine cannot act on regardless of what it calls the state.

### 2.1 "EOSE" names the ruling, not the mechanism — BUILT (#1019)

The ruling is about *finishing*: a source has said everything it has. EOSE is
what that looks like over NIP-01, and it is what the ruling reaches for
because it is the only terminal NIP-01 offers. It is not the only terminal
this engine has, and taking the name literally cost a bug.

Two facts, both measured rather than argued:

- **On a NIP-77 relay the discovery filter never rides an ordinary REQ at
  all.** The live-first handoff (#563) sends a `limit:0` barrier — which
  attests nothing, the relay sends no events by construction — and asks the
  real question inside a negentropy session. `NEG-DONE` with nothing left to
  fetch is that source finishing, and it is the *only* finishing signal that
  path ever produces. Settling only on EOSE meant a NIP-77-capable indexer
  could never settle an absence, deterministically, on every run.
- **Coalescing means the request that carries the question is usually not
  shaped like the question.** The first discovery pass really is sent as
  `kinds:{3,10002}`, because the contact list and the relay list are wanted
  for the same person at the same moment. "Is this the relay-list request?"
  is therefore a containment test, never an equality test on `kinds`.

So the settlement event is: **the terminal signal of the request that actually
asked** — EOSE for an ordinary REQ, negentropy completion for a reconciled
one (deferred to the backfill's own EOSE when reconciliation proved ids were
missing, since one of them may be the kind:10002 in question).

The corollary is the shape of the state. What must be durable is the
*question*, recorded from the filter that went on the wire and keyed by the
subscription id it went out under — never re-derived from the router plan.
The plan describes what the engine is asking *now*; a question is about what
it asked *then*, and coalescing rewrites the former constantly while the
latter is still in flight. A question is discharged by exactly one terminal
signal and forgotten when its request is abandoned or its session drops.
Forgetting settles nothing: an abandoned question has been answered by
nobody, and crediting it would be the same fail-open as treating "nowhere to
ask" as "asked, nothing there" (§9).

## 3. What exists today, and the precise gap — BUILT

Master already distinguishes *half* of this.
`RelayDirectory::knows_write_relays` (`crates/nmp-router/src/facts.rs:119-140`)
answers "has this directory ever recorded a write-relay FACT for this author"
— explicitly documented as "known, possibly zero" vs "never resolved". An
author whose current kind:10002 declares ZERO write relays is `true` (known),
distinct from an author never ingested at all (`false`). `LiveDirectory` backs
it with a real per-author ingestion record (override at `facts.rs:497`), and
`sync_discovery` already consults it to stop discovery for a known-empty
author (`crates/nmp/src/core/query.rs:474`) instead of keeping the
subscription open forever.

So "declares zero relays" is already settled knowledge. **The gap:**
`knows_write_relays` only flips on INGEST — the sole write door is
`ingest_write_relays` (`facts.rs:157`), fed when a kind:10002 event actually
arrives. An author who has never published a relay-list event at all produces
no event, therefore no ingest, therefore stays `false` — **`Unknown` forever,
no matter how many relays have EOSE'd having said nothing.** Master can know
"they said zero"; it cannot know "they never said anything, and we checked."
That second state is precisely what the EOSE rule supplies, and it is the only
missing transition.

(In three-valued terms: master's boolean is `Known` vs not-`Known`; the design
splits not-`Known` into `KnownAbsent` and `Unknown` using EOSE as the
attestation.)

## 4. Absence is derived once, then read as a directory fact — DESIGNED

Where does the EOSE-to-absence derivation live? Not in resolvers. The settled
shape:

**Discovery derives absence ONCE, at the moment its sources settle, and caches
it into the directory via a new door — `settle_relay_list_absent(author)` —
the negative sibling of `ingest_write_relays`.** From then on the fact is an
ordinary directory answer. Resolvers read only the directory's three-valued
answer; **they never see EOSE, `SourceStatus`, or `reconciled_through`.**

The rejected alternative was threading read-side acquisition evidence into
resolution — handing resolvers the per-source machinery
(`SourceStatus`/`reconciled_through`, `crates/nmp/src/core/evidence.rs`) so
each could decide settlement itself. Rejected because it makes every resolver
re-derive (and possibly disagree on) the same epistemic judgment, couples the
write plane to read-side evidence types, and turns "is it settled?" from a
cached fact into a per-resolution computation. One derivation, one cache, one
reader interface. The write plane's knowledge surface stays exactly as wide as
the directory trait.

A later ingest of a real kind:10002 for a settled-absent author simply
overwrites: `ingest_write_relays` replaces whatever was held, absence
included. `KnownAbsent` is a cache of "nothing existed as of settlement",
never a tombstone.

## 5. Needs ride the existing query system — BUILT substrate, DESIGNED extension

How does an unknown input become a discovery query? An early sketch had the
nip17 resolver "performing a query from the network", which risked reading as
a resolver-owned query path. Pablo confirmed the network fetch is required:

> 2. the routing that happens in nip17, per this example, when the app comes online it sees "oh, we need to route this dm through nip17", nip17 the says "ok, which one are the relays of the parties? ah, ok, relay 1 and 2". So yes, in that sense, the nip17 crate must perform a query from the network; of course.

— and then corrected, hard, the over-reading that this meant new machinery:

> Yes, of course I didn't mean that nip17 should introduce a separate querying approach, parallel to the existing querying system! I meant that any routing crate should use the querying system to retrieve the data they need!

The existing querying system already contains the exact mechanism:
**`sync_discovery`** (`crates/nmp/src/core/query.rs:458`) — the engine-owned
internal kind:10002 subscription for authors whose write relays are unknown.
Its own doc comment (`query.rs:449-457`) states the design rule verbatim:
"Deliberately reuses the ordinary resolver subscribe/unsubscribe machinery
rather than hand-rolling a parallel subscription system" — the discovery atom
is just another entry in `resolver.active_demand()`, the router's existing
discovery-kind eligibility routes it to the configured indexers, and the
subscription widens as the needed set grows (`query.rs:498-515`) and tears
down when it empties (`query.rs:477-489`).

The design's only change: the needed set gains a second contributor.

```
needed = f(wire_demand) ∪ route_unknowns
```

Today `needed` is derived purely from read-side wire demand
(`query.rs:463-475`); under the design, the unknowns declared by parked routes
(§6) union in. Same subscription, same widen/teardown lifecycle, same indexer
routing, same ingestion path — and the ingestion path is what fires the
wake-on-settlement resolution moment (`resolution-lifecycle.md` §5). Nothing
parallel exists for resolvers to misuse, which is the correction made
structural. Declared needs are also STATELESS — re-derived on each
resolution pass from the intents that still have unknowns — so a crash loses
nothing (boot re-resolves every open intent, re-declaring every live need)
and dedup across intents wanting the same fact is free set union.

## 6. `RouteNeed(Filter)` and `NeedState` — DESIGNED

The need a resolver declares is a full filter, not a purpose-built shape:

```rust
pub struct RouteNeed(pub Filter);

pub enum NeedState {
    Pending,   // discovery sources have not all settled for this filter
    Settled,   // sources EOSE'd — whatever the store now holds is the answer
}
```

`RouteNeed` was deliberately generalized from an earlier `{author, kind}` pair
so future resolvers are not boxed into relay-list shapes — a resolver whose
missing input is, say, a group's metadata event or an addressable
parameterized record declares it in the same vocabulary the whole query system
already speaks (`Filter` is the lingua franca of demand). For the built-in
outbox resolver the need is exactly `sync_discovery`'s existing atom shape
(`kinds:[10002], authors:{...}`), so the generalization costs the common case
nothing.

`NeedState::Settled` is the EOSE ruling as a type: settled means the
discovery sources for that filter have all EOSE'd, so **a miss against the
local store is now definitive absence**, not a pending fetch. `Settled` +
event present → `Known`; `Settled` + no event → `KnownAbsent` (cached per §4);
`Pending` → `Unknown`, keep parking. Resolvers receive need states through
their resolution context and — per §4 — never anything lower-level.

## 7. Absence facts are session-scoped, never persisted — DESIGNED

Stated as a position, not an open question: **`KnownAbsent` lives in the
in-memory directory for the life of the process and is never written to the
store.**

Why this is right, and not merely cautious:

- **Restart re-probes.** A fresh process re-runs discovery for whatever its
  parked routes and live demand need; if the absent fact is still true, it is
  re-derived within one EOSE round-trip. The cost of forgetting is one
  discovery query per restart — small, bounded, and paid exactly when the
  answer could have changed.
- **Staleness is self-limiting.** Absence is the one fact that is *expected*
  to change (the whole point of a user publishing their first relay list is to
  stop being absent). A persisted absence fact would be a bet against the
  user's own future action, growing staler the longer it is held — the exact
  opposite of a relay-list event, whose persistence is anchored to a
  replaceable-event winner with an author-signed timestamp.
- **No TTL is invented.** Persisting absence forces an expiry policy —
  how long is "no relay list" trustworthy? — and any number chosen is a made-up
  freshness contract with no protocol grounding. Session scope answers the
  question by deleting it: the fact lives exactly as long as the discovery
  round that derived it is plausibly current.

The asymmetry with positive facts is deliberate: a kind:10002 is durable
protocol data with a canonical winner and belongs in the store; "we looked and
found nothing" is an observation about a session's sources and belongs to the
session.

## 8. The dependency: resolvers, needs, and linked crates — DESIGNED

Settlement interacts with the resolver registry (`resolvers.md`) in one way
worth pinning here. Pablo's ruling on missing protocol crates:

> 1. no table; if the crate isn't linked the app isn't sending DMs

An `Auto` only ever declares the needs of the resolver that claims it. There
is no engine-side catalog of "kinds that would need settlement if their crate
were present" — a kind with no registered resolver routes by the built-in
outbox rules, declaring outbox's needs. Settlement machinery never has to
reason about protocols that are not linked; the knowledge model's inputs are
always supplied by a resolver that actually exists in the process.

## 9. The fail-closed consequence: zero indexers, nothing settles — DESIGNED

`sync_discovery`'s atoms are routed by the router's discovery-kind eligibility
to the **configured indexers** (`query.rs:453-457` — and "indexers are never a
content fallback" cuts the other way too: content relays are not a discovery
fallback). The EOSE rule needs sources to EOSE. Compose the two:

**With zero indexers configured, no discovery query has any source, no source
ever EOSEs, no need ever settles, no absence is ever derived — and every
`Auto` with an unknown input parks as `AwaitingRoute`, forever.**

This is fail-closed and it is correct: an engine with no discovery sources
*cannot know*, and the alternative — treating "nowhere to ask" as "asked,
nothing there" — would silently under-route every outbox write and misdeliver
every DM on a misconfigured app. The park is durable, visible, and carries its
reason (`resolution-lifecycle.md` §8); `preview-and-observability.md` owns
making the stall loud (`stalled_writes` in diagnostics) so a configuration
error reads as "every write is stuck determining destinations — and here is
why" rather than as a mystery. But the design deliberately refuses to invent
knowledge it does not have. Nothing settles because nothing *can* settle, and
the type system says so rather than a timeout pretending otherwise.

## 10. Rules of thumb

1. **Absence is knowledge; ignorance is not.** `KnownAbsent` retires unknowns;
   `Unknown` parks them. Collapse the two and either nothing retires or
   cold-start under-routes.
2. **EOSE from the sources we use is the settlement event.** Not timeouts, not
   retry counts. If a proposal needs a timer to declare absence, it is
   guessing.
3. **Derive absence once; read it as a directory fact.** Resolvers never see
   EOSE, `SourceStatus`, or `reconciled_through`. If a resolver signature
   grows an evidence parameter, the boundary in §4 has been breached.
4. **Needs are filters, and they ride `sync_discovery`.** No parallel query
   path, per Pablo's explicit correction. A resolver that opens its own
   subscription is a defect.
5. **Never persist "we found nothing."** Restart re-probes. A stored absence
   fact needs a TTL, and every TTL here is invented.
6. **Fail closed on missing sources.** Zero indexers means parked writes, not
   guessed routes. Make the park loud; do not make the guess.
