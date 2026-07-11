# Adding NMP to an app you already own

NMP is an ordinary long-lived dependency. Put it wherever the app already keeps
services, then fold query snapshots and receipt facts into the app's existing
state.

No NMP type needs to own the scene graph.

## Hold the engine in app-owned state

The exact spellings remain provisional, but the dependency shape is simple:

```swift
@Observable
final class LibraryModel {
    let nmp: NMPEngine
    var rows: [NMPRow] = []

    init(cacheURL: URL) throws {
        nmp = try NMPEngine(configuration: .persistent(cacheURL))
    }
}
```

`LibraryModel` is the app's type. It may also hold a REST client, its own
database, feature flags, or any other dependency. NMP neither knows nor cares.

An app that already uses dependency injection can register the engine there
instead. Construct one engine per local trust domain and persistent store; do
not build an NMP-specific provider hierarchy around it.

## Observe in the app's natural scope

A view model, reducer effect, SwiftUI task, Kotlin coroutine, or Rust task can
own an observation:

```swift
func observeLibrary() async throws {
    let demand = NMPDemand(selection: .filter(kinds: [9999]))

    for try await snapshot in nmp.observe(demand) {
        rows = snapshot.rows
        sourceEvidence = snapshot.acquisition
    }
}
```

Dropping the final observation owner withdraws its demand. NMP performs the
reference counting, dependency repair, REQ close, and reconnect work. The app
does not mirror subscription lifecycle or keep expanded author and relay sets
alive.

Use the platform's normal rules when updating UI state. For example, a Swift
consumer running off the main actor must hop to `MainActor` before changing
UI-bound properties; NMP does not invent another executor model.

## Keep app data and NMP data in their owning stores

NMP's persistent replica contains Nostr events, provenance, scoped acquisition
evidence, pending write obligations, attempts, and receipts. The app's database
continues to own product-specific records.

Rows crossing the facade are plain values. The app may map them into its own
view models, combine them with server data, or retain presentation state. It
must not create a second authoritative Nostr cache or an optimistic write
overlay that competes with the canonical NMP store.

One engine instance is one local trust domain. Switching the current pubkey does
not partition or wipe public cached events. An app serving mutually untrusted
local users must invoke the explicit destructive-reset operation between them.

## Migrate one ownership slice at a time

An existing Nostr client can run beside NMP during migration:

1. Add one engine without changing the app's architecture.
2. Move one read workflow to a live query.
3. Confirm its compiled demand and source evidence in diagnostics.
4. Delete the old subscription and local expansion logic for that workflow.
5. Move its writes to intents and consume the receipt facts.

Avoid leaving two live owners for the same workflow indefinitely. A temporary
side-by-side comparison is useful; two permanent caches, retry loops, or relay
planners create ambiguous authority.

Do not fill gaps in an unfinished NMP surface with app-owned subscription
repair, relay expansion, signer persistence, or durable retry. Keep the old
owner for that workflow until the supported facade can replace it end to end.

## Account and signer inputs stay separate

The app may keep its existing account model. Feed the current pubkey into NMP as
a reactive input for queries that reference it. Literal multi-account queries
remain live when that value changes.

For writes, the common path uses the signer registered for the current pubkey.
An explicit per-write identity override supports podcast, disposable, hardware,
or delegated identities without changing the reactive account input. The chosen
identity is pinned at acceptance and cannot drift after an account switch.

## What adoption must not require

A brownfield integration should not add:

- an `NMPApp`, `NMPProvider`, or NMP-owned state container;
- scene-phase callbacks that reopen subscriptions;
- app-generated relay lists for each filter;
- a timer that polls engine state;
- app-owned copies of derived binding expansions; or
- a second optimistic event list wired directly to the write path.

If one of those seems necessary, inspect the permanent diagnostics surface and
the [current implementation status](03-status-map.md). The right response may
be an upstream NMP gap rather than downstream application machinery.

---

<!-- nav-footer -->
<sub>[Index](README.md) · [Packaging](08-packaging.md) →</sub>
