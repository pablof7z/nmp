# NMP

**An embeddable Nostr sync-and-routing engine — a library you add to an app, not a framework you build your app inside.**

> **Status — greenfield rebuild, past its core-engine gates; now building the routing/ownership and retraction families.** The engine core is proven end-to-end against the *live* Nostr network through milestones **M1–M4** — headless, through a Swift SDK, and via a dogfooding CLI — each independently verified by a separate Opus review. The iOS falsifier app (**M5**, the final thesis gate) is built and being dogfooded but **not yet judged**: its human "native library, or framework in disguise?" verdict is the one gate still open, and it has known blockers (see *Where we are*). We are now building two engine families in parallel — **relay-role routing/ownership** and **retraction/negative-deltas**. Wallet and DMs are explicitly deferred. Nothing is self-compat-binding before a v2.0 ships (not before Aug 2026). This README is the honest current picture, not a pitch — it changes as milestones prove or disprove things. **Disproved so far: nothing.**

## Why

Every correct Nostr client re-implements the same brutal machinery — outbox routing, subscription lifecycle, replaceable-event semantics, dedup and provenance, cache authority, relay fan-out discipline. Every incorrect one skips some of it and ships bugs users can't see until their timeline is silently stale.

The previous design (the `nostr-multi-platform` repo) attacked this by owning the *whole application* — actor, app-state, reducers, projections — then policing the wide seam that created with a 46-principle corpus, doctrine lints, and recurring audits. The apps built on it don't work well, and an app that touched Nostr for one feature had to buy an entire way of architecting itself. The lesson: **correctness must live in the shape of the API, not in a police force patrolling it.**

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

The old design had a hardcoded one-level special case of this (`dependent_interests.rs`) whose end-to-end path was never actually proven. Building the *general* primitive and proving it at two different depths is this project's central bet — and M1 (below) is where it held.

## Who owns what

| NMP owns (engine) | The app owns | The UI framework owns |
|---|---|---|
| Store: dedup+provenance, replaceable/delete/expiry on insert, persistence, bounded GC | Which queries exist and when | Rendering, layout, navigation |
| Binding resolution, incremental re-eval, re-root on identity change | Its own state model & architecture | View identity / recomposition |
| Routing: REQ coalescing, outbox, 2-relay-min, capped fan-out, reconnection | Who the active signer is (sets it; engine reacts) | Observation scope (the refcount edge) |
| Sync: negentropy-first, coverage watermarks | Folding query streams into view state | — |
| Write outbox: durable intents, signing orchestration, per-relay acks | All presentation (engine emits raw tokens only) | — |

## The plan (M0 → M6)

Each milestone has a pre-committed **kill condition**; only two are thesis-level. **M0** — founding gate (thinking): adversarial propose/refute on the grammar & two-noun surface. **M1** — grammar engine spike (headless): the binding resolver proving depth-1 and depth-2 surgical deltas and identity re-root via the *real* ingest→supersede→re-eval path (**thesis kill:** depth-2 needs per-shape code). **M2** — compiler/router + coalescing derisk (widen-only merge, mandatory local re-filter). **M3** — store + write outbox + transport, durable. **M4** — Swift SDK boundary. **M5** — the falsifier app (**thesis gate**, judged by a human on a device): does a normal iOS dev experience this as a library, not a framework? **M6** — second platform (Kotlin/Flow) + cold-start-offline authority + ledger falsification pass.

Full detail and every gate verdict: [`docs/VISION.md`](docs/VISION.md).

## Where we are

**Proven** (running code, each independently verified — detail in [`docs/reviews/`](docs/reviews/)):

- **M0 — founding gate PASSED** (conditional on amendments, all applied: `SetOp`, write durability classes, engine-internal decrypt capability).
- **M1 — the reactive filter-binding grammar is general.** One shared code path re-routes surgically at two depths (`$myFollows`; depth-2 NIP-29 groups; account-switch re-root with no cross-account leak; `follows − mutes` in-engine). Zero kind-branches; the real pipeline; a third shape needs zero engine change.
- **M2 — routing correct, coalescing off the correctness path.** 2-relay-min coverage solver + cap + widen-only coalescing with local re-filter (a wrong merge costs bandwidth, not correctness — property tests + differential oracle).
- **M3 — the engine runs, persists, stays correct offline.** A dedicated thread (blocking `recv`, no polling) drives the pure reducer against real relays behind a `Send+Clone` handle. Live-proven: subscribe→rows, durable publish→per-relay acks, reconnect→replay, NIP-77 negentropy, and the capstone — a **cold-start offline read that is authoritative** (`CompleteUpTo` vs `Unknown`).
- **M4 — a Swift SDK that reads like a native library.** `NMPQuery: AsyncSequence` (deinit→unsubscribe) over UniFFI; no framework adoption, no `Ffi*` leakage. `swift test` sees real notes flow from a live relay.
- **Self-bootstrapping outbox** (the MVP's core promise): given only two indexer relays, the engine discovers each follow's write relays from their kind:10002 reactively and routes content there — the app resolves *no* relays. Live-proven through `nmp-demo` (thousands of real notes from 2 indexers) and the Swift SDK's live tests.

**In flight** (the current build phase — GitHub epics [#22](../../issues/22) routing/ownership, [#23](../../issues/23) retraction):

- **Routing & Ownership (#22).** Lanes + directory read-relay accessor + app/fallback config landed (#24). Still ahead: solver-input narrowing (#29), per-kind claim-table routing, and the **kind-ownership static audit** (the load-bearing unit — every relay decision as compiler output, enforced by types + one cargo-metadata test, never a lint).
- **Retraction / negative-deltas (#23).** The resolver has only ever *grown or superseded*, never *retracted* (an unfollow lingers, a deleted note stays). Store door made symmetric (#25). Still building: kind:5 deletion + permanent tombstones, NIP-40 expiry index, resolver retract path, and the deadline-driven `recv_timeout` time loop.
- **M5 falsifier.** The app + its permanent diagnostic surface are built and dogfooded — dogfooding found and **fixed** a real O(n²) kind:10002 discovery-churn bug live. Open blockers before M5 can claim pass: Swift-side delta batching for unbounded historical replay ([#17](../../issues/17)), clean-clone Swift package usability ([#18](../../issues/18)), and unsigned-only publish across FFI. The human thesis verdict has **not** been rendered.

**Not started / deferred:**

- NIP-17 DMs (inbox routing [#19](../../issues/19), decrypt feedback path), wallet/NWC ([#6](../../issues/6)), drafts ([#13](../../issues/13)), authorless/global feeds ([#7](../../issues/7)), durable write intents surviving restart ([#3](../../issues/3)).
- **M6** — Kotlin/Flow second platform.

## How correctness is enforced — a bug-class ledger, not a lint

[`docs/bug-class-ledger.md`](docs/bug-class-ledger.md) lists the concrete Nostr bugs the design must make *structurally* impossible, each naming the type/API mechanism that excludes it. To claim an entry holds, an agent **attempts to write the bug** and records why it can't compile, reach the wire, or corrupt state — lints are not admissible. Of the 13 entries, 11 are **demonstrated at M1/M2/M3** (stale-replaceable, subscription leak, wrong-relay routing, fan-out cap, dedup/provenance, private-republish, cache-miss authority, NIP-77 assumption, enqueue≠converged, cross-account leak, app-owned expansion); #12 (encrypted-content presentation) and #13 (pagination cursors) remain design-level pending their milestones. The ledger's own status column is a second truth anchor.

## Canonical surfaces

[`docs/VISION.md`](docs/VISION.md) — the M0–M6 plan and gate verdicts. GitHub Issues — tactical state; epics **#22/#23** define the current build phase. [`docs/known-gaps.md`](docs/known-gaps.md) — the honest built-but-incomplete / deferred list. [`docs/bug-class-ledger.md`](docs/bug-class-ledger.md) — which bug classes are structurally closed. [`AGENTS.md`](AGENTS.md) — how work is run (issue-first). Point to them; this README doesn't duplicate them.
