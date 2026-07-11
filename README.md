# NMP

**An embeddable Nostr sync-and-routing engine: a library an app uses, not a framework an app lives inside.**

> **Status:** M1-M4 are built and independently verified. The M5 SwiftUI falsifier and permanent diagnostics screen are built, but the human library-vs-framework verdict and final on-device performance recheck remain open. Routing/ownership (#22), retraction (#23), and the governed v2 product-contract work (#43) are active. Public shapes are intentionally provisional; behavioral invariants are stable, and public changes require evidence, cross-platform impact review, and human signoff.

## Why

Correct Nostr clients repeatedly rebuild the same machinery: relay discovery and routing, subscription lifecycle, replaceable/delete/expiry semantics, dedup and provenance, offline persistence, fan-out discipline, and durable publication evidence. Incorrect clients omit part of it and silently show stale or incomplete state.

The previous NMP tried to solve that by owning the whole application architecture and policing a wide seam. The rewrite keeps the valuable Nostr machinery and removes the app framework. Correctness belongs in a small library surface and falsifiable invariants, not in an AppState/reducer/provider system or a doctrine-lint corpus.

## The two workload nouns

1. **A live query** declares demand: a Nostr selection whose field values can be reactive `Binding`s, plus typed source authority and access context. Results arrive through the platform's native observation primitive. Swift `AsyncSequence` and the JVM Kotlin `Flow` projection are live-proven; full Android remains open.
2. **A write intent** declares an exact publication obligation and durability policy. Durable acceptance is distinct from signing, routing, relay handoff, and relay acknowledgement; its receipt reports observed facts rather than claiming global delivery or convergence.

The supporting control plane stays narrow: current-pubkey inputs, signer providers, operator configuration, and diagnostics. None is an application architecture.

## Reactive demand

The built selection grammar is closed and introspectable:

```text
Binding  := Literal(set)
          | Reactive(ActivePubkey)
          | Derived(inner: Filter, project: Selector)
          | SetOp(Union | Intersect | Diff, [Binding])
Selector := Authors | Ids | Tag(char) | AddressCoord
```

A reusable helper may construct a `Derived` graph, but the engine always receives the expanded value: no app closure enters acquisition or routing. An opt-in protocol module may expose a richer typed query when its result is a protocol resource rather than merely a value set. Whether arbitrary app-owned reactive inputs should extend the grammar remains an explicit decision (#48), not an accidental escape hatch.

Demand correctness is keyed by the complete descriptor: **selection + source authority + access context**. Identical selection subgraphs can share internally, while wire subscriptions and acquisition evidence share only when their relay and visibility contexts are equivalent.

## Honest query evidence

NMP persists rows and relay-scoped acquisition facts. It can say which planned relays answered, which exact filters were sent, what EOSE/watermark evidence was retained, and which sources were blocked or omitted. It cannot prove that no matching event exists on an unknown, offline, private, or user-operated relay.

The target snapshot contract is therefore rows plus small, scoped cache/acquisition evidence. Raw per-relay plan, connection, AUTH, error, retry, watermark, and limit facts remain available through permanent diagnostics. NMP does not publish an aggregate `syncHealth`, globally `synced`, `converged`, or authoritative-empty judgment. The current `Unknown | CompleteUpTo` API is built but scheduled for correction under #49.

## Durable writes and signers

The target durable-write contract is:

- `Accepted` follows a crash-atomic commit of the frozen unsigned body, expected pubkey, receipt state, and one canonical pending store row.
- The pending row participates in ordinary filters and store semantics. The write path never pushes query rows directly.
- Signing promotes that same stable-id row from pending to signed after exact response verification.
- Publishing uses the signer associated with the current pubkey by default; a write may override the identity for podcast, disposable, hardware, or other secondary keys. The chosen identity is pinned at acceptance.
- An unavailable signer leaves the intent durably `AwaitingSigner(pubkey)` until a matching provider attaches or the app cancels it.
- NMP persists the obligation, not raw secret material. Platform SDKs provide standard secure signer providers; apps own identity import/removal/backup policy and may attach custom providers.
- Durable relay attempts use persisted logical backoff and per-relay evidence. A relay outcome after signing changes the receipt, not the canonical row.

The current engine has in-process receipts and signing, but crash-safe acceptance, pending rows, reattachment, and persisted retry are still open work (#2, #3, #6, #47).

## Protocol modules, not content recipes

Core is content-agnostic and protocol-aware only where a Nostr standard requires engine behavior. Opt-in NIP/feature crates own the exact schemas, codecs, validation, state reconstruction, and semantic operations defined by that protocol. They do not gain ownership of unrelated event kinds that may participate in it.

Modules compose immutable unsigned drafts and typed context. For example, a NIP-29 group can add its `h` context and host-relay authority to a separately owned draft without claiming that draft's kind; core signs once after composition. Blossom upload, media-draft construction, and Nostr publication remain separate operations with separate failure evidence. No core package or acceptance story privileges kind:1 or one social-feed product shape (#45).

## Ownership

| NMP owns | The app owns | The UI framework owns |
|---|---|---|
| Shared event cache, validation, dedup/provenance, replaceable/delete/expiry, persistence | Which queries exist; app state and architecture | Rendering, layout, navigation |
| Binding resolution, demand compilation, source/access-context identity, REQ coalescing | Current-pubkey value and account UX | Observation scope |
| Relay discovery/routing, capped fan-out, sync, acquisition evidence | Operator configuration and identity policy | View identity/recomposition |
| Durable intents, signer orchestration, attempt journal, per-relay receipts | What/when to publish; optional identity override | Presentation of raw states |
| Permanent raw diagnostics and explicit destructive reset | Whether logout also removes identities/credentials | - |

One engine is one local trust/cache domain: rows acquired while using any account enter the same cache. Apps serving mutually untrusted sessions need the explicit destructive reset tracked by #53; ordinary account switching is not a cache wipe.

## Boundedness

Query and diagnostics observations are latest-complete-state streams: internal deltas accumulate correctly, while slow consumers may skip intermediate rendered snapshots. Durable receipt facts remain persisted and reattachable. Oversized demand is chunked where semantics remain exact; otherwise NMP keeps available cached rows visible and reports explicit local shortfall. It never silently takes the first N values and calls the result complete (#46, #20).

## Where we are

**Built and verified:**

- M1: one reactive grammar handles bounded nested `Derived`/`SetOp` graphs and surgical demand changes without kind-specific resolver branches.
- M2: routing and widen-only coalescing keep local re-filtering on the correctness path.
- M3: store, transport, receipt streaming, coverage persistence, reconnect replay, and probed negentropy run end to end. Restart-durable writes and the public evidence wording remain incomplete.
- M4: the Swift package exposes live queries, writes, receipts, and diagnostics without FFI types leaking into app code.
- M5 implementation: an ordinary SwiftUI falsifier app and permanent diagnostics screen exist. The owner verdict is not yet recorded.

**Active:**

- Routing/ownership epic #22.
- Retraction/negative-delta epic #23; its unified deadline driver landed in #42.
- Product-contract epic #43 and promotion issue #44.
- The minimal JVM Kotlin/Flow falsifier landed in #54; Android/AAR/Compose remains under #40 after the M5 verdict.

**Known incomplete or deferred:** see [`docs/known-gaps.md`](docs/known-gaps.md). GitHub Issues are the one tactical queue.

## Canonical surfaces

- [`docs/VISION.md`](docs/VISION.md): stable behavioral invariants and milestone gates.
- [`docs/design-record.md`](docs/design-record.md): exploration history and promoted decisions.
- [`docs/design/`](docs/design/): focused normative designs for demand/evidence, durable writes, modules/composition, routing, retraction, and bounded delivery.
- [`docs/bug-class-ledger.md`](docs/bug-class-ledger.md): structural exclusions and their actual proof status.
- [`docs/known-gaps.md`](docs/known-gaps.md): honest built-versus-missing truth.
- [GitHub epic #43](https://github.com/pablof7z/nmp/issues/43): cross-surface work organization.
- [`AGENTS.md`](AGENTS.md): issue-first working discipline.
