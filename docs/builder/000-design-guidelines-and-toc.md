# NMP public-interface guidelines and builder-manual TOC

- **Date:** 2026-07-11
- **Status:** governed provisional guidance. Behavioral invariants are agreed;
  public names and shapes remain changeable under the review gate below.
- **Canonical owners:** [`VISION.md`](../VISION.md) and the focused specs in
  [`docs/design/`](../design/). The README owns current milestone truth.

## Part A - public-interface guidelines

### A.0 The frame

NMP is an embeddable Nostr sync-and-routing engine, not an app framework. Its
workload surface is two nouns: a live query and a write intent. Current-pubkey
inputs, signer providers, operator configuration, diagnostics, and protocol
modules are a small control/extension plane around those nouns; none owns the
app's state, navigation, lifecycle, or account UX.

Everything the engine uses to acquire, route, key, admit, or retry work crosses
the boundary as a closed, typed, introspectable value. Arbitrary app code runs
after delivery.

### A.1 Product primitives

| Surface | Current truth | Target invariant | App ownership |
|---|---|---|---|
| **Live query** | A `Filter`/`Binding` graph is built. Source/access context is not yet in the public descriptor. | Demand identity is selection + source authority + access context. Results are rows plus scoped acquisition evidence. | Which queries exist; how rows/evidence fold into app state. |
| **Write intent** | Signing, routing, and in-process receipts are built. Restart durability and pending rows are not. | Durable `Accepted` atomically owns the frozen body, expected pubkey, receipt, and canonical pending row. | What and when to publish; optional identity override; presentation of evidence. |
| **Current pubkey** | `setActiveAccount` currently also moves one active signer. | A reactive/default input. It reroots dependent bindings; publish uses its signer by default but may override and pins identity at acceptance. | Account list, selection UX, login/logout policy. |
| **Signer providers** | Local Rust signer exists; NIP-46/platform providers are incomplete. | NMP resolves providers by identity. Core persists obligations, not raw secrets; platform SDKs ship standard secure providers. | Identity import/removal/backup/consent and custom provider choice. |
| **Diagnostics** | Filters, lanes, counts, and watermarks are built. | Permanent raw source/AUTH/error/retry/limit proof without aggregate health judgments. | Rendering and redaction policy choices exposed by the SDK. |
| **Protocol modules** | Ownership/routing foundations exist; module composition is not built. | Opt-in modules own only protocol-defined schemas and may contribute typed context to foreign drafts without claiming their kinds. | Which modules to enable and how typed results enter the product. |

One engine is one shared local cache/trust domain. Account switching is not a
privacy wipe. A mutually untrusted logout uses the explicit destructive reset
tracked by issue #53.

### A.2 Reactive declarations and protocol queries

The closed binding grammar remains the acquisition foundation:

```text
Binding  := Literal(set)
          | Reactive(CurrentPubkey)
          | Derived(inner: Filter, project: Selector)
          | SetOp(Union | Intersect | Diff, [Binding])
Selector := Authors | Ids | Tag(char) | AddressCoord
```

A reusable helper can construct and return this graph. The expansion must stay
printable and equivalent to writing the value directly. A richer protocol
query may return typed protocol resources, but it still lowers to ordinary
demand and cannot introduce a second subscription lifecycle, cache, or app
closure.

Arbitrary typed reactive inputs beyond current pubkey remain an explicit
decision (#48). Do not invent blessed global inputs for one app's state.

### A.3 Protocol modules, not content batteries

No content kind is the center of NMP. Core exposes no privileged text-note,
home-feed, or kind:1 recipe layer.

An opt-in module may own protocol-defined schemas, codecs, validation, state
reconstruction, semantic operations, reusable declarations, and typed routing
context. It may not:

- claim unrelated content kinds because they participate in its protocol;
- register app closures into demand/routing/admission;
- own an app container, reducer, navigation, or lifecycle;
- maintain a second store, relay pool, outbox, or signing path;
- mutate an already signed event.

Composition uses immutable unsigned drafts. Each module adds only its owned
contribution, then core freezes and signs once. A bound NIP-29 group may add its
`h` context and host authority to a foreign draft without becoming that draft's
schema owner.

### A.4 Query evidence and boundedness

EOSE and watermarks are facts about a concrete source/request, never proof of
global Nostr state. Do not teach `synced`, global `complete`, `syncHealth`, or
authoritative-empty interpretations.

Query and diagnostics observations are latest-complete-local-state streams:
intermediate frames may coalesce after all underlying deltas are accumulated.
Write facts are persisted and reattachable. Every graph, wire, relay, result,
queue, and transport limit either preserves semantics, rejects explicitly, or
emits shortfall evidence. Silent first-N truncation is forbidden.

### A.5 Cross-platform and Rust boundaries

- One invariant-preserving Rust facade is the supported product surface for
  direct Rust apps and `nmp-ffi`.
- Swift `AsyncSequence` and Kotlin `Flow` are native projections of the same
  values and cancellation/backpressure semantics.
- Mechanism crates remain testable internals, not a second supported app
  assembly contract.
- Platform SDK sugar may improve spelling, never add expressiveness absent from
  the canonical values.
- No provider/container, scene-phase hook, or mandatory app lifecycle exists.

### A.6 Governed provisional changes

Before v2, public breaking changes are allowed and expected. They are not
casual. A public-shape change requires:

1. Evidence showing what the current shape cannot express or proves wrongly.
2. Impact analysis across Rust, FFI, Swift, Kotlin, persistence, diagnostics,
   and protocol modules.
3. Explicit human signoff.
4. Synchronized projection and falsifier updates.
5. Removal of the superseded path rather than a compatibility alias.

Stable behavioral invariants are not permission to freeze today's names or
enum layout before the falsifiers have earned that promise.

## Part B - builder manual

### Mental model

1. [Why NMP exists](01-why-nmp.md)
2. [The mental model](02-mental-model.md)
3. [Current status map and glossary](03-status-map.md)
4. [A first app](04-ten-minute-timeline.md)
5. [The two nouns](05-two-nouns.md)

### Reading and evidence

6. [Live query grammar](09-binding-grammar.md)
7. [Consuming results](10-consuming-results.md)
8. [Query evidence](11-coverage.md)
9. [Collection mode](12-collection-mode.md)
10. [Delivery-side transforms](13-delivery-transforms.md)

### Writing and identity

11. [Durable writes and receipts](14-writing.md)
12. [Replaceable edits](15-editing-replaceable.md)
13. [Identity and multi-account](16-identity.md)
14. [Relays and source authority](17-relays.md)
15. [Offline and sync](19-offline-sync.md)
16. [Capabilities](20-capabilities.md)

### Operation and extension

17. [Provenance](21-provenance.md)
18. [Diagnostics](22-diagnostics.md)
19. [Threading and lifecycle](23-threading-lifecycle.md)
20. [Performance and boundedness](24-performance.md)
21. [Testing](25-testing.md)
22. [Troubleshooting](26-troubleshooting.md)
23. [Protocol modules and reusable declarations](27-recipes-and-choosing.md)
24. [Patterns and anti-patterns](28-patterns.md)
25. [What NMP does not do](29-not-do.md)
26. [Platform guides](30-platform-guides.md)
27. [Gallery](31-gallery.md)
28. [Extending NMP](32-extending.md)
29. [Versioning and stability](33-versioning.md)

Every chapter must distinguish current implementation from target contract.
Historical reviews and milestone plans preserve what was believed and proven at
their time; they are evidence records, not current public guidance.
