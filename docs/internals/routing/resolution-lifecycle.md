---
title: "Routing resolution lifecycle: the queue rewriter"
category: routing
slug: resolution-lifecycle
status: built
date: 2026-07-29
owns:
  - routing as a queue rewriter (Auto entries becoming Explicit lanes)
  - the route-revision log and lane substrate
  - the four resolution moments
  - Auto retirement (`route_complete`) as settled resolution, not delivery
  - routed vs published as separate axes, and the app-state mapping
  - re-spawn suppression, one-receipt-per-intent, and idempotency
  - the on_signed terminal-failure defect and its fix
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/writes/identity.md
issues:
  - "FIXED: a routing error at on_signed used to terminally Fail and drop the intent (§8) — publishing before the first relay-list fetch died permanently; the shipped fix parks as an open Destinations picture instead"
  - "an Auto whose unknowns never settle parks durably forever; cancel is pre-signature only (`CancelWriteError::AlreadySigned`) — observability, not auto-abandon, is the chosen answer (preview-and-observability.md)"
---

# Routing resolution lifecycle: the queue rewriter

This document is the mechanism half of the routing design: what actually
happens, durably, between "an intent carrying `Auto` is accepted" and "every
relay that should hold the event holds it". The contract half — why the app
only ever says `Auto` or `Explicit` — is `auto-and-explicit.md`; how "we don't
know yet" becomes "we know there is nothing" is `knowledge-and-settlement.md`.

Every mechanism this document describes is built and verified against this
worktree. Nine call sites across the engine and its tests cite this document
by section number, in code comments next to the citations reproduced below;
the numbering here matches theirs.

---

## 1. The claim: routing is a queue rewriter

An earlier framing had resolvers as a new kind of actor that could "enqueue
follow-up obligations" — a new capability with its own durability and
idempotency questions, flagged at the time as the riskiest piece of the whole
design. Pablo dissolved it. In full:

> about 3. remember these events, by the time they are attempted to be sent are already signed, so they are idempotent at the relay level; publishing to relay1 event with id 1 twice is completely harmless. We don't want to go overboard so as to not waste bandwidth, but the idempotency is almost not a problem, but yes, they are idempotent in terms of the event id being always there. And I don't know how much of a new thing it really is, perhaps all nip17 routing does is, get the two relays that should receive the event and literally publish the event using the exact same machinery with an explicit relay set of the relays that it resolved. Perhaps that's what routing actually is, just a way to turn an item that has Auto in the queue to something that has Explicit on the queue. For example, say that an event that should be routed via nip17 is in the queue, the user is offline and is missing one of the relays of the parties; it can either realize it's not reaching an indexer relay to retrieve the 10050 so it doesn't consume the Auto entry in the publishing queue: next time the queue drains again it will try again -- or it could not drain the Auto but since it does know one of the relays it needs to publish to, it adds that Explicit relay but keeps the Auto and it will try again.

And, in the same breath, the epistemic register it was offered in — quoted so
nobody mistakes the *confidence* of this document for the tone of the session:

> I don't know, these are just ideas, not sure what's the right approach, but I think this design trends things in more modular and less chaotic, less imperative ways. Happy to entertain other proposals.

The idea survived every subsequent round and shipped. Routing is not a
separate subsystem with its own delivery machinery: it is **the operation that
turns an `Auto` queue item into `Explicit` queue items, incrementally, using
the exact same publish machinery**. A resolver that can only partially answer
does not block and does not fail — it emits what it knows and keeps the `Auto`
alive for the rest. This is exactly what `rewrite_route`
(`crates/nmp-engine/src/core/write.rs:4760`) does; its own doc comment says
so: "This is the queue rewriter (resolution-lifecycle.md §§1-4)."

## 2. The key refinement: lanes, not child intents

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
  (`PublishQueueLaneKey`, `crates/nmp-store/src/lib.rs:1161-1166`).
  Re-executing a resolver that reports an already-known relay collides with
  the existing lane and creates nothing. An acked lane is terminal and
  untouched.
- **One receipt per intent, always** (§6): child intents would each mint a
  receipt, and the app would face N receipts for one logical publish. Lanes
  keep the existing shape — one receipt, per-relay facts streaming through it.
- **Crash safety with no new journal shape**: the route-revision log and lane
  tables already exist and already recover (§3).

## 3. The existing substrate

The queue rewriter's durable substrate — the append-only route-revision log,
per-`(intent, relay)` lanes, and per-relay ack tracking — is not a foundation
waiting on a mechanism above it; §§1, 2, 4 and 5 run directly against it.

**The append-only per-intent route log.**
`record_route_revision`/`recover_route_revisions`
(`crates/nmp-store/src/redb_store/publish_queue_ops.rs:184,268`) maintain an
append-only, ordinal-numbered log of resolved relay sets per intent.
`record_route_revision` refuses to write for an intent that is not open, scans
the intent's existing revisions to compute `last_ordinal + 1`, and commits the
new set atomically; `recover_route_revisions` returns the full history sorted
by ordinal. Revisions are never edited or deleted — a route, once resolved and
committed, is history.

**Resolution at signature time.** `on_signed`
(`crates/nmp-engine/src/core/write.rs:4012-4190`) resolves the intent's
routing against the directory at that moment via `resolve_routes` (§4), then
hands the answer to `apply_route_answer`
(`crates/nmp-engine/src/core/write.rs:4825`), which diffs it against the
intent's durable union, commits any new relays as a revision
(`commit_route_revision`, `crates/nmp-engine/src/core/lane_projection.rs:216`,
wrapping `record_route_revision` above), emits the `WriteFact::Destinations`
picture (§7.2), and — only once the revision is durably committed — bootstraps
lanes from it (`bootstrap_projected_lanes`,
`crates/nmp-engine/src/core/lane_projection.rs:44`, wrapping
`bootstrap_publish_queue_lanes`, `crates/nmp-store/src/redb_store/publish_queue_ops.rs:379`).
Note what order that implies: the revision commit is the durable fact, and
lanes are derived from it — exactly the discipline the rewriter generalizes.
Every resolution moment (§5) funnels through this same `apply_route_answer`,
so there is one place, not four, where a route becomes a lane.

**Resolution at boot, with diff-and-append.** Boot recovery (`recover_on_boot`,
`crates/nmp-engine/src/core/write.rs:1680`) recovers every open intent's
revision history, unions the relays it has ever durably resolved to
(`durable_relays`), re-resolves the routing against the CURRENT directory,
diffs (`answer.relays.difference(&durable_relays)`, `:1904-1907`), and appends
a new revision when the diff is non-empty. **Boot treats routing as a strategy
re-executed against fresh knowledge, with the log absorbing only the delta.**
This loop — re-resolve, diff, append, mint lanes — is the queue rewriter, and
it now runs at all four resolution moments (§5); the terminal-failure defect
this substrate used to trip over at signature time is fixed (§8).

**Per-`(event, relay)` ack tracking.** `handle_write_ack`
(`crates/nmp-engine/src/core/write.rs:5040`) resolves exactly one (event,
relay) pair's pending ack, so "delivered to one of two required relays" is
already representable today. Delivery state was never the missing piece.

## 4. What the resolver re-execution appends

Generalizing §3's loop: at each resolution moment (§5), the engine re-executes
the intent's strategy via `resolve_routes`
(`crates/nmp-engine/src/core/write.rs:4494`) — its resolver for `Auto`, or
verbatim for `Explicit` — obtains the currently-knowable relay set plus a
remaining-unknowns report (`knowledge-and-settlement.md`), and
`apply_route_answer` (`crates/nmp-engine/src/core/write.rs:4825`) diffs it
against the union of all committed revisions and appends a revision for any
new relays — each of which mints a lane through the ordinary machinery.
`Explicit` degenerates exactly as it should: one revision, at first
resolution, verbatim, and no unknowns ever — the rewriter's fixed point.

## 5. The four resolution moments

1. **`on_signed`** (`crates/nmp-engine/src/core/write.rs:4012-4190`). The
   first opportunity: the event's bytes are final, delivery can begin.
2. **Boot recovery** (`recover_on_boot`,
   `crates/nmp-engine/src/core/write.rs:1680`, routing re-resolution at
   `:1897-1926`). Every crash-survivor is re-resolved against the directory
   the new process holds.
3. **`Tick` re-execution** (`tick`, `crates/nmp-engine/src/core/mod.rs:2922`,
   calling `rewrite_open_routes` at `:2941`). Intents with `route_complete ==
   false` are re-resolved on every engine tick via `rewrite_open_routes`
   (`crates/nmp-engine/src/core/write.rs:4988`), so a directory that learned
   something through ordinary ingestion (a 10002 arriving for any reason) is
   consulted without any bespoke wiring.
4. **Wake on need settlement.** `replace_author_routes`
   (`crates/nmp-engine/src/core/mod.rs:2351-2363`, calling
   `rewrite_open_routes` at `:2361`) — the sole neutral author-route mutation
   door — calls the same `rewrite_open_routes` immediately after a
   replacement lands, rather than waiting for the next tick. It re-resolves
   every open intent, not only the ones that declared a need on that specific
   author; diff-and-append makes the extra passes free (§6). This is the
   moment that unparks the offline-DM case: the 10050 arrives (or is settled
   absent), and the parked `Auto` resolves within the same reducer turn.

Moments 3 and 4 overlap by design — 4 is latency, 3 is the safety net.
`rewrite_open_routes`'s own doc comment says exactly this: "Moment 3/4 of the
lifecycle (resolution-lifecycle.md §5) ... Called from the engine tick as a
safety net and immediately after a private author-route replacement as the
latency path." Because resolution is diff-and-append against the revision
log, running a moment "too often" costs a directory read and an empty diff,
never a duplicate lane (§6). Correctness never depends on which moment fired.

## 6. Re-spawn suppression and one receipt per intent

Both properties fall out of §2's lanes-not-children decision; they are listed
separately because they answer the two objections most often raised against
incremental re-resolution.

**Re-spawn suppression.** The dedup key is the lane key `(intent_id, relay)`
(`PublishQueueLaneKey`, `crates/nmp-store/src/lib.rs:1161-1166`).
Re-resolution appends only relays absent from the durable union (§3's boot
diff is the shipped precedent), and a lane that already exists — pending,
in-flight, or acked — is simply not re-minted. An acked lane is terminal and
untouched by any later resolution. No resolver output can cause a second
delivery obligation to the same relay for the same intent.

**One receipt per intent, always.** The app called `publish` once; it holds
one receipt; every per-relay fact (`RelayState`: `Waiting`, `Attempting`,
`Sent`, `Published`, `Rejected`, `AuthFailed`, `GaveUp` —
`crates/nmp-engine/src/publish_queue/mod.rs:181` onward) streams through that
one receipt as lanes progress. Incremental routing changes how many lanes a
receipt fans out to over time; it never changes how many receipts exist. An
app that wants "partially sent" reads the per-relay facts; it never
correlates sibling receipts, because there are none.

## 7. Retirement and the routed/published split

### 7.1 `route_complete` flips on settled resolution, not delivery

An `Auto` is consumed when its resolver has **no remaining unknowns** — never
when every relay acked. Pablo's worked example, in full:

> an outbox can end too; for example, if the user is p-tagging 3 users and only one of them has a 10002 and we know the other two don't have one, once we have the relays we'll publish to for the author's own relay + any app relay + some of the 1-p-tagged-user that did have a 10002 then the outbox item is consumed.

Map it onto the three-valued knowledge model (`knowledge-and-settlement.md`):
author `Present`, p1 `Present`, p2 and p3 `Absent` — zero `Unknown`s remain,
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

The receipt surfaces the two axes without conflating them. What shipped is
`WriteFact::Destinations`
(`crates/nmp-engine/src/publish_queue/mod.rs:337-341`):

```rust
Destinations {
    relays: BTreeSet<RelayUrl>,
    complete: bool,
    awaiting_author_routes: BTreeSet<PublicKey>,
}
```

a superset of the two-field shape once proposed for this section:
`awaiting_author_routes` names, as keys rather than as a rendered sentence,
which authors' neutral outbox presence the open picture is still waiting on
(§7.3) — the WHY behind an incomplete resolution, not just the THAT. It is
emitted on every resolution that changes the picture
(`apply_route_answer`, `crates/nmp-engine/src/core/write.rs:4825`) — new
relays, the `complete` flip, or the waiting set itself changing. Delivery
continues to stream through the existing per-relay facts (§6), unchanged.

### 7.2.1 The two axes advance independently, so the receipt has no stable total order

Stated because it is a real consequence of §7.2 that is easy to miss, and
because it was found the expensive way — by a direct/FFI parity oracle going
red on a loaded CI runner and green on an idle laptop.

Routing completeness advances on **discovery** round-trips; delivery advances
on **delivery** round-trips. Nothing orders those two against each other. So
for any write that still has routing unknowns when its first lane opens, the
`Destinations { complete: true }` that retires its routing can arrive before
or after `Published` purely according to which socket answered first, and two
runs of the identical scenario legitimately produce different receipt ORDERS.

A concrete instance, because it is not an exotic one: a kind:3 contact list
p-tags everyone it names, none of whom need have a known relay list, so an
ordinary follow is exactly this shape.

The consequence for anything that compares receipt streams:

> **Any total-order comparison over a delivery-terminated prefix is
> nondeterministic for a write with routing unknowns.** An observer that needs
> a stable order must terminate on receipt CLOSURE, which is causally after
> both axes — the engine closes an intent only when routing is complete AND
> every lane is terminal (§7.1, and `close_if_all_lanes_terminal`).

Measured refinement (#1019): closure stabilises the stream's **content**, not
its **order**. Both axes have reached a terminal by then, so the set of facts
is determined — but where the routing retirement lands among the delivery
beats still is not. Over twelve runs of the identical parity scenario it
arrived before `awaiting_auth` on one surface and after it on the other, about
half the time. An observer comparing two surfaces must therefore compare each
axis's own order, and must not compare the interleaving — which is not a
weakening, because the interleaving is not a fact about either surface.

Note what this asks of closure in return: an observer that waits for it is
waiting on settlement, so a settlement that can silently fail to fire turns a
bounded wait into a hang. Non-completion is reachable by design (zero
configured indexers means an `Auto` parks forever, `knowledge-and-settlement.md`
§9), so such an observer needs a bound, and on expiry it must name WHICH axis
failed to advance — "timed out" alone sends the next reader down the whole
instrumentation path this rule was found on.

### 7.3 The app-state mapping

| receipt state | app shows |
|---|---|
| `Destinations { complete: false }` (§8) | "determining destinations" — with whatever is already known, and with the authors in `awaiting_author_routes` as the reason it is still open |
| `Destinations { complete: true }`, lanes live | "sending (n of m)" |
| every lane terminal, receipt closed | "sent" (or the per-relay failure detail) |

## 8. The on_signed terminal-failure defect (fixed)

**The defect, historically.** Before the fix, `on_signed`'s routing arm
treated "no relays known for the author yet" as an error: `resolve_routes`
returned a fallible result, and an error at signing time removed the pending
write, emitted a terminal failure, and returned — dropping the intent for
good. The routing strategy errored whenever the directory knew no write
relays for the author yet. Compose the two: publish anything before the
author's first relay-list fetch completed — first run, cold start, offline —
and the write died permanently. The event was signed, journaled, durable; the
app did everything right; the directory was merely young. Boot recovery
handled the same situation more gently — an error there was folded into an
empty current set and moved on — which is how a crash-survivor outlived the
exact condition that killed a write at first signing. That asymmetry was the
tell that the terminal arm was wrong.

**The fix, as shipped.** `resolve_routes`
(`crates/nmp-engine/src/core/write.rs:4494`) is now TOTAL: it returns
`RouteAnswer` directly, with no `Result` and no error arm. Its own doc comment
states the discipline directly (`:4471-4489`): "the engine has not learned
enough yet" is not an error — it is an `Auto` with unknowns, which is the
normal INITIAL state of the queue rewriter
(`docs/internals/routing/resolution-lifecycle.md` §8). A resolution that
yields nothing yields `RouteAnswer::default()`, whose empty relay set and
`complete == false` park the intent (`WriteFact::Destinations { complete:
false, .. }`, §7.3) instead of killing it. `on_signed`'s comment on the first
resolution moment (`:4145-4147`) names what this replaced: an answer that
comes up short "parks and is re-executed at every later moment
(resolution-lifecycle.md §5) rather than killing a durable, already-journaled
obligation." A test now guards this directly
(`crates/nmp/tests/signer_registry_headless.rs:357-358`): "A cold directory is
a reason to WAIT, never a reason to destroy a durable obligation."

Failure remains possible for other reasons: an `Explicit` route to an
unreachable relay still fails per-lane, and a permanently unsatisfiable
`Auto` parks visibly forever (`preview-and-observability.md` owns how that is
surfaced). But "the engine had not learned enough yet" is never again a
terminal verdict.

## 9. Idempotency: bandwidth, not correctness

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
