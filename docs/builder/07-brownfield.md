# Adding NMP to an app you already own

**Status: BUILT** — the coexistence patterns below use the real Swift SDK and the real Rust `Handle`, and are exactly how the Falsifier app embeds the engine. Kotlin/TS notes are marked PLANNED-shape.

After this chapter you'll know exactly where the engine object lives in an app that already has its own state, networking, and UI — and you'll have proof, from the SDK's own shape, that NMP demands no wrapper, no rewrite, and no surrender of your architecture.

---

## The one adoption fact: NMP is a library, not a framework

The engine is a plain object you construct and hold. It is not a scene, not an app delegate, not an environment you inject, not a base class you inherit. Adopting it is `import NMP` plus one `let`. Everything else in your app stays exactly as it is.

This is a *design guarantee*, not a style preference — the SDK is shaped so that the framework-shaped alternatives are impossible to write. There is no `NMPProvider`, no `NMPApp`, no `@NMPEnvironment`, because those types don't exist. The Falsifier's kill condition is permanent: *if embedding the engine makes your app NMP-shaped, the SDK is wrong.* So you can add it to one screen of a large existing app and leave the other forty untouched.

## Where the engine object lives

The engine is `Sendable` and cheap to hold. Two placements are idiomatic; pick by your existing architecture, not by anything NMP prefers.

### On your own model object (the Falsifier's choice)

The Falsifier puts it on a plain `@Observable` class the app authored. Nothing about `AppModel` is an NMP concept — the engine is just a property:

```swift
@Observable
final class AppModel {           // YOUR class, your shape, your rules
    let engine: NMPEngine

    init() throws {
        engine = try NMPEngine(config: NMPConfig(
            storePath: cachePath,
            indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"]))
    }
}
```

If you already have an `AppState` / `Store` / view-model layer, the engine drops onto it the same way, beside your existing properties. It coexists with whatever else that object holds — REST clients, a Core Data stack, a Combine pipeline — because it shares nothing with them.

### As a dependency you inject (DI containers)

Nothing stops you registering `NMPEngine` in your existing DI container and resolving it where needed:

```swift
// Whatever DI you already use — NMP has no opinion.
container.register(NMPEngine.self) { _ in
    try! NMPEngine(config: .init(storePath: path, indexerRelays: indexers))
}.inObjectScope(.container)      // one instance for the app's lifetime
```

Construct it once (a single store path wants a single owner), then treat it as a normal long-lived service. There is no NMP-mandated lifecycle to reconcile with your container's — `deinit` calls `shutdown()` as a safety net, so even a container that forgets to tear it down doesn't leak the engine thread.

### The no-provider-wrapper proof

Here's the evidence the SDK forces this on you rather than merely allowing it. Look at the Falsifier's entire app entry point — a plain SwiftUI `App` with no NMP type anywhere in the scene graph:

```swift
@main
struct FalsifierApp: App {
    @State private var model: AppModel?
    var body: some Scene {
        WindowGroup {
            if let model { ContentView(model: model) }
            else { ProgressView().task { model = try? AppModel() } }
        }
    }
}
```

No `.environment(engine)`, no `NMPProvider { }`, no modifier NMP requires. A view reaches the engine because *you* passed your own `model` down, using whatever propagation your app already uses (`@Environment`, an initializer argument, a singleton — NMP doesn't see the difference). The read path is likewise just a `.task` on a view you already have:

```swift
.task(id: model.kinds) {
    guard let query = try? model.engine.observe(myFilter) else { return }
    for await batch in query { self.rows = batch.rows }   // into YOUR @State
}
```

Demand is torn down when that `.task` is cancelled and drops the iterator — which happens on your view's natural lifecycle, not on any schedule NMP imposes.

## Coexisting with your existing state and networking

NMP owns *its* store (the SQLite cache at `storePath`) and *its* sockets (routed from your two indexers). It does not touch your networking stack, your persistence, or your other sockets. Three coexistence facts follow:

- **Two stores, no conflict.** NMP's cache is its own file. Keep your existing database for your own domain data; let NMP's watermark-backed cache serve Nostr rows. They never contend.
- **Delivered rows are just data.** `Row` is a plain `Sendable`/`Hashable` value of raw tokens. Fold it into your existing view models, map it into your own domain types, merge it with data from your REST API — it's ordinary Swift after it crosses the boundary. NMP has no opinion about what you do with a row.
- **Your sockets stay yours.** NMP has no `relays:` parameter and no way to hand it a connection. It routes exclusively from the indexer config plus what it discovers. Your existing WebSocket/HTTP code is invisible to it and vice versa.

## Migrating alongside another Nostr library

If you're moving off NDK / Applesauce / a hand-rolled relay pool, you don't cut over in one commit. Run both, screen by screen:

1. **Add the engine beside your existing client.** Both can be live at once; they don't share state. Your old code keeps serving the screens you haven't migrated.
2. **Migrate one query at a time.** Replace a hand-managed subscription with an `observe(NMPFilter)`. The classic thing you *delete* here is the machinery you used to hand-maintain: watching kind:3 and re-issuing REQs when follows changed is now a single `Derived` binding the engine re-resolves for you. That whole loop goes away.
3. **Move writes to intents.** Swap your sign-then-send code for `publish(WriteIntent)` and consume the receipt stream. Signing moves engine-side (the key crosses `addAccount` once); your old signer code retires.
4. **Verify each migrated screen on the diagnostics surface** before deleting the old path. The diagnostics screen shows the exact filters and per-kind event counts for the NMP-served screen, so you can confirm parity with the old client's behavior by *reading*, not by hoping. When the counts look right, delete the legacy path for that screen.

Because the two libraries share nothing, a half-migrated app is a stable state you can ship, not a risky in-between. Identity is the one thing to keep coherent: once a screen is NMP-served, that account's key should live engine-side (via `addAccount`), and `setActiveAccount` is the single switch that re-roots every NMP query. Your old client's notion of "current user" and NMP's active account should agree — keep one source of truth in *your* code and push it into NMP with that one call.

## Rust and other platforms

Embedding the Rust `Handle` in an existing Rust app is the same story: `EngineThread::spawn(...)` returns a `Clone + Send` `Handle` you store wherever your app keeps services. It owns two interior threads and nothing of yours; `handle.shutdown()` + `engine.join()` is the clean teardown, and until then it's just a value you clone into whatever tasks need it.

> **PLANNED-shape (Kotlin/TS).** The same placement rules are the *intended* shape on Android (the engine as a field on your own repository/`ViewModel`, collected as a cold `Flow`, no DI framework required) and on web (a module-level engine instance, consumed as an async iterator). Neither SDK is built yet; when they ship, the no-wrapper guarantee is contractual on every platform, because it's a property of the two-noun surface, not of any one language binding.

The through-line: NMP is additive. You keep your architecture, your networking, and your other libraries; you gain two nouns and a diagnostics screen; and the SDK's shape means you can never accidentally invert that relationship and end up living inside NMP.

---

<!-- nav-footer -->
<sub>← [Your first app in 20 lines](06-first-app.md) · [Index](README.md) · [Packaging & distribution](08-packaging.md) →</sub>
