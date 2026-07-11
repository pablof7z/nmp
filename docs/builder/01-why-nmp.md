# Why NMP exists

**Status: BUILT** (conceptual — the framing here is anchored to running code; see [`README.md`](../../README.md) for the live status table.)

After this chapter you'll know what NMP is *for*, why it's a library and not a framework, and why the tools you might reach for instead — NDK, Applesauce, or rolling your own — leave you holding a specific, recurring class of bug that has nothing to attach to inside NMP.

## The problem: everyone re-implements the same brutal machinery

Nostr looks deceptively simple. Open a WebSocket, send a `REQ` with a filter, get events back. You could ship a timeline in an afternoon.

Then correctness arrives, and it never leaves. To keep a single timeline *right* — not just populated once, but continuously, correctly, offline and online — you have to implement:

- **Outbox routing.** Events for a given author live on *that author's* write relays, which you learn from their kind:10002. There is no central relay. Get this wrong and you silently miss half your follows' posts.
- **Subscription lifecycle.** Every open `REQ` is a live resource. Open too many and relays drop you; leak them and you burn battery and bandwidth; close them at the wrong moment and your UI goes stale.
- **Replaceable-event semantics.** kind:0 profiles, kind:3 follow lists, kind:10002 relay lists, and all of `30000`–`39999` are *replaceable*: a newer event supersedes an older one by `created_at`, with a lexicographic-id tiebreak. Store both and you'll show stale data half the time.
- **Dedup and provenance.** The same event arrives from five relays. You want it once — but you also want to remember *which* relays had it, because that's how you route replies and prove coverage.
- **Cache authority.** When your local store returns nothing, does that mean "there are no results" or "I haven't synced this yet"? These are different facts, and conflating them is how you show a user an empty screen that should be full.
- **Relay fan-out discipline.** A naive client unions every follow's relay list and connects to 200 relays. A correct one solves for a minimal covering set with a fan-out cap.

Every *correct* Nostr client re-implements all of this. Every *incorrect* one skips some of it and ships bugs users can't see until their timeline is quietly, invisibly wrong. The machinery is the same in every client, it is genuinely hard, and it is not the thing you set out to build.

**NMP is that machinery, extracted, made correct once, and handed to you as a library.** You keep the part that is your product — what to query, what to write, how it looks. The engine keeps the part that is Nostr's.

## Library, not framework

This is the load-bearing distinction, so it's worth being precise about what it means.

A **framework** owns your application. You build *inside* it: you adopt its state container, its lifecycle, its module registration, its way of structuring an app. The previous NMP design worked this way — it owned the whole application (an actor, an app-state, reducers, projections) and then policed the wide seam that created with a 46-principle rulebook, lint passes, and recurring audits. The verdict, in the words of its own retrospective: the apps built on it don't work well, and *an app that touches Nostr for one feature had to buy an entire way of architecting itself.* A podcast player that wanted one Nostr feed had to become an NMP-shaped app.

A **library** is something you add to an app you already own. You call it; it doesn't call you. Think TanStack Query, SwiftData, or Room — nobody builds "a TanStack Query app." You have a normal app, and one of the things in it happens to be a query engine.

NMP v2 is a library. The test it holds itself to is exact and falsifiable: *a normal iOS developer who knows SwiftData or TanStack Query patterns should be able to add NMP to an ordinary app in an afternoon — two calls for a small app, twenty for a full client — without learning an "NMP architecture," because there isn't one.* If any use of NMP requires NMP-shaped scaffolding in your app, that's the design failing, and it's a pre-committed kill condition judged by a human on a real app (the falsifier — see *[The mental model](02-mental-model.md)* and the milestone plan in [`docs/VISION.md`](../VISION.md)).

The whole app-facing surface is **two nouns**: a live query you observe, and a write intent you publish. Everything else the engine does — the entire list of brutal machinery above — is interior, and you never touch it directly. You watch it work through a read-only **diagnostic surface** that shows, per relay and per kind, exactly what was asked, what arrived, and what coverage has been proven. That's it. That's the surface.

## Correctness lives in the shape, not in a police force

Here's the deeper bet, and it's what makes NMP different from "a nicer Nostr SDK."

The old design had a wide surface and *policed* it — a rulebook you had to follow, lints that caught you when you didn't, audits when the lints missed. That approach rotted anyway (display-separation violated 27 times; a feed layer that baked one app's rendering policy into the framework). The lesson: **a wide surface plus discipline cannot hold. Correctness has to live in the shape of the API, so that the wrong program doesn't compile or can't reach the wire — not in a set of rules you're trusted to remember.**

NMP replaces the rulebook with a **bug-class ledger**: a concrete list of Nostr bugs the design makes *structurally impossible*, each naming the type or API mechanism that excludes it (see [`docs/bug-class-ledger.md`](../bug-class-ledger.md), and *[Patterns & anti-patterns](28-patterns.md)* for the builder's-eye retelling). To claim an entry holds, someone *attempts to write the bug* and records why it won't compile, can't reach the wire, or can't corrupt state. For you as a builder, this is the payoff: whole categories of Nostr bug are simply not expressible in the API you're handed.

The clearest example is the one that motivates half of this manual.

## The canonical bug that has nothing to attach to

Here is the single most common outbox-era Nostr bug, the one nearly every SDK reproduces:

> Your app wants "my follows' notes." So it subscribes to the current user's kind:3 (their follow list), reads the `p`-tags, and issues a `REQ` for `kinds:[1], authors:[...those pubkeys]`. Now the follow list is *app state*. When a new kind:3 arrives, your code has to notice it, re-read the tags, diff against what it had, and re-issue the right `REQ`s. Miss that, and a user who follows someone new never sees their posts. Botch the diff, and you tear down and reopen every subscription on every change. Handle account-switching, and you get to do it all again, correctly, with no leakage between accounts.

This bug lives in the *seam* between "the app owns the expanded follow set" and "the app re-issues subscriptions." Every SDK that hands you the follow list and lets you build the `REQ` yourself hands you this seam — and with it, this bug.

In NMP, that seam does not exist. You declare the whole thing as one value:

```
kinds:[1], authors := Derived(kinds:[3], authors:[$currentPubkey] → Tag(p))
```

Read it as: *notes (kind:1) whose authors are the `p`-tags projected out of the current user's follow list (kind:3).* You hand that one declaration to the engine and observe the result. When the follow list changes, the engine re-evaluates the binding and surgically re-routes the wire subscriptions — closing exactly the departed author, opening exactly the new one, zero churn on the unchanged ones. When the active signer changes, the entire graph re-roots. **You write zero code for any of it.**

The bug has nothing to attach to because the app never sees the expanded author set, never holds it as state, and never issues a `REQ`. The expansion happens *inside* the engine, over a closed, introspectable vocabulary (bug-class ledger #11). There is no seam to get wrong. This binding grammar is NMP's crown jewel, and *[Live queries & the binding grammar](09-binding-grammar.md)* is where you'll learn to wield it.

## Why not NDK / Applesauce / roll-your-own

None of these are bad. They're the honest alternatives, and here's where each leaves you:

- **NDK (and most JS/relay-pool SDKs).** These give you excellent primitives — relay pools, subscription helpers, signer abstractions, often outbox-model routing. What they *don't* give you is a place for the follows-expansion bug to not exist. They hand you the follow list and let you build subscriptions from it, because that's the honest shape of a relay-pool library. The correctness of "my follows, forever" is *your* code, living in *your* app state — which means the canonical bug above is yours to write and yours to get wrong. NDK helps you build the `REQ`; it can't hold the invariant that the `REQ` stays right, because the invariant lives on your side of its API.
- **Applesauce (and reactive-store Nostr SDKs).** Closer in spirit — reactive, store-centric. But the reactivity typically flows over *delivered events*, not over *demand*. You still assemble subscriptions, and the store reacts to what arrives; it doesn't re-route the wire when a follow list you depend on changes, because it has no closed grammar describing *why* you asked for those authors. Reactive delivery is not reactive demand. NMP's bindings are reactive on the demand side: the subscription itself is a function of live state, re-evaluated by the engine.
- **Roll-your-own.** Entirely reasonable for a toy, and a career for anything real. You'll implement the brutal-machinery list one hard-won bug at a time. The reason NMP exists is that this work is *identical in every client* and genuinely hard — outbox routing, coverage watermarks, replaceable supersession, dedup with provenance. Writing it yourself means writing it correctly, offline and online, across reconnections and account switches, forever. That's the machinery NMP extracts.

The distinction that ties all three together: **they route delivery; NMP routes demand.** In a roll-your-own or relay-pool world, your app decides which subscriptions exist and keeps them correct as state changes. In NMP, you declare *what you want* as a value, and keeping the subscriptions correct — as follow lists change, as accounts switch, as relays come and go — is the engine's job, structurally, with no seam handed back to you.

## What to read next

- *[The mental model in one diagram](02-mental-model.md)* — values in → engine → rows + coverage out → your code after; the two nouns and the modularity principle.
- *[What works today vs. what's coming](03-status-map.md)* — the BUILT/PLANNED map and a glossary of every Nostr and NMP term this manual uses.

Everything in NMP traces back to the frame in this chapter: **the engine owns the part of the problem that is Nostr's; you own the part that is your app's; and the boundary between them is drawn so the hard bugs can't cross it.**

---

<!-- nav-footer -->
<sub>[Index](README.md) · [The mental model](02-mental-model.md) →</sub>
