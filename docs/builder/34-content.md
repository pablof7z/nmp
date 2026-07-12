# Mixed Nostr content and live references

NMP's optional content layer handles the reusable work between a raw event
string and native pixels:

```text
source text
  -> semantic document with source ranges
  -> normalized npub/nprofile/note/nevent/naddr targets
  -> bounded claims on ordinary NMP live demand
  -> latest profile or event resources plus scoped evidence
```

It does not choose typography, cards, navigation, image policy, or a renderer
for every event kind. Core remains unaware that content rendering exists.

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
context is semantic input to one content renderer; `Code`, `ListItem`, and
`Heading` are not a component catalog applications must adopt.

Malformed references remain visible as text and produce a diagnostic.
Secret-key entities are never emitted as renderable targets.

## Resolve only what is render-relevant

Create the optional client from the engine the app already owns:

```swift
let client = NMPContentClient(engine: nmp)
let session = client.session(content: event.content)
```

Retain a claim while an occurrence is visible:

```swift
let occurrence = session.snapshot.document.references[0]
let claim = session.claim(referenceID: occurrence.id)

// Read synchronously from the latest snapshot.
let state = session.snapshot.state(for: occurrence)

// Optional early withdrawal. Dropping the claim also releases it.
claim?.cancel()
```

Kotlin makes the coroutine lifetime explicit:

```kotlin
val session = NMPContentClient(nmp).session(content, viewModelScope)
val claim = session.claim(session.snapshot.value.document.references[0].id)

// StateFlow latest-state projection.
val state = session.snapshot.value

claim?.close()
session.close()
```

Repeated occurrences of one target share one session acquisition. The final
release cancels its `NMPQuery`/`Flow` collection after a small grace period.
Cycle, depth, active-target, and total-resolution decisions come from the same
shared Rust rules on both platforms.

## What NMP still owns

A profile target lowers to current kind:0 demand. An address lowers to exact
kind + author + `d`. An event id lowers to exact id. Relay and author values in
NIP-19 entities remain acquisition hints where the protocol defines them as
hints. Network-authored relay hints pass NMP's public-host safety predicate
before they can become pinned helper demand; private, loopback, link-local, and
onion hints are ignored while ordinary outbox/public acquisition remains
available.

One canonical query supplies renderable truth. Optional pinned-relay and
author-outbox helper queries can acquire into NMP's existing store, after which
the canonical query observes the store-selected row. The content session owns
no cache, socket, replacement winner, or deletion algorithm.

This is why a relay-less `naddr` works: NMP discovers the author's kind:10002
through configured indexers, routes the address query to the discovered outbox,
and updates the same live snapshot when the current address winner changes. A
live package test proves that path using only `wss://purplepag.es` and
`wss://relay.primal.net` as operator configuration.

## Read the states literally

- `idle`: no render-relevant claim.
- `loading`: acquisition is active and no canonical row is present.
- `refreshing(cached)`: a previously resolved resource stays renderable while a
  new observation starts.
- `resolved`: the canonical query currently has a row.
- `withdrawn(previous)`: the canonical row disappeared; NMP does not guess
  whether a deletion, expiration, or another scoped cause was responsible.
- `shortfall`: an explicit path or query shortfall, never global absence.
- `stopped`: acquisition ended without a canonical resource.
- `collapsed`: cycle or configured budget boundary.

Evidence remains separated into the canonical path and helper paths. A renderer
may show a retry or relay-detail affordance, but must not relabel scoped EOSE or
a failed hint as “not found on Nostr.”

## Typed protocol resources

`decodeNostrProfile(from:)` / `decodeNostrProfile(row)` project kind:0 fields.
`decodeNIP23Article(from:)` / `decodeNip23Article(row)` project NIP-23 title,
summary, image, publication time, identifier, and Markdown body. Display-name
fallback, reading-time estimation, truncation, and layout remain native UI
policy.

Unknown event kinds stay raw `Row` values. Adding a custom renderer never
requires editing a central Rust enum.
