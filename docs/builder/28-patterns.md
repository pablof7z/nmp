# Guarantees and the bugs they exclude

This chapter is the map of the bug classes NMP's design structurally excludes,
and the mechanism that excludes each. The numbers are stable identifiers: design
docs and code comments cite them. [Current implementation
status](03-status-map.md) and [known gaps](../known-gaps.md) record what ships.

The old NMP relied on broad doctrine and lints. The rewrite excludes a bug class
by making the bad path unreachable on the supported facade — most often by there
being no API that can express it, so no caller can ask for it and no reviewer
has to notice.

## Core structural guarantees

### #1: one canonical store mutation path

**Excludes:** stale replaceable winners and duplicate ids with lost provenance.

Exact-id dedup, provenance merge, and replaceable arbitration happen behind the
store door. There is no public index or storage setter — that door is the only
way an event enters the store. Apps do not maintain a second
profile/list/event cache or decide which candidate wins.

### #2: demand-derived subscription lifetime

**Excludes:** app-opened REQs that leak or close while another observer still
needs them.

Apps own query-handle scope. NMP owns reference counts, compilation, REQ open /
close, and surgical dependency updates. There is no public open-REQ verb.

### #3: typed relay authority

**Excludes:** a generic `relays:` escape hatch that bypasses routing policy.

Raw reads and writes do not take app-expanded relay arrays. Indexers are typed
operator policy. A protocol operation may contribute a closed contextual fact,
such as a NIP-29 group host relay, but cannot register an arbitrary route
closure.

### #4: capped fan-out with visible shortfall

**Excludes:** unioning every discovered relay into an unbounded connection set.

Every whole-demand cap and uncovered portion is explicit; a two-relay objective
is never presented as met when available facts or the cap prevented it.

### #5: dedup with provenance

**Excludes:** one visual row per relay or loss of the evidence describing where
an event was observed.

The canonical event id identifies one row. Duplicate arrival merges source
provenance before downstream semantics.

### #6: private routes cannot widen

**Excludes:** falling back from a private/narrow protocol route to public
relays.

Narrow route types have no widen operation. A protocol module that cannot
resolve a required private route fails closed with typed evidence.

### #7: source evidence cannot claim global truth

**Excludes:** treating an empty cache or one relay's EOSE as proof that no
matching event exists anywhere.

The snapshot carries rows plus compact per-planned-source acquisition and
shortfall facts. Apps interpret those facts; NMP exposes no
`synced`, `syncHealth`, global `complete`, or `authoritativeEmpty` state.

### #8: negentropy requires a proved capability

**Excludes:** sending NIP-77 messages to an unprobed relay.

Only the prober can mint `ProbedRelay`; the negentropy effect requires that
token. A relay's NIP-11 advertisement is separate evidence and can never mint
it. Other relays use REQ.

### #9: durable acceptance is not convergence

**Excludes:** a publish return value being mistaken for relay success.

`Accepted` is emitted only after atomic persistence of the frozen body, expected
author, intent, receipt, and canonical pending row. ACK, rejection, and retry
remain separate facts. Facts about the whole write and facts about one relay
live on different arms of the write fact, so "is this over?" is answered once
rather than re-derived by each consumer.

### #10: accepted writes cannot drift to another signer

**Excludes:** an account/current-pubkey change reassigning an already accepted
unsigned write.

Publish defaults to the signer registered for `$currentPubkey`, permits an
explicit identity override, and pins the selected expected author at
acceptance. Missing capability becomes durable `AwaitingSigner`, not silent
reassignment.

### #11: apps do not own derived expansion

**Excludes:** app code watching one query, caching its projected set, and
manually repairing another subscription.

`Derived` and `SetOp` remain inside the engine's closed graph. Reusable helpers
return the same printable graph; they do not receive expanded-set callbacks.
Changing `$currentPubkey` reroots only dependent graphs. Literal multi-account
queries remain live. This describes the resolver's demand graph specifically;
the row-projection layer that turns that graph into `RowDelta`s currently
re-derives every open observation and history on each switch, literal
multi-account queries included (#1646).

### #12: core has no presentation policy

**Excludes:** one app's date, name, truncation, ranking, or plaintext display
policy becoming shared infrastructure.

Core and modules emit raw protocol-semantic values. Crypto providers may
decrypt protocol data, but presentation remains downstream in the app/UI.

## Extended v2 guarantees

### #13: acquisition and presentation cursors stay distinct

**Excludes:** a late-arriving old-timestamped event being skipped because a UI
pagination cursor already passed it.

Windowing is a policy on the one read noun: `observe(query, window)` maintains
the complete bounded canonical partition ordered by an exact exclusive
`(created_at DESC, event_id ASC)` cursor the engine owns. Growth is declarative
and monotonic — `requestRows(atLeast:)` states the total you want — so the host
never constructs, holds, or replays a protocol cursor or continuation token, and
there is nothing to go stale. Acquisition cursors are separate engine state.
Durable cursor resume and a global end verdict are deliberately not offered.

### #14: schema ownership is not contextual authority

**Excludes:** a module claiming a foreign content kind merely because its
protocol publishes that draft in a context.

A NIP module owns only its exact schemas. NIP-29 may add its `h` tag and group
host context to a NIP-C7 chat draft without owning kind:9. Core
validates the immutable composition and signs once.

### #15: pending writes use ordinary query semantics

**Excludes:** an optimistic overlay or direct write-to-observer lane diverging
from the store.

The canonical row carries `Pending | Signed(signature)` and
participates in normal filters, derived bindings, replacement, delete, expiry,
persistence, and invalidation.

### #16: exactly one retry owner per domain

**Excludes:** transport, signer adapter, and delivery independently resending the
same obligation.

Transport reconnects sockets; a signer adapter owns one correlated operation;
publish queue owns each `(intent, relay)` attempt; one deadline scheduler
owns time and concurrency.

### #17: limits cannot silently truncate

**Excludes:** first-N substitution presented as the requested result.

Every graph, wire, relay, observer, and result limit must preserve exact
semantics, return explicit shortfall, reject with a type, or backpressure with a
diagnostic reason. Every projection and interior queue must prove the bound end
to end.

### #18: source/identity cannot borrow evidence incorrectly

**Excludes:** equal filters under different AUTH or read routing sharing a
watermark as though they were the same request.

Descriptor identity is `Selection + ReadRouting + AuthenticateAs`.
Selection work may share; wire demand and evidence share only after a
compatibility proof. Every nested `Derived` demand carries its own explicit
source/identity; it cannot inherit or borrow the outer demand's evidence.

### #19: event/delivery persistence cannot become a secret vault

**Excludes:** raw signing material being stored beside event and retry state.

Rust persists obligations and expected pubkeys. Standard platform providers
own secure secret storage; apps own identity policy and may supply custom
providers.

### #20: replaceable edits are engine-materialized operations

**Excludes:** an app reading a replaceable event, editing the whole value, and
overwriting a newer version it never saw.

An edit is an engine-issued, capability-bound operation that NMP materializes
over the current source, retains across restart, and reapplies when newer source
truth arrives. There is no caller-facing compare-and-swap payload to lose a race
against; a capability defines its own first value, and a cache miss never creates
one. See [Editing replaceable events](15-editing-replaceable.md); the residual
limits of the coordinate gate are in [known gaps](../known-gaps.md).

### #21: a live store cannot be double-opened or deleted underneath itself

**Excludes:** two engines writing one database file, or a destructive reset
unlinking a file another process still owns.

Opening a persistent store takes cross-process ownership of the resolved target
before the database initializes, and holds it for the store's life. That lock is
mandatory: a platform that cannot provide it fails closed rather than degrading
to unlocked operation, so relative, symlink, and hard-link aliases resolve to one
authority rather than one lock each. Destructive reset joins the same ownership
and holds it through removal, so no check-then-delete gap exists. Violations are
typed `StoreAlreadyOpen`/`StoreStillOpen` refusals on every SDK, never a doc
comment.

### #22: there is no route-override noun to register

**Excludes:** a global registry mapping event kinds to route policies, and the
"who owns this kind" arbitration such a registry would require.

Route resolution happens inside the layer that owns the operation or query, from
that operation's complete facts. There is nothing to register and nowhere to
register it. Two crates that parse the same numeric kind are therefore not in a
routing collision at all: the authority boundary is the whole operation, not
global ownership of a number.

### #23: detection does not create an effect

**Excludes:** parsing a note's content and thereby opening observations the app
never asked for.

`nmp-content` returns an immutable document and depends on no engine or
mechanism crate. Decoding a NIP-19/NIP-21 locator preserves its exact identity
and authored relay hints but cannot construct a demand: kind, read routing,
relay admission, and observation count are choices that belong to the component
the app selected, and each selected component owns one independently cancellable
ordinary handle. See [Content and components](34-content.md).

### #24: protocol capability is physically separable from core

**Excludes:** a protocol verb on the generic engine facade, and a core artifact
shipping protocol symbols nobody selected.

Protocol-owned composition returns the ordinary core noun — NIP-22's comment
builder returns a `WriteIntent` — so generic facades keep only observation,
publication, and lifecycle verbs. The boundary is physical rather than stylistic:
a core-only build contains no protocol crate, API, or symbol, and each selected
capability arrives through one Cargo feature and one catalog record. See
[Packaging and distribution](08-packaging.md).

### #25: an authorization names the identity it speaks for

**Excludes:** an account switch between drafting and signing producing a
well-formed authorization that acts as the wrong identity.

A signed authorization carries the `PublicKey` the caller believed it would
speak for, and validation refuses any event whose signer is someone else.
Nothing else can catch this: the signature is genuinely valid for whoever signed,
so a signature check cannot see the mistake. That validated value is the only
door to a Blossom `Authorization` header.

## Builder rule

Treat this list as the North Star, not evidence that every mechanism ships.
Check the [status appendix](03-status-map.md) and [known
gaps](../known-gaps.md) before relying on one in a shipping app.

---

<!-- nav-footer -->
<sub>← [Reusable declarations and protocol operations](27-recipes-and-choosing.md) · [Index](README.md) · [What NMP does NOT do](29-not-do.md) →</sub>
