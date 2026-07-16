# Mixed Nostr content and reference plans

NMP's optional content layer parses authored text into immutable semantics. It
does not decide that a reference should be resolved:

```text
source text
  -> NostrContentDocument
  -> authored NostrReferenceOccurrence
  -> application selects a component
     -> literal/link component: no observation
     -> resolving component: pure reference plan -> ordinary NMP observations
```

That decision order is load-bearing. An `npub` identifies a public key; it does
not command an application to retrieve kind:0. A `note`, `nevent`, or `naddr`
can be rendered as authored text, a link, a consent prompt, a cache-only
preview, or a resolved embed. Only the selected component knows which result it
is trying to produce.

`nmp-content` therefore owns no engine, query handle, kind:0 or NIP-23 codec,
renderer, acquisition state, or hydration budget. Core remains unaware that
content rendering exists.

## Parse without I/O

Swift:

```swift
import NMPContent

let document = parseNostrContent(
    event.content,
    syntax: event.kind == 30_023 ? .markdown : .plainText
)
```

Kotlin:

```kotlin
val document = parseNostrContent(content, NostrContentSyntax.PlainText)
```

The parser preserves original UTF-8 source ranges, original reference text,
separate occurrence identity, and normalized target identity. Markdown block
context is semantic input to a renderer; `Code`, `ListItem`, and `Heading` are
not a component catalog applications must adopt.

Malformed references remain visible as text and produce a diagnostic.
Secret-key entities are never emitted as actionable targets. Parsing either
case constructs no engine task and opens no observation.

## Planning is also pure

When a selected component wants to resolve an occurrence, it asks the Rust
grammar owner for a closed plan:

Swift:

```swift
let occurrence = document.references[0]
let plan = try referenceDemandPlan(for: occurrence.target)
// `plan` is data. No query has opened yet.
```

Kotlin:

```kotlin
val occurrence = document.references.first()
val plan = referenceDemandPlan(occurrence.target)
// The component may now collect plan.canonical and selected helpers.
```

`NostrReferenceDemandPlan` contains one canonical `NMPDemand`, zero or more
helper demands, a stable target key, and the count of unsafe or over-bound raw
relay hints that were discarded. It owns no engine or lifecycle.

The lowering rules are exact:

- `npub` / `nprofile` -> current kind:0 selection for the public key;
- `note` / `nevent` -> exact event-id selection;
- `naddr` -> exact kind + author + `d` coordinate selection.

An optional `nevent` author or kind remains a hint; it never becomes an extra
canonical match constraint. Relay hints are canonicalized, deduplicated,
safety-filtered, and bounded. Safe pinned-relay and author-outbox helpers may
improve acquisition, but the canonical observation remains the rendering
authority. Helpers feed NMP's one canonical store and keep their own scoped
evidence; they do not select a second winner.

The shared corpus in `fixtures/reference-plans.json` proves these rules through
the direct Rust, FFI, Swift, and Kotlin decoder/parser/planner surfaces.

## The component opens and owns observations

A component that chooses resolution opens ordinary NMP observations and keeps
their handles for exactly as long as its policy requires:

```swift
let canonical = try engine.observe(plan.canonical)
let helpers = try plan.helpers.map { try engine.observe($0) }
```

Equal demands still coalesce in NMP Core. Two components receive independent
handles, so either can release its own interest without changing the other's
contract; the engine may nevertheless use one compatible wire subscription.
Dropping the last handle withdraws live demand without deleting the durable
canonical event.

Visibility is one optional way to scope a chosen observation. It is not itself
a reason to create one. The SwiftUI helper and standard components are covered
in [SwiftUI content and components](35-swiftui-ui.md).

## What NMP still owns

Components never manipulate relay `REQ`/`CLOSE` ids or maintain their own event
cache, replacement winner, deletion interpretation, or retry loop. They declare
ordinary demands; NMP owns the store, directory, routing, wire lifecycle,
coalescing, and scoped acquisition evidence.

This is why a relay-less `naddr` remains meaningful. The canonical plan names
the exact kind + author + `d` coordinate. NMP can discover the author's
kind:10002 through configured indexers, route the demand to the resulting
outboxes, and update the same canonical observation when the current address
winner changes. The component does not need to copy outbox discovery or
replaceable-event arbitration.

Canonical and helper evidence must stay separate. One pinned hint reaching
EOSE, one outbox being unavailable, or one local relay refusal describes that
exact path; it is never relabeled as "not found on Nostr." A component may show
retry, consent, or relay-detail UI from those facts without manufacturing a
global completeness verdict.

## Freshness belongs to each handle

The independently merged freshness axis (#565 / PR #577) lets the component
state its acquisition policy on the ordinary demand it opens:

- `Live` is cache-then-live and preserves the previous default;
- `MaxAge(seconds)` suppresses wire work when all currently planned source
  scopes have sufficiently recent coverage, otherwise it behaves as `Live`;
- `CacheOnly` returns cached state and never contributes wire work.

This is per-handle policy, not parser configuration or a shared content
coordinator. A feed-avatar component can accept four-hour-old kind:0 coverage
while a profile screen opens a `Live` handle for the same selection. A
cache-prompt event loader can begin with `CacheOnly` and open `Live` only after
consent. Equal handles may share graph/cache state while retaining independent
freshness contracts.

`SourceAuthority::Pinned + CacheMode::Strict + CacheOnly` means cached rows
whose provenance intersects that pinned relay set, with no network. Recent
empty coverage also satisfies `MaxAge`: freshness proves the question was
checked, not that a row existed. Evidence remains scoped; none of these modes
means "globally absent."

## Cycle and depth are immutable presentation context

Nested rendering threads an ancestor path, current depth, and maximum depth
through `NostrContentRenderContext.descending(into:)`. Descending returns a new
value or refuses a cycle/depth violation. There is no active-reference count,
resolved-reference count, grace-window claim table, or mutable document-wide
coordinator under another name.

NMP Core still owns its independent finite resource ceilings. Those mechanism
bounds do not turn authored reference detection into UI acquisition policy.

## Typed protocol resources have exact owners

The content parser no longer decodes kind:0 or NIP-23. A profile module (#208)
owns exact kind:0 schema/validation and a NIP-23 module owns exact article
schema. Until those owners land, a component can render a canonical raw `Row`
honestly and use the permanent generic fallback.

This keeps the dependency direction clean:

```text
nmp-content       source -> semantic document
nmp-grammar       target -> safe closed demand plan
nmp-profile       kind:0 protocol value (tracked by #208)
future NIP-23     article protocol value (exact owner/name TBD)
native component  acquisition choice, handle lifetime, presentation
NMP Core          store, routing, coalescing, relay I/O, evidence
```

## Migration from the removed session API

This is a clean break; none of the deleted names has a compatibility alias.

| Removed API or behavior | Migration |
|---|---|
| `NMPContentClient` | Delete it. Call `parseNostrContent` directly; keep the app's existing `NMPEngine` only where a selected component needs observation. |
| `NostrContentSession` / Kotlin `ContentSession` | Keep the immutable `NostrContentDocument`. Move observation ownership into the selected component or loader. |
| `NostrContentClaim`, `claim(referenceID:)`, and the unconditional claim modifier | Delete them. A no-fetch component opens nothing; a resolving component owns ordinary query handles, optionally through `observeWhileVisible`. |
| Session resource snapshots/states/evidence merging | Read each component's canonical/helper `RowBatch` values and their exact scoped evidence. Do not construct a UI-global winner or absence verdict. |
| `HydrationPolicy`, active/resolved counts, grace windows | Delete them. Preserve only immutable cycle/depth context; rely on NMP's core handle coalescing and finite mechanism ceilings. |
| `decodeNostrProfile` / `decodeNIP23Article` from the content package | Move schema interpretation to the exact protocol owner. Use raw-row/generic presentation until that owner ships. |
| Scripted resource sessions | Use a pure document plus literal/custom components, or inject an observation factory owned by the preview/test. Do not recreate a fake shared acquisition owner. |

The discriminating review question is simple: **name a reasonable application
policy the current ownership shape makes impossible.** Literal/no-fetch,
consent-before-network, cache-only preview, and explicit-relay fallback must all
remain expressible at the selected component boundary. If no real policy is
excluded, discard the finding instead of inventing another coordinator.
