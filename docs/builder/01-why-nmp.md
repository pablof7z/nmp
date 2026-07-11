# Why NMP exists

Nostr's wire protocol is small. A dependable local view is not.

An app that opens relay subscriptions directly soon owns routing discovery,
deduplication, replaceable and addressable winners, deletes, expiry, reconnect,
coverage evidence, reactive dependency repair, durable publication, retry, and
per-relay diagnostics. Those mechanisms are mostly independent of the app's
screens or content model, yet each app can get them differently wrong.

NMP extracts that machinery into an embeddable engine.

## The boundary

NMP owns:

- the canonical event and write-obligation store;
- demand compilation, relay planning, and subscription coalescing;
- provenance, replacement, deletion, expiry, and acquisition evidence;
- signing orchestration, durable delivery attempts, and receipts; and
- diagnostics that explain the resulting work.

The app owns:

- its state architecture, navigation, and presentation;
- which queries and writes its product needs;
- account and identity UX;
- ordering, formatting, moderation, and other product policy; and
- how scoped source evidence is presented to a person.

The UI framework owns rendering and observation scope.

This division is the thesis. NMP is a library an app talks to, not a framework
an app lives inside. It has no application container, reducer model, provider
hierarchy, navigation system, or scene-phase protocol to adopt.

## Two app-facing nouns

The target surface is intentionally small:

1. A **live query** is a closed demand value observed through the platform's
   native reactive primitive.
2. A **write intent** is a durable or explicitly non-durable publication
   obligation observed through a reattachable receipt.

Diagnostics is a read-only proof surface over those nouns. It is not a third
command API.

A live query describes more than a local filter:

```text
Demand = Selection + SourceAuthority + AccessContext
```

`Selection` says which events match. `SourceAuthority` says which relay facts
may acquire them. `AccessContext` carries protocol context such as AUTH that can
change what a source returns. Keeping those parts together prevents equal
filters under incompatible authority or access contexts from borrowing each
other's evidence.

## Reactive demand, not app-managed subscription repair

Suppose an app wants a caller-chosen event kind from authors projected out of a
NIP-02 contact list. A conventional client often watches the contact list,
stores its expanded pubkey set in app state, diffs that set, and repairs a
second group of relay subscriptions whenever the list or account changes.

NMP represents the dependency as data instead:

```text
kinds: [9999]
authors: Derived(
  inner: Filter(kinds: [3], authors: Reactive(CurrentPubkey)),
  project: Tag(p)
)
```

The app declares the graph and observes its snapshot. The engine owns the
expanded set, route repair, reference counts, and exact REQ changes. Changing
the current pubkey reroots only graphs that reference that input; literal
multi-account queries remain unchanged.

NIP-02 meaning in this example belongs in an opt-in protocol module or an
app-authored reusable declaration. The outer kind remains the caller's choice.
Core does not define a preferred feed or content kind.

## Correctness is structural

The earlier NMP design exposed a wide application framework and then relied on
doctrine, lints, and audits to police the boundary. The rewrite takes the
opposite approach: a bug class is closed only when the supported facade makes
the bad path unreachable and a falsifier proves that claim.

Examples of that standard include:

- no public verb opens a raw REQ;
- apps never receive a derived expansion to maintain themselves;
- raw app-computed relay arrays cannot bypass typed route authority;
- `Accepted` cannot be emitted before its promised persistence boundary;
- signer results cannot mutate a frozen event body; and
- an empty local result is never represented as global Nostr completeness.

The [bug-class ledger](../bug-class-ledger.md) records which guarantees are
built, partial, or still target work. Examples in this guide describe the
coherent v2 North Star; [Current implementation status](03-status-map.md) is the
shipping truth.

## The practical test

An existing Swift, Kotlin, or Rust app should be able to hold one engine as an
ordinary dependency, declare demand, publish intents, and fold returned values
into its own state. If correct use requires an NMP-shaped application, manual
subscription lifecycle, app-owned relay expansion, or a second optimistic
cache, the boundary has failed.

Read [The mental model](02-mental-model.md) next, then walk through
[A ten-minute embedding](04-ten-minute-timeline.md).

---

<!-- nav-footer -->
<sub>[Index](README.md) · [The mental model](02-mental-model.md) →</sub>
