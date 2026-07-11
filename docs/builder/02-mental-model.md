# The mental model in one diagram

**Status: CURRENT + TARGET.** The two-operation engine is built. Query evidence,
durable acceptance, signer overrides, and protocol-module composition are the
target contract and are not all implemented yet.

After this chapter you'll hold the whole engine in your head as one picture:
**values in -> engine -> snapshots + receipts out -> your code after.**

## The one diagram

```
        YOU                        THE ENGINE                        YOU
   (declare values)          (owns Nostr's hard part)         (arbitrary code)
   ┌───────────────┐        ┌────────────────────────┐        ┌──────────────┐
   │  live query   │──────▶ │  store · resolver ·     │ ─────▶ │  fold rows   │
   │ (Filter with  │        │  router · sync          │  rows  │  into your   │
   │  Bindings)    │        │                         │   +    │  view state; │
   │               │        │  outbox · coverage      │coverage│  format;     │
   │  write intent │──────▶ │  watermarks · dedup     │ ─────▶ │  render      │
   └───────────────┘        └────────────────────────┘ receipt└──────────────┘
        │                            ▲        │                       ▲
        │  reactive inputs +        │        │  diagnostics          │
        └────────────────────────────┘        └───────────────────────┘
              current pubkey; signer override       read-only projection:
                                                 per relay / per kind —
                                                 asked / arrived / proven
```

Read it left to right. You hand the engine closed values: a live-query
descriptor, a write intent, reactive inputs such as the current pubkey, and
registered capabilities. It returns query snapshots with source-scoped
evidence and durable receipt facts. Everything after delivery is app code.

That sentence is the governing rule of the entire system:

> **Values in, code after.**

Everything the engine uses to *make a decision* — what demand exists, how to route it, how to order and key rows, what to admit — crosses the boundary as a **closed, introspectable value**, never as your code. Everything *downstream of delivery* is yours, and there you may run anything. Hold that line and the rest of the manual is corollaries.

### Why "values in" is not a stylistic preference

It's what makes the engine able to do its job. Because a live query is a plain, hashable, serializable *value* — not a closure — the engine can hash it, dedup it against other queries, coalesce overlapping ones into a single wire subscription, refcount it, and route it. Two screens that ask for the same thing share one graph node and one `REQ`. The moment any part of the *decision path* became an opaque closure — "call my function to decide which authors" — the engine could no longer hash, dedup, or route it, and the whole demand-routing story would collapse. This is why the vocabulary the engine decides over is *closed* and extend-don't-escape: a new need extends the vocabulary (a deliberate design event), it never admits a closure. (This is the Electric-SQL "Shape" lesson and the Replicache closure trap, learned once and encoded structurally.)

Your closures are still welcome — they just live *after* delivery. A web-of-trust score, a custom comparator, a mute heuristic applied to rows the engine already delivered: arbitrary app code, on your side of the boundary, never parameterizing what the engine fetches or how it routes. See *[Delivery-side transforms](13-delivery-transforms.md)*.

## The two nouns

The entire app-facing surface is two nouns. Not three. If a feature can't be framed as one of these two — or as configuration of the machinery that serves them — the feature (or the chapter describing it) is mis-designed.

### Noun 1 — the live query (the read noun)

A live query is a Nostr `Filter` whose field values are **`Binding`s** instead of plain literals. The binding vocabulary is:

```
Binding  := Literal(set)                                  // a fixed set of values
          | Reactive(ActivePubkey)                        // the current-pubkey input
          | Derived(inner: Filter, project: Selector)     // the projected output of ANOTHER filter
          | SetOp(Union | Intersect | Diff, [Binding])    // compose bindings
Selector := Authors | Ids | Tag(char) | AddressCoord      // CLOSED — how to project the inner filter's rows
```

You hand this selection, together with its source authority and access context,
to `observe`. When a dependency changes, the engine re-evaluates only dependent
graph nodes and surgically re-routes the wire. Literal concurrent-account
queries do not change merely because the current pubkey does.

### Noun 2 — the write intent (the write noun)

A write intent is a durable or explicitly non-durable operation over an
immutable draft. Durable acceptance persists the obligation and its pending
cache record atomically. The current-pubkey signer is the default; an explicit
identity override handles podcast, disposable, hardware, or remote signers.
The receipt reports durable per-relay facts and can be reattached after restart.

### Inputs and capabilities are not extra app architecture

`ActivePubkey` is a reactive input and the default signer selection, not a
global authority over all work. Changing it re-roots descriptors that depend on
it. Already-accepted writes retain their captured signer, and an explicit
signer override need not become current. The app still owns account UX and
which queries exist.

Three things that look like they might be nouns, and aren't: **capabilities** (signer, encrypt/decrypt, AUTH policy) are plug points you configure; **diagnostics** are a read-only projection; the **Collection observation mode** is a mode of the read noun, not a separate one. The recurring failure this guards against is a "resource" or "session" or "module" concept creeping in beside the two nouns — that's the old fragmentation returning. If something you're building starts to feel like a third noun, stop and re-read this section.

## The engine, in one breath

You can build apps without opening this box, but here's what's inside so you trust it. Four planes, one process:

- **The store (data plane).** A local-first replica of events, keyed by id and by replaceable address. One mutating door: insert runs id-dedup (merging provenance) first, then replaceable supersession. Reads serve from cache before network, always.
- **The resolver + router (demand plane).** The seat of the grammar. It expands your bindings into a small dependency graph, re-evaluates incrementally when the store changes, and compiles the resolved demand into per-relay wire plans — coalescing, coverage-solving, diffing into surgical CLOSE/REQ deltas.
- **The write outbox (intent plane).** Durable intents flowing through orthogonal stages (sign, route, send, ack), each streamed on the receipt.
- **The diagnostic plane.** A read-only projection of the other three: per relay and per kind, what was asked, what arrived, what coverage is proven.

You watch all of it through diagnostics — which is the acceptance test rendered on screen, permanently. Debugging NMP is *reading*, not printf. See *[Coverage: empty vs unknown](11-coverage.md)* for coverage specifically and *[Diagnostics & debugging](22-diagnostics.md)* for the surface. Read the engine internals to build *trust*; skip them to build *apps*.

## Evidence, not global completeness

A query cannot prove the complete global Nostr result. Its snapshot reports
cached rows plus evidence about the currently planned sources: cache state,
connection/AUTH status, EOSE/watermarks, failures, and local limits. The app
interprets those facts. Exact per-relay proof remains in diagnostics.

## The modularity principle: a tiny core, opt-in protocol meaning

The two nouns govern the API *surface*. A second principle governs *code weight*, and you feel it in your binary size and your dependency graph.

> **The engine core is the two nouns plus the hard concerns — store, routing/outbox, sync/negentropy, coverage, identity, diagnostics, the capability seams. Everything protocol-specific and non-primitive is opt-in and modular. You carry only what you use.**

Each protocol module owns only the schemas, validation, reconstruction,
semantic operations, and routing context defined by that protocol. No content
kind is core-adjacent or privileged. Modules may compose immutable drafts: a
NIP-29 group can add its `h` tag and host-relay context to a photo draft without
owning the photo kind.

This reframes three things you'll meet later in the manual:

- **Reusable declarations and semantic operations** live in opt-in protocol
  modules or app packages, never in a favored content catalog in core. See
  *[Protocol modules and reusable declarations](27-recipes-and-choosing.md)*.
- **Extending NMP** = adding a protocol module. That's the primary extension path — not forking the engine, not registering a closure that parameterizes engine behavior. See *[Extending NMP](32-extending.md)*.
- **Packaging** = composing exactly the modules you enable, which is what determines your binary size. See *[Packaging, build & distribution](08-packaging.md)*.

## The shape of every chapter from here

With this model in hand, every feature in the manual is presented one of three ways, and now you can predict which:

1. **A query you observe** (the read noun) — most of Part III.
2. **A write you intend** (the write noun) — Part IV.
3. **Configuration of the machinery that serves those two** — identity, relays, capabilities, diagnostics; Part V and VI.

If you ever find a chapter that fits none of these, it's a signal the feature is mis-designed — and that's exactly the tripwire the two-noun surface exists to trip. Next: *[What works today vs. what's coming](03-status-map.md)*, the BUILT/PLANNED map and the glossary that grounds every term used above.

---

<!-- nav-footer -->
<sub>← [Why NMP exists](01-why-nmp.md) · [Index](README.md) · [What works today](03-status-map.md) →</sub>
