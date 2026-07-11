# The mental model in one diagram

**Status: BUILT** (conceptual — the shape described here is proven through M4; see [`README.md`](../../README.md).)

After this chapter you'll hold the whole engine in your head as one picture: **values in → engine → rows + coverage out → your code after.** You'll know the two nouns everything traces back to, and the modularity principle that keeps the core tiny and protocol meaning opt-in.

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
        │  identity (one input)      │        │  diagnostics          │
        └────────────────────────────┘        └───────────────────────┘
              "current signer is A… now B"       read-only projection:
                                                 per relay / per kind —
                                                 asked / arrived / proven
```

Read it left to right. You hand the engine **values** — a live query, a write intent, and one input (who the active signer is). The engine does the entire brutal-machinery job (see *[Why NMP exists](01-why-nmp.md)*) and hands back **rows plus a coverage state** for reads, and a **streaming receipt** for writes. Everything *after* delivery — folding rows into your view model, formatting a hex pubkey into a name, deciding layout — is your code, and you may use arbitrary code there.

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
          | Reactive(ActivePubkey)                        // "whoever the active signer is, right now"
          | Derived(inner: Filter, project: Selector)     // the projected output of ANOTHER filter
          | SetOp(Union | Intersect | Diff, [Binding])    // compose bindings
Selector := Authors | Ids | Tag(char) | AddressCoord      // CLOSED — how to project the inner filter's rows
```

You hand this value to `observe`, and rows stream back as your platform's native reactive primitive — a Swift `AsyncSequence`, a Kotlin `Flow`, a Rust `Handle`. When any live input changes (a follow list, the active account), the engine re-evaluates the binding and surgically re-routes the wire, keeping the handle open (**replace-not-rebuild, recompile-not-reopen**). You never see the expanded intermediate set; you see final rows. Full treatment in *[Live queries & the binding grammar](09-binding-grammar.md)*.

### Noun 2 — the write intent (the write noun)

A write intent is a durable, acknowledged operation: an unsigned template plus a **durability class** (`durable | ephemeral | at-most-once`) and a **routing class** (`authorOutbox | toInboxes | privateNarrow`). You hand it to the engine and get back a **receipt whose status streams** — from accepted, through signed and routed, to per-relay acked. Enqueued is never confused with converged; signing and publishing are orthogonal; the app never picks relays and the types don't let it confuse "sent" with "acknowledged." Full treatment in *[Writing: intents, receipts, and the durability guarantee lattice](14-writing.md)*.

### And one input, not a third noun

**Identity is an input, not a noun.** The whole identity contract is: `addAccount`, then `setActiveAccount(pubkey?)`. You state a fact — "the current signer is A… now B" — and the engine derives everything downstream: the account's relay lists, its outboxes, its follow expansion, which key signs. `Reactive(ActivePubkey)` in any query is simply "whatever you last set." Switching accounts re-roots the entire binding graph, tearing the old account's demand down *before* activating the new, so cross-account leakage has no path. There is no session model, no login flow, no account vector in the engine — that's all your UX. See *[Identity & multi-account](16-identity.md)*.

Three things that look like they might be nouns, and aren't: **capabilities** (signer, encrypt/decrypt, AUTH policy) are plug points you configure; **diagnostics** are a read-only projection; the **Collection observation mode** is a mode of the read noun, not a separate one. The recurring failure this guards against is a "resource" or "session" or "module" concept creeping in beside the two nouns — that's the old fragmentation returning. If something you're building starts to feel like a third noun, stop and re-read this section.

## The engine, in one breath

You can build apps without opening this box, but here's what's inside so you trust it. Four planes, one process:

- **The store (data plane).** A local-first replica of events, keyed by id and by replaceable address. One mutating door: insert runs id-dedup (merging provenance) first, then replaceable supersession. Reads serve from cache before network, always.
- **The resolver + router (demand plane).** The seat of the grammar. It expands your bindings into a small dependency graph, re-evaluates incrementally when the store changes, and compiles the resolved demand into per-relay wire plans — coalescing, coverage-solving, diffing into surgical CLOSE/REQ deltas.
- **The write outbox (intent plane).** Durable intents flowing through orthogonal stages (sign, route, send, ack), each streamed on the receipt.
- **The diagnostic plane.** A read-only projection of the other three: per relay and per kind, what was asked, what arrived, what coverage is proven.

You watch all of it through diagnostics — which is the acceptance test rendered on screen, permanently. Debugging NMP is *reading*, not printf. See *[Coverage: empty vs unknown](11-coverage.md)* for coverage specifically and *[Diagnostics & debugging](22-diagnostics.md)* for the surface. Read the engine internals to build *trust*; skip them to build *apps*.

## Coverage: empty and unknown are different types

One piece of the "out" side deserves its own callout because it changes how you write every read. Alongside rows, a query delivers a **coverage** state:

```
Coverage := Unknown | CompleteUpTo(watermark)
```

`Unknown` means "we can't yet prove anything about this window" — don't render an empty-state, render a spinner. `CompleteUpTo(watermark)` means "this window is provably complete up to this point" — an empty result here is an *authoritative* empty; render "no results." A cache miss is only authoritative when a watermark proves it; a non-empty result is never proof of completeness. Keeping these two apart is the difference between a trustworthy "nothing here" and a bug where a stale cache shows a user an empty screen. Every read example in this manual shows what it does with `Coverage`; the full treatment is *[Coverage: empty vs unknown](11-coverage.md)*.

## The modularity principle: a tiny core, opt-in protocol meaning

The two nouns govern the API *surface*. A second principle governs *code weight*, and you feel it in your binary size and your dependency graph.

> **The engine core is the two nouns plus the hard concerns — store, routing/outbox, sync/negentropy, coverage, identity, diagnostics, the capability seams. Everything protocol-specific and non-primitive is opt-in and modular. You carry only what you use.**

Reactions, reposts, follow packs, highlights, long-form, lists, comments — none of these are in the core. Each lives in its own opt-in module (a per-NIP crate, a feature flag, a registerable module — the exact mechanism is being finalized; treat it as **PLANNED-shape**). The principle is what matters and it's load-bearing: **a minimal app that never reacts links zero reaction code; adding follow-pack support must not tax every other app.**

The shape you'll see: enable a NIP module and its recipes and kinds appear — you can now say `.react(to:)` or `.reactions(to:)`; don't enable it and that vocabulary is simply absent, and so is its weight. This was the old design's one genuine win worth keeping: the reactions crate encoded what reactions *mean*, and apps that didn't care didn't pack it.

This reframes three things you'll meet later in the manual:

- **Recipes** (the "batteries" — `.profile(pubkey)`, `.reactions(to:)`, `.textNote(content:)`) don't live in core. Each ships in its NIP module; *enabling the module is how you get the recipe.* A recipe is only ever a pure, printable composition of the two nouns — a protocol *fact* (kinds, tags, NIP shapes), never a product *decision* (what a feed contains, how things rank or render). See *[The batteries](27-recipes-and-choosing.md)* and the boundary tests in the design guidelines.
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
