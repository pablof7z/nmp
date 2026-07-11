# NMP

**An embeddable Nostr sync-and-routing engine — a library you add to an app, not a framework you build your app inside.**

> Status: **day 0, greenfield rebuild.** Nothing here is proven yet. This README is the honest current picture, not a pitch — it changes as milestones prove or disprove things. Everything is provisional until a v2.0 ships (not before Aug 2026); nothing is self-compat-binding before then.

## Why

Every correct Nostr client re-implements the same brutal machinery — outbox routing, subscription lifecycle, replaceable-event semantics, dedup and provenance, cache authority, relay fan-out discipline. Every incorrect one skips some of it and ships bugs users can't see until their timeline is silently stale.

The previous design (in the `nostr-multi-platform` repo) attacked this by owning the *whole application* — actor, app-state, reducers, projections — then policing the wide seam that created with a 46-principle corpus, doctrine lints, and recurring audits. The apps built on it don't work well, and an app that touches Nostr for one feature had to buy an entire way of architecting itself. The lesson: **correctness must live in the shape of the API, not in a police force patrolling it.**

## What it is

The app-facing surface is **two nouns**:

1. **A live query** — a Nostr `Filter` whose field values are `Binding`s (see grammar below). It arrives as the platform's native reactive primitive (Swift `AsyncSequence`/`@Observable`, Kotlin `Flow`). You fold it into your own state however you like.
2. **A write intent** — a durable, acknowledged operation. It returns a receipt whose status streams from pending → signed → routed → per-relay acked. Enqueued is never confused with converged.

Everything else — outbox routing that can't be turned off, per-relay REQ coalescing, 2-relay-minimum coverage with capped fan-out, negentropy-first sync, coverage watermarks — is engine interior, visible only through a permanent **diagnostic surface** (per relay / per kind: what was asked, what arrived, what coverage is proven). The diagnostics are the acceptance test rendered on screen.

There is no NMP app architecture to learn, because there is no NMP app architecture.

## The crown jewel — the reactive filter-binding grammar

Every filter field value is a `Binding`:

```
Binding  := Literal(set) | Reactive(ActivePubkey) | Derived(inner: Filter, project: Selector)
          | SetOp(Union|Intersect|Diff, [Binding])
Selector := Authors | Ids | Tag(char) | AddressCoord    // CLOSED, introspectable — never a closure
```

- **"My follows' notes, forever correct"** is one declaration:
  `kinds:[1], authors := Derived(kinds:[3], authors:[$currentPubkey] → Tag(p))`.
  When the follow list changes, the engine surgically re-routes the wire subscriptions. When the active signer changes, the whole graph re-roots. **Zero app code.**
- Nesting is bounded (≤3 deep). `Selector` is closed so the engine can hash, dedup, coalesce, and route demand — an opaque closure would make demand un-routable.
- At every node: **replace-not-rebuild** (unchanged members = zero wire churn) and **recompile-not-reopen** (the handle stays open across re-routes; no teardown/reopen race).

The old design had a hardcoded one-level special case of this (`dependent_interests.rs`) whose end-to-end path was never actually proven. Building the *general* primitive and proving it at two different depths is this project's central bet.

## Who owns what

| NMP owns (engine) | The app owns | The UI framework owns |
|---|---|---|
| Store: dedup+provenance, replaceable/delete/expiry on insert, persistence, bounded GC | Which queries exist and when | Rendering, layout, navigation |
| Binding resolution, incremental re-eval, re-root on identity change | Its own state model & architecture | View identity / recomposition |
| Routing: REQ coalescing, outbox, 2-relay-min, capped fan-out, reconnection | Who the active signer is (sets it; engine reacts) | Observation scope (the refcount edge) |
| Sync: negentropy-first, coverage watermarks | Folding query streams into view state | — |
| Write outbox: durable intents, signing orchestration, per-relay acks | All presentation (engine emits raw tokens only) | — |

## The plan (M0 → M6)

Each milestone has a pre-committed **kill condition** — the evidence that would make us abandon that piece rather than patch it. Only two are thesis-level.

- **M0 — Founding gate** (thinking only): adversarial propose/refute on the grammar & the two-noun surface. *Kill:* a real read shape the closed `Selector` vocabulary can't express, or a forced third app-facing noun.
- **M1 — Grammar engine spike** (headless): the binding resolver over an in-memory store + fake-relay harness. Proves depth-1 (`$myFollows`) and depth-2 (NIP-29 groups) surgical deltas and identity re-root, via the *real* ingest→supersede→re-eval path. **Thesis kill:** depth-2 needs per-shape special-casing → the grammar isn't general.
- **M2 — Compiler/router + coalescing derisk:** per-relay compilation, 2-relay-min coverage solver, widen-only coalescing with mandatory local re-filter (so a broken lattice costs bandwidth, never correctness). *Kill:* dedup-only floor can't fit realistic demand within relay limits.
- **M3 — Store + write outbox, durable:** persistence, real transport, negentropy probing, ack receipts. Harvest of old transport/store/negentropy through the import gate (re-justified, never verbatim). Execution risk, not bet risk.
- **M4 — Swift SDK boundary:** detachable `AsyncSequence` handles + `@Observable` adapter; async write + receipt stream. *Kill:* native ergonomics force app-lifecycle machinery into the SDK.
- **M5 — The falsifier app** (thesis gate): a small idiomatic SwiftUI app using NMP as a library. **Thesis kill:** it still needs NMP-shaped scaffolding, or a normal iOS dev couldn't have written it from SwiftData/Query knowledge. Judged by a human, on a device.
- **M6 — Second platform (Kotlin/Flow)** + cold-start-offline cache authority + ledger falsification pass. *Kill:* Kotlin needs the core surface reshaped.

Full detail: [`docs/VISION.md`](docs/VISION.md). Design record & non-negotiables: [`docs/design-record.md`](docs/design-record.md).

## How correctness is enforced — a bug-class ledger, not a lint

[`docs/bug-class-ledger.md`](docs/bug-class-ledger.md) lists the concrete Nostr bugs the design must make *structurally* impossible, each naming the type/API mechanism that excludes it. To claim an entry holds, an agent **attempts to write the bug** and records why it can't compile, can't reach the wire, or can't corrupt state. Lints are not admissible mechanisms. When a new bug class is found, we change the surface, then add a ledger entry — never add a lint.

## Status

| Milestone | State |
|---|---|
| M0 — Founding gate | **PASSED** (conditional; amendments applied) |
| M1 — Grammar engine spike | **PROVED** — 12/12 contract tests green; independently verified honest |
| M2 — Compiler/router + coalescing | **PROVED** — 91 tests green; kill did not fire; independently verified honest |
| M3 — Store + transport + write outbox | **PROVED** — 170 tests green; runs end-to-end vs a real relay; offline authority verified honest |
| M4 — Swift SDK boundary | **PROVED** — Swift `AsyncSequence` SDK; `swift test` sees real notes live; reads like a native library |
| M5 — iOS falsifier app | next (after reactive-outbox engine work) |
| M6 — Android | not started |

**Proved so far (running code, each independently verified by a separate Opus review — detail in [`docs/reviews/`](docs/reviews/)):**
- **M1 — the reactive filter-binding grammar is general.** One shared code path re-routes surgically at two depths (`$myFollows`; depth-2 NIP-29 groups; account-switch re-root with no cross-account leak; `follows − mutes` in-engine). Verified: zero kind-branches, real pipeline (v1's C5 synthetic-stand-in mode retired), no silent rebuild, a third shape needing zero engine change.
- **M2 — routing correct; coalescing off the correctness path.** 2-relay-min coverage solver + cap + widen-only coalescing with local re-filter (a wrong merge rule costs bandwidth, not correctness — property tests + differential oracle). Kill did not fire. CI gates every push.
- **M3 — the engine runs, persists, stays correct offline.** A dedicated thread (blocking `recv`, no polling) drives the pure reducer against real relays behind a `Send+Clone` handle. Live-proven: subscribe→rows, durable publish→per-relay acks (two relays → *different* terminals), reconnect→replay, NIP-77 negentropy, and the capstone — a **cold-start offline read that is authoritative** (fresh engine, persisted store, no relay: cached rows as `CompleteUpTo`; never-synced → `Unknown`).
- **M4 — a Swift SDK that reads like a native library.** `NMPQuery: AsyncSequence` (deinit→unsubscribe) over UniFFI; no framework adoption, no `Ffi*` leakage. `swift test` sees real notes flow through the `AsyncSequence` from a live relay.

A **Rust demo CLI** (`nmp-demo`) runs the engine against the *live* Nostr network — discovered a real user's 193 follows + their NIP-65 relays and streamed ~1,200–2,600 real notes via outbox, no fixtures. Dogfooding it found and **fixed** a P0 the 170 tests missed (full-set row re-emit, O(n²) → incremental deltas, 1.0× confirmed live).

**Next, before M5:** the engine must self-bootstrap outbox from 2 indexers (discover follows' write relays from kind:10002, reactively) so an app never resolves relays itself. Tracked in [`docs/known-gaps.md`](docs/known-gaps.md).

**Not yet proved:** the falsifier app (M5 — the second and final thesis-gate: does a normal iOS dev experience this as a library, not a framework?); Android (M6). **Disproved so far:** nothing.

This section is the truth anchor. It always says exactly where we are, including what has failed. Earlier gate detail (M0's amendments) is in [`docs/VISION.md`](docs/VISION.md) §9.
