---
title: Outbox — the built-in default write resolver
category: routing
slug: outbox
status: built
date: 2026-07-29
owns:
  - what "outbox" means for the WRITE side (author + p-tag fan-out + app relays)
  - the author-write-relays-only stub this replaced, kept as the defect record
  - the built-in resolver's full derivation, in pseudocode
  - when an outbox obligation SETTLES (retires) and what knowledge that needs
  - what the engine says when the derivation names nothing at all (§6.3)
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/routing/removed-routes.md
  - docs/internals/writes/event-builder.md
  - docs/internals/writes/identity.md
  - docs/internals/subscriptions/identity-grouping-and-limits.md
issues:
  - "BUILT (#975, #976): the full three-source fan-out replaced the author-write-relays-only stub"
  - "BUILT (#975): publishing before the first relay-list fetch parks instead of failing (§2.2)"
  - "BUILT (#976): the write side adopts fallback_relays() with the read path's suppression rule, armed by RECIPIENT coverage only (§6.2)"
---

# Outbox — the built-in default write resolver

Outbox is what `Auto` routing falls back to when no registered resolver claims
the event's kind (see `resolvers.md`). It is the answer to "publish this
ordinary event" — and the settled design makes it a much bigger answer than
the stub it replaced. This document records both: that stub, and the full model
the owner specified, in his words.

**Status is marked per section.** The design is now BUILT: #975 landed the
lifecycle and the three-source derivation, and #976 landed the recipient-scoped
`fallback_relays` trigger and the stated refusal. §2 is kept as the RECORD OF
THE DEFECT — it describes `b99f9d41`, the shipped stub these two arms replaced,
and it is retained rather than deleted because the bug-class ledger points at
it.

---

## 1. The full model — BUILT (#975, #976)

Pablo's outbox is not "the author's write relays." It is three additive
sources:

1. the author's own NIP-65 **write** relays,
2. the operator-configured **app relays**, always,
3. each **p-tagged recipient's** NIP-65 **read** relays (their inbox).

And it is an obligation that can *finish*. His worked example, verbatim:

> an outbox can end too; for example, if the user is p-tagging 3 users and only one of them has a 10002 and we know the other two don't have one, once we have the relays we'll publish to for the author's own relay + any app relay + some of the 1-p-tagged-user that did have a 10002 then the outbox item is consumed.

Note what that sentence quietly requires: knowing that two users **don't have**
a relay list is different from not having looked yet. That distinction —
`KnownAbsent` versus `Unknown` — is what makes retirement possible at all, and
it is settled by EOSE (§5, and `knowledge-and-settlement.md`).

This is also why outbox is the strongest argument for the whole `Auto` model
rather than a fixed-set special case: even "publish a note" has unknowns
(have we fetched each p-tagged user's kind:10002?), so the default resolver is
itself the incremental case. There is no simple tier below the interesting one.

---

## 2. What master did before this arm — the defect, kept on the record

### 2.1 Author write relays and NOTHING else

`resolve_routes` (`crates/nmp/src/core/write.rs:2585`) is the only resolution
site. Its `AuthorOutbox` arm (`crates/nmp/src/core/write.rs:2591-2604`)
resolves to

```rust
self.directory.write_relays(&author)   // write.rs:2595
```

mapped to bare URLs, and errors with `"no write relays known for author …"`
when that set is empty (`write.rs:2600`). No p-tag fan-out. No app relays. No
distinction between "author has no relay list" and "we never fetched it" —
the collapsed `Vec` is empty either way, and the arm cannot tell which.

### 2.2 The empty case is a terminal failure, not a park

At `on_signed`, a routing error removes the pending intent and emits
`WriteStatus::Failed` (`crates/nmp/src/core/write.rs:2229-2243`). Combined
with §2.1, **publishing before the first relay-list fetch dies permanently
today** — the exact cold-start case the designed lifecycle parks as
`AwaitingRoute` instead (`resolution-lifecycle.md`,
`preview-and-observability.md`). This is a verified defect the design fixes,
not a hypothetical.

---

## 3. The striking fact: the trait docs described the unbuilt policy first — BUILT (both halves, #976)

The vocabulary for the full model was designed into `RelayDirectory` and never
consumed by the write path. Read the shipped doc comments.

`read_relays` (`crates/nmp-router/src/facts.rs:105-117`), whose own doc names
a write policy that does not exist on master:

> An author's READ relays (NIP-65 kind:10002 read-marked + unmarked
> entries, lane `Nip65Read`) -- distinct from `write_relays`: an
> unmarked `r` tag is BOTH read and write, but a `"write"`-marked entry
> is excluded here (§2.4). This is what the p-tag inbox fan-out
> (`resolve_routes`'s `Default` write policy) consumes for a
> recipient, never `write_relays`.

There is no `Default` write policy anywhere in `resolve_routes` — the doc
describes the designed fan-out as if it were shipped. The only thing missing
is the code.

`app_relays` (`crates/nmp-router/src/facts.rs:81-91`):

> Operator-configured app relay set (`Lane::AppRelay`, §2.1 of
> `routing-and-ownership.md`) -- every kind, every author, always,
> additive, never counted toward the 2-relay-min.

"Every kind, every author, always, additive" is exactly clause 2 of §1 — the
read path honours it (`Lane::AppRelay`), and the write path ignores the
accessor entirely.

So the facts, the lanes, and the ingestion machinery all exist and are
exercised by reads. The designed outbox resolver consumes existing directory
surface; it invents no new fact source.

---

## 4. The resolver's derivation — BUILT (`EngineCore::resolve_outbox`)

The built-in resolver, in the `RouteResolver` vocabulary of `resolvers.md`
(three-valued knowledge from `knowledge-and-settlement.md`):

```text
resolve(subject, ctx):
    relays = {}
    needs  = {}

    # 1. the author's own outbox
    match ctx.write_relays(subject.author):
        Known(set)  -> relays += set
        KnownAbsent -> ()                      # settled: author declares none
        Unknown     -> needs += relay_list(subject.author)

    # 2. app relays: every kind, every author, always, additive
    relays += ctx.app_relays()

    # 3. each p-tagged recipient's INBOX (read relays, never write_relays)
    thin_recipient = false
    for p in subject.tags.p_tags():
        match ctx.read_relays(p):
            Known(set)  -> relays += set
                           thin_recipient |= set.len() < 2
            KnownAbsent -> thin_recipient = true  # settled: skip, do not wait
            Unknown     -> needs += relay_list(p) # no coverage to judge yet

    # 4. the operator's per-RECIPIENT top-up, under the read path's own
    #    suppression rule (§6.2)
    if thin_recipient and ctx.app_relays().is_empty():
        relays += ctx.fallback_relays()

    if relays.is_empty():                      # §6.3: nothing decided at all
        Refused { every exhausted source named }
    elif needs.is_empty(): Resolved(relays)    # obligation retires
    else:                  Partial { relays, needs }
```

Load-bearing details:

- **Recipients use `read_relays`, never `write_relays`** — the inbox half of
  NIP-65. The trait doc in §3 states this as the contract.
- **`KnownAbsent` is settled, not pending.** A recipient definitively without
  a kind:10002 contributes nothing and blocks nothing — that is how Pablo's
  3-p-tag example completes (§1).
- **`Unknown` declares a need; the resolver never fetches.** The engine folds
  the need into the existing discovery subscription (`resolvers.md` §4). Next
  drain, the answer is either `Known` or — after the indexers EOSE —
  `KnownAbsent`, and the obligation converges.
- **`Partial` still delivers.** The relays already known get lanes now
  (`resolution-lifecycle.md`); the `Auto` entry stays until zero unknowns
  remain. Retirement is settled *resolution*, not successful delivery:

> yeah, once we know "this event goes in relay 1, 2 and 3" it's been routed; it might have not been published and it sits on the publishing queue, but it's been routed. Whether you consider that "done" it depends on your position; it's done in terms of routing.

---

## 5. When the unknowns settle — BUILT (`EngineCore::settle_relay_list_eose`)

Absence must be knowable or no outbox obligation with a listless recipient
ever retires. Pablo's ruling on how it becomes knowable:

> and "do we have a 10002 for these three users" is very knowable: the moment we receive EOSE from the indexer relays we use we know, one way or another, whether we have a 10002 or not.

Mechanically: discovery derives absence from EOSE once and caches it as a
directory fact; resolvers only ever read the three-valued directory answer.
EOSE, `SourceStatus`, and acquisition evidence never enter the write plane.
Full treatment, including the session-scoped (never persisted) lifetime of
absence facts, is in `knowledge-and-settlement.md`.

The directory is already halfway there:
`RelayDirectory::knows_write_relays` (`crates/nmp-router/src/facts.rs:138`)
distinguishes "known, possibly zero" from "never resolved" — what it cannot
yet express is "queried and definitively absent", which is exactly the
transition EOSE supplies.

---

## 6. Consequences

### 6.1 A refused consumer request falls out for free — BUILT

@lima-codex asked for kind:0 profile copies to reach the configured
app/indexer relays, and was refused at the time as an out-of-scope
pinned-route request. Under `Auto`, it is not a feature at all:
`app_relays()` is additive for every kind (§3), so a kind:0 under `Auto`
reaches the app relays with no exception, no special case, and no route the
app ever names. The request is retired by the model, not by an
accommodation. Recorded so nobody re-adds a kind:0-shaped special path.

### 6.2 BUILT (#976) — the write side adopts `fallback_relays()`

The read path has a third operator set: `fallback_relays`
(`crates/nmp-router/src/facts.rs:93-103`), applied per-author only when the
author's own-relay coverage falls under the 2-relay-min **and** no app relay
is configured — its doc states "`app_relays` suppresses fallback entirely".

**Pablo ruled yes: the write-side outbox resolver adopts the same set with the
same suppression rule.**

The failure mode this closes is concrete. You reply to someone whose kind 10002
lists exactly one relay. Without fallback the reply goes to that single relay
and nowhere else, so if it is down the person you are replying to never sees
it. Reads already faced this exact question for the same author and answered
it; a write that cannot reach its addressee is the worse half of the problem,
not the lesser one.

The counter-argument considered and rejected: the shipped `resolve_routes` doc
says "a write fans out to every known write relay, it does not need
coverage-solving" (`write.rs:2570-2578`), which reads as though the
2-relay-min trigger has no write-side analogue. It does have one — it is just
about the RECIPIENT's coverage rather than the author's fan-out. Fanning out to
every known write relay and topping up a recipient below coverage are
independent, and adopting the second does not weaken the first.

Two clauses of that sentence are the whole of the built trigger, and each was
briefly implemented the other way:

- **The author's own thinness never arms it.** One write relay of my own is a
  fact about where I publish, not a deficit to repair; widening on it would put
  every note from a single-relay setup onto the operator's fallbacks, which is
  a different policy nobody ruled on
  (`outbox-fallback-coverage.feature`, "The author's own thin fan-out is not a
  coverage problem").
- **Only a SETTLED recipient answer can be short.** Until a recipient's list is
  looked up to completion nobody knows what their coverage is, so an `Unknown`
  arms nothing — the resolution stays open and the top-up is decided again when
  the answer lands. Topping up on ignorance would widen every route on its
  first pass and then never narrow it, because a relay already committed to an
  intent's route log is never withdrawn.

### 6.3 BUILT (#976) — an answer of NOTHING is a stated refusal

A resolution that names no destination at all still parks — a retired route is
never re-executed, so calling zero destinations "complete" would strand the
write permanently — but its reason names every exhausted source rather than a
bare "stuck". Each clause doubles as a way to fix it, because configuring any
single one of them would have produced a route:

```text
no destination could be determined: no relay list exists for <hex>;
no app relays are configured; no fallback relays are configured
```

The load-bearing distinction inside it is between a relay list discovery
settled as ABSENT ("no relay list exists for …") and one nobody has finished
looking up ("no relay list known yet for …"). Both look like an empty answer;
only the second is worth waiting for, and an operator told to wait for a fact
that will never arrive has been told nothing.

The same fact is also engine-global, as `DiagnosticsSnapshot.stalled_writes`
(`nmp::StalledWrite { receipt, reason, stalled_since }`). Per-receipt reporting
answers "what happened to THIS note" and needs somebody to be holding that
receipt; a misconfigured operator relay set is exactly the case where EVERY
write lands here and no single receipt says so. `stalled_since` is when the
CURRENT stall began — a write that parked, routed, and parked again is stuck
since the second park — and it is never journalled: how long a stall has lasted
is only knowable from facts the running engine holds, so a recovered write is
stalled as of the recovering process's clock.

The engine substitutes nothing. It ships no relay list of its own, and a
well-known public relay picked as a kindness would publish someone's event to a
host nobody in the chain consented to — and make the misconfiguration invisible,
because the write would appear to succeed
(`outbox-no-route.feature`, "No public relay is ever substituted for an empty
answer", "The indexers are not a publishing destination of last resort").
