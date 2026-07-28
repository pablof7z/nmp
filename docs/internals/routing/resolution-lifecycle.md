---
title: "Routing resolution lifecycle: the queue rewriter"
category: routing
slug: resolution-lifecycle
status: designed
date: 2026-07-29
owns:
  - routing as a queue rewriter (Auto entries becoming Explicit lanes)
  - the route-revision log and lane substrate that already exists on master
  - the four resolution moments
  - Auto retirement (`route_complete`) as settled resolution, not delivery
  - routed vs published as separate axes, and the app-state mapping
  - re-spawn suppression, one-receipt-per-intent, and idempotency
  - the on_signed terminal-failure defect this design fixes
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/routing/removed-routes.md
  - docs/internals/writes/identity.md
issues:
  - "DEFECT on master: a routing error at on_signed terminally Fails and drops the intent (§8) — publishing before the first relay-list fetch dies permanently; the design parks as AwaitingRoute instead"
  - "an Auto whose unknowns never settle parks durably forever; cancel is pre-signature only (`CancelWriteError::AlreadySigned`) — observability, not auto-abandon, is the chosen answer (preview-and-observability.md)"
---

# Routing resolution lifecycle: the queue rewriter

This document is the mechanism half of the routing design: what actually
happens, durably, between "an intent carrying `Auto` is accepted" and "every
relay that should hold the event holds it". The contract half — why the app
only ever says `Auto` or `Explicit` — is `auto-and-explicit.md`; how "we don't
know yet" becomes "we know there is nothing" is `knowledge-and-settlement.md`.

**Status is marked per section.** Sections marked BUILT describe master
behaviour at `b99f9d41`, verified against this worktree. Sections marked
DESIGNED describe the settled-but-unbuilt design.

---

## 1. The claim: routing is a queue rewriter — DESIGNED

An earlier framing had resolvers as a new kind of actor that could "enqueue
follow-up obligations" — a new capability with its own durability and
idempotency questions, flagged at the time as the riskiest piece of the whole
design. Pablo dissolved it. In full:

> about 3. remember these events, by the time they are attempted to be sent are already signed, so they are idempotent at the relay level; publishing to relay1 event with id 1 twice is completely harmless. We don't want to go overboard so as to not waste bandwidth, but the idempotency is almost not a problem, but yes, they are idempotent in terms of the event id being always there. And I don't know how much of a new thing it really is, perhaps all nip17 routing does is, get the two relays that should receive the event and literally publish the event using the exact same machinery with an explicit relay set of the relays that it resolved. Perhaps that's what routing actually is, just a way to turn an item that has Auto in the queue to something that has Explicit on the queue. For example, say that an event that should be routed via nip17 is in the queue, the user is offline and is missing one of the relays of the parties; it can either realize it's not reaching an indexer relay to retrieve the 10050 so it doesn't consume the Auto entry in the publishing queue: next time the queue drains again it will try again -- or it could not drain the Auto but since it does know one of the relays it needs to publish to, it adds that Explicit relay but keeps the Auto and it will try again.

And, in the same breath, the epistemic register it was offered in — quoted so
nobody mistakes the *confidence* of this document for the tone of the session:

> I don't know, these are just ideas, not sure what's the right approach, but I think this design trends things in more modular and less chaotic, less imperative ways. Happy to entertain other proposals.

The idea survived every subsequent round and is now settled. Routing is not a
separate subsystem with its own delivery machinery: it is **the operation that
turns an `Auto` queue item into `Explicit` queue items, incrementally, using
the exact same publish machinery**. A resolver that can only partially answer
does not block and does not fail — it emits what it knows and keeps the `Auto`
alive for the rest.

## 2. The key refinement: lanes, not child intents — DESIGNED

Read literally, "adds an Explicit entry to the queue" suggests the `Auto`
spawns *child intents*. It does not, and the distinction carries most of the
design's correctness properties:

**The "Explicit entries" an `Auto` emits are the intent's own durable lanes.**
Re-executing the strategy appends a revision to the intent's route-revision
log (§3), and each newly-revealed relay mints a lane — the existing
per-`(intent, relay)` delivery obligation — through the same machinery every
write already uses. Nothing new is enqueued; the one intent's resolved-relay
set grows.

What this buys, for free, from structures that already exist:

- **Re-spawn suppression** (§6): the lane key is `(intent_id, relay)`
  (`LaneKey`, `crates/nmp-store/src/lib.rs:1106-1109`). Re-executing a
  resolver that reports an already-known relay collides with the existing lane
  and creates nothing. An acked lane is terminal and untouched.
- **One receipt per intent, always** (§6): child intents would each mint a
  receipt, and the app would face N receipts for one logical publish. Lanes
  keep the existing shape — one receipt, per-relay facts streaming through it.
- **Crash safety with no new journal shape**: the route-revision log and lane
  tables already exist and already recover (§3).

## 3. The existing substrate — BUILT

The queue rewriter is not built, but its durable substrate is — roughly half
the mechanism already ships on master, which is why the design is an extension
rather than an architecture.

**The append-only per-intent route log.**
`record_route_revision`/`recover_route_revisions`
(`crates/nmp-store/src/redb_store/outbox_ops.rs:141,211`) maintain an
append-only, ordinal-numbered log of resolved relay sets per intent.
`record_route_revision` refuses to write for an intent that is not open, scans
the intent's existing revisions to compute `last_ordinal + 1`, and commits the
new set atomically; `recover_route_revisions` returns the full history sorted
by ordinal. Revisions are never edited or deleted — a route, once resolved and
committed, is history.

**Resolution at signature time.** `on_signed` resolves the intent's routing
against the directory at that moment
(`crates/nmp/src/core/write.rs:2223-2247`), emits
`WriteStatus::Routed(relays)` (`:2247`), records the revision (`:2296`), and
bootstraps lanes from it (`bootstrap_outbox_lanes`, `:2308`). Note what order
that implies: the revision commit is the durable fact, and lanes are derived
from it — exactly the discipline the rewriter generalizes.

**Resolution at boot, with diff-and-append.** Boot recovery
(`crates/nmp/src/core/write.rs:851-892`) recovers every open intent's revision
history, unions the relays it has ever durably resolved to
(`durable_relays`), re-resolves the routing against the CURRENT directory,
diffs (`current_routes.difference(&durable_relays)`, `:877-880`), and appends
a new revision when the diff is non-empty. **Boot already treats routing as a
strategy re-executed against fresh knowledge, with the log absorbing only the
delta.** This loop — re-resolve, diff, append, mint lanes — is the queue
rewriter; master just runs it at only two moments and handles its errors
wrongly (§8).

**Per-`(event, relay)` ack tracking.** `handle_write_ack`
(`crates/nmp/src/core/write.rs:2631`) resolves exactly one (event, relay)
pair's pending ack, so "delivered to one of two required relays" is already
representable today. Delivery state was never the missing piece.

## 4. What the resolver re-execution appends — DESIGNED

Generalizing §3's loop: at each resolution moment (§5), the engine re-executes
the intent's strategy via its resolver (for `Auto`) or verbatim (for
`Explicit`), obtains the currently-knowable relay set plus a
remaining-unknowns report (`knowledge-and-settlement.md`), diffs against the
union of all committed revisions, and appends a revision for any new relays —
each of which mints a lane through the ordinary machinery. `Explicit`
degenerates exactly as it should: one revision, at first resolution, verbatim,
and no unknowns ever — the rewriter's fixed point.

## 5. The four resolution moments — two BUILT, two DESIGNED

1. **`on_signed`** — BUILT (`crates/nmp/src/core/write.rs:2223-2306`). The
   first opportunity: the event's bytes are final, delivery can begin.
2. **Boot recovery** — BUILT (`:851-892`). Every crash-survivor is re-resolved
   against the directory the new process holds.
3. **`Tick` re-execution** — DESIGNED. Intents with `route_complete == false`
   are re-resolved on the engine's tick, so a directory that learned something
   through ordinary ingestion (a 10002 arriving for any reason) is consulted
   without any bespoke wiring.
4. **Wake on need settlement** — DESIGNED. When a `RouteNeed` an intent
   declared transitions to `Settled` (`knowledge-and-settlement.md` §6), the
   intents that declared it are re-resolved immediately rather than waiting
   for the next tick. This is the moment that unparks the offline-DM case:
   the 10050 arrives (or is settled absent), and the parked `Auto` resolves
   within the same ingestion turn.

Moments 3 and 4 overlap by design — 4 is latency, 3 is the safety net.
Because resolution is diff-and-append against the revision log, running a
moment "too often" costs a directory read and an empty diff, never a duplicate
lane (§6). Correctness never depends on which moment fired.

## 6. Re-spawn suppression and one receipt per intent — DESIGNED (on BUILT structures)

Both properties fall out of §2's lanes-not-children decision; they are listed
separately because they answer the two objections most often raised against
incremental re-resolution.

**Re-spawn suppression.** The dedup key is the lane key `(intent_id, relay)`
(`crates/nmp-store/src/lib.rs:1106-1109`). Re-resolution appends only relays
absent from the durable union (§3's boot diff is the shipped precedent), and a
lane that already exists — pending, in-flight, or acked — is simply not
re-minted. An acked lane is terminal and untouched by any later resolution.
No resolver output can cause a second delivery obligation to the same relay
for the same intent.

**One receipt per intent, always.** The app called `publish` once; it holds
one receipt; every per-relay fact (`Sent`, `Acked`, `Rejected`,
`RetryEligible`, `GaveUp`, `OutcomeUnknown` — `crates/nmp/src/outbox/mod.rs:32`
onward) streams through that one receipt as lanes progress. Incremental
routing changes how many lanes a receipt fans out to over time; it never
changes how many receipts exist. An app that wants "partially sent" reads the
per-relay facts; it never correlates sibling receipts, because there are none.

## 7. Retirement and the routed/published split — DESIGNED

### 7.1 `route_complete` flips on settled resolution, not delivery

An `Auto` is consumed when its resolver has **no remaining unknowns** — never
when every relay acked. Pablo's worked example, in full:

> an outbox can end too; for example, if the user is p-tagging 3 users and only one of them has a 10002 and we know the other two don't have one, once we have the relays we'll publish to for the author's own relay + any app relay + some of the 1-p-tagged-user that did have a 10002 then the outbox item is consumed.

Map it onto the three-valued knowledge model (`knowledge-and-settlement.md`):
author `Known`, p1 `Known`, p2 and p3 `KnownAbsent` — zero `Unknown`s remain,
so the resolver's answer can never change again for this intent, so
re-executing it is pointless, so the `Auto` retires. `route_complete` is a
statement about *knowledge exhaustion*, not about success: an intent can be
fully routed with every lane still undelivered, and (transiently) delivering
on some lanes while routing is still incomplete.

Note the two clauses that make even outbox — the open-ended-looking strategy —
retire: "we know the other two don't have one" requires settled negative
knowledge (that is the entire subject of `knowledge-and-settlement.md`), and
the answer is evaluated against the p-tag set frozen in the signed event, which
never changes. Retirement is per-intent and reachable.

### 7.2 Routed vs published are separate axes

Pablo, ruling on whether a routed-but-undelivered write is "done":

> 3. yeah, once we know "this event goes in relay 1, 2 and 3" it's been routed; it might have not been published and it sits on the publishing queue, but it's been routed. Whether you consider that "done" it depends on your position; it's done in terms of routing.

The receipt surfaces the two axes without conflating them. Master's
`WriteStatus::Routed(BTreeSet<RelayUrl>)` (`crates/nmp/src/outbox/mod.rs:51`)
becomes:

```rust
Routed { relays: BTreeSet<RelayUrl>, complete: bool }
```

emitted on every resolution that changes the picture — new relays, or the
`complete` flip. Delivery continues to stream through the existing per-relay
facts, unchanged.

### 7.3 The app-state mapping

| receipt state | app shows |
|---|---|
| `AwaitingRoute` (§8), or `Routed { complete: false }` | "determining destinations" (with whatever is already known) |
| `Routed { complete: true }`, lanes live | "sending (n of m)" |
| every lane terminal, receipt closed | "sent" (or the per-relay failure detail) |

## 8. The defect this design fixes — BUILT (and wrong)

Verified at `crates/nmp/src/core/write.rs:2223-2244`: when `resolve_routes`
errors at `on_signed`, the `Some(Err(reason))` arm (`:2229`) **removes the
pending write, emits `WriteStatus::Failed(reason)`, and returns — a terminal
failure that drops the intent**. `AuthorOutbox` errors whenever the directory
knows no write relays for the author
(`:2599-2600` — `"no write relays known for author …"`).

Compose the two: **publish anything before the author's first relay-list fetch
completes — first run, cold start, offline — and the write dies permanently.**
The event is signed, journaled, durable; the app did everything right; the
directory was merely young. A durability-plane write is killed by a transient
knowledge gap. (Boot recovery handles the same situation more gently —
`resolve_routes(...).unwrap_or_default()` at `:876` treats an error as an
empty current set and moves on — which is how a crash-survivor outlives the
exact condition that would have killed it at first signing. The asymmetry is
accidental, and it is the tell that the terminal arm is wrong.)

Under this design that arm is unrepresentable. "No relays known" is not an
error — it is an `Auto` with unknowns, which is the *normal initial state* of
the queue rewriter. The intent parks as `AwaitingRoute { detail }` (a
retained, replayed-on-reattach receipt state — the routing sibling of
`AwaitingCapability`'s durable park, `crates/nmp/src/outbox/mod.rs:41-49`),
its needs are declared (`knowledge-and-settlement.md` §6), and moments 3/4
re-resolve it when knowledge arrives. Failure remains possible — an `Explicit`
to an unreachable relay still fails per-lane, and a permanently unsatisfiable
`Auto` parks visibly forever (`preview-and-observability.md` owns how that is
surfaced) — but "the engine had not learned enough yet" is never again a
terminal verdict.

## 9. Idempotency: bandwidth, not correctness — DESIGNED

The re-execution model means the same event can, across process lifetimes and
resolver reruns, be offered to the same relay more than once. Pablo's ruling
(quoted in full in §1) settles the class:

> remember these events, by the time they are attempted to be sent are already signed, so they are idempotent at the relay level; publishing to relay1 event with id 1 twice is completely harmless.

By the time anything reaches the wire the event is signed and its id is fixed;
a relay receiving a duplicate id deduplicates it. **Bandwidth, not
correctness, is the only concern** — "We don't want to go overboard so as to
not waste bandwidth" — and the lane key (§6) plus diff-and-append (§3) already
bound the waste: at most one redundant offer per (intent, relay) per ambiguity
window (e.g. a crash between wire write and ack), which is the floor any
at-least-once delivery system pays. No dedup machinery beyond what exists is
justified.

## 10. Rules of thumb

1. **Routing rewrites the queue; it never delivers.** If a routing change
   touches transport code, the layering is wrong.
2. **Lanes, not child intents.** One publish, one receipt, N lanes. A design
   that mints sibling receipts has left the rails.
3. **`route_complete` means "nothing left to learn", never "delivered".**
   Retirement is knowledge exhaustion; delivery is the lanes' business.
4. **Re-resolution must be safe at any frequency.** Diff against the revision
   log plus the `(intent_id, relay)` lane key make extra runs cost an empty
   diff. Preserve that property in every extension.
5. **"Not enough knowledge yet" parks; it never fails.** §8 is what happens
   when that rule is violated — the failure mode shipped.
6. **Duplicate sends are a bandwidth bug, not a correctness bug.** Signed
   events are idempotent at the relay. Do not build correctness machinery
   against a cost problem.
