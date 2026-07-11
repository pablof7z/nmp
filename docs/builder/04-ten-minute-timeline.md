# Build a working timeline in 10 minutes

**Status: BUILT** — every line below runs against the real Swift SDK (`Packages/NMP`) and the real Falsifier app (`apps/Falsifier`). No pseudo-code.

After this chapter you'll have a running iOS app that shows a live `$myFollows` timeline against real relays, switches accounts, and — the part that matters — proves the rows arrived by *reading the diagnostics screen*, not by trusting a spinner.

We build the smallest possible version of the Falsifier app. Everything here is lifted verbatim from `apps/Falsifier/Sources/Falsifier/` — if a line looks too short to be real, that's the point.

---

## Minute 0–2 — a project that links NMP

NMP ships as a local SwiftPM package at `Packages/NMP`. The one build artifact it needs — `NMP.xcframework`, the compiled Rust core — is generated, never committed. Build it once:

```bash
# from the repo root
scripts/build-swift-xcframework.sh --sim-only
```

`--sim-only` skips the device slice (no signing identity needed), which is all a simulator run wants. This produces `Packages/NMP/NMP.xcframework` and the generated bindings. (Full details in *Packaging, build & distribution*.)

Now point an app at the package. The Falsifier uses xcodegen; its entire dependency declaration is four lines (`apps/Falsifier/project.yml`):

```yaml
packages:
  NMP:
    path: ../../Packages/NMP
targets:
  Falsifier:
    dependencies:
      - package: NMP
        product: NMP
```

`import NMP` is the whole adoption cost. There is no provider, no container, no NMP-owned app delegate.

## Minute 2–4 — construct the engine, exactly once

The engine is a plain `let` on your own model object. It is not a singleton NMP forces on you, not an `@EnvironmentObject`, not a base class you subclass. This is `AppModel.swift`, trimmed:

```swift
import NMP

@Observable
final class AppModel {
    let engine: NMPEngine

    // The ONLY relay fact you ever supply: two indexer relays. The engine
    // self-discovers every author's real write relays from these alone.
    // There is no `relays:` parameter anywhere else in this manual.
    static let indexerRelays = ["wss://purplepag.es", "wss://relay.primal.net"]

    var kinds: [UInt16] = [1]
    private(set) var activePubkey: String?

    init() throws {
        let caches = FileManager.default
            .urls(for: .cachesDirectory, in: .userDomainMask).first
        let storePath = caches?
            .appendingPathComponent("nmp-timeline.sqlite").path
        engine = try NMPEngine(
            config: NMPConfig(storePath: storePath,
                              indexerRelays: Self.indexerRelays)
        )
    }
}
```

That's the entire engine lifecycle. `storePath: nil` would give you an in-memory store instead; a path gives you a cache that survives relaunches (cold-start offline reads come for free — see *Offline & sync*). `deinit` shuts the engine down for you; you never have to.

Wire it into a plain SwiftUI `App` (`FalsifierApp.swift`):

```swift
@main
struct TimelineApp: App {
    @State private var model: AppModel?

    var body: some Scene {
        WindowGroup {
            if let model {
                ContentView(model: model)
            } else {
                ProgressView("Starting NMP engine…")
                    .task { model = try? AppModel() }
            }
        }
    }
}
```

## Minute 4–6 — say who you are

Identity is an *input*, not a session. You state a fact — "this account is active" — and the engine re-roots every reactive query onto it. Two methods do everything.

For a real feed you don't even need a secret key: browsing read-only is a first-class state. `setActiveAccount` accepts any pubkey, keyed or not.

```swift
extension AppModel {
    // Read-only: no key involved. Legal, and the fastest way to see data.
    func browse(pubkeyHex: String) {
        try? engine.setActiveAccount(pubkeyHex)
        activePubkey = pubkeyHex
    }

    // Keyed: the secret crosses the boundary exactly once, here, and lives
    // engine-side forever after. `addAccount` returns the hex pubkey.
    func logIn(secretKey: String) async {
        let pubkey = try? await engine.addAccount(secretKey: secretKey)
        if let pubkey {
            try? engine.setActiveAccount(pubkey)
            activePubkey = pubkey
        }
    }
}
```

For the tutorial, browse a well-known active account so there's live data immediately — fiatjaf's pubkey, straight from `AccountsView.swift`:

```swift
model.browse(pubkeyHex:
    "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d")
```

## Minute 6–8 — the timeline (one query, one loop)

Here are both nouns' read half in one screen. The query is a *value* you build; `observe` hands you an `AsyncSequence` of snapshots; you fold each snapshot into your own `@State`. NMP does zero rendering — `Row` carries raw tokens (hex pubkey, Unix timestamp, verbatim content), and formatting them is your app's job.

First, the query. `$myFollows` is "kind:1 notes authored by whoever my kind:3 contact list currently names" — a `Derived` binding, reactive on the active account. This lives in *your* code, not NMP's (`FeedFilters.swift`):

```swift
import NMP

enum FeedFilters {
    static func follows(kinds: [UInt16]) -> NMPFilter {
        NMPFilter(
            kinds: kinds,
            authors: .derived(
                inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                project: .tag("p")
            ),
            limit: 200
        )
    }
}
```

Read that binding aloud: *the authors are derived from the `p` tags of the kind:3 event authored by the active pubkey.* When you switch accounts, the whole feed re-roots with no second `observe` call. (The binding grammar gets a chapter of its own: *Live queries & the binding grammar*.)

Now the view (`FeedView.swift`, trimmed):

```swift
struct FeedView: View {
    let model: AppModel
    @State private var rows: [Row] = []
    @State private var coverage: Coverage = .unknown

    var body: some View {
        List {
            Section { Text("Coverage: \(coverageText)").font(.caption) }
            ForEach(rows) { row in
                VStack(alignment: .leading) {
                    Text(shortHex(row.pubkey)).font(.caption.monospaced())
                    Text(row.content)
                    Text(date(row.createdAt)).font(.caption2)
                }
            }
        }
        // Re-observe when `kinds` changes: editing the filter builds a NEW
        // value and .task(id:) tears down the old query, opens a fresh one.
        // There is no "edit a running query" verb — there's no need for one.
        .task(id: model.kinds) { await observe() }
    }

    private func observe() async {
        rows = []; coverage = .unknown
        guard let query = try? model.engine
            .observe(FeedFilters.follows(kinds: model.kinds)) else { return }
        for await batch in query {
            rows = batch.rows.sorted { $0.createdAt > $1.createdAt }
            coverage = batch.coverage
        }
    }

    private var coverageText: String {
        switch coverage {
        case .unknown: return "unknown"
        case .completeUpTo(let ts): return "complete up to \(date(ts))"
        }
    }
    private func shortHex(_ h: String) -> String {
        h.count > 16 ? "\(h.prefix(8))…\(h.suffix(8))" : h
    }
    private func date(_ s: UInt64) -> String {
        Date(timeIntervalSince1970: TimeInterval(s))
            .formatted(date: .abbreviated, time: .shortened)
    }
}
```

Run it. Within a few seconds, notes stream in. Note two things the types forced on you:

- **You sorted the rows.** NMP delivered a set of live rows; *ordering is render policy*, so it's yours. The bridge accumulates the engine's added/removed deltas into a snapshot for you, but it does not pick an order.
- **Coverage is its own value.** An empty `rows` with `coverage == .unknown` means "we can't prove anything yet" — not "there are nothing." That distinction is a type, not a convention (*Coverage: empty vs unknown*).

## Minute 8–10 — confirm it worked by reading, not trusting

A spinner that stopped spinning proves nothing. The honest question is: *what did the engine actually ask, of which relays, and what came back?* That's the diagnostics screen — a live, read-only projection of real engine state, never estimated. It is the acceptance test rendered on screen.

One call, same iteration idiom as a query (`DiagnosticsView.swift`, trimmed):

```swift
struct DiagnosticsView: View {
    let model: AppModel
    @State private var snapshot = DiagnosticsSnapshot()

    var body: some View {
        List {
            Section("Summary") {
                LabeledContent("Relays", value: "\(snapshot.relays.count)")
                LabeledContent("Uncovered authors",
                               value: "\(snapshot.uncoveredAuthorCount)")
            }
            ForEach(snapshot.relays) { relay in
                Section(relay.relay) {
                    LabeledContent("Wire subs", value: "\(relay.wireSubCount)")
                    LabeledContent("Authors served", value: "\(relay.authorsServed)")
                    ForEach(relay.eventsByKind, id: \.kind) { kc in
                        LabeledContent("events kind:\(kc.kind)", value: "\(kc.count)")
                    }
                    DisclosureGroup("Exact wire filters (\(relay.filters.count))") {
                        ForEach(relay.filters, id: \.self) { json in
                            Text(json).font(.caption2.monospaced())
                        }
                    }
                }
            }
        }
        .task {
            for await s in model.engine.observeDiagnostics() { snapshot = s }
        }
    }
}
```

**Read this screen to confirm the rows arrived.** You should see:

- A relay list you never typed — `purplepag.es`, `relay.primal.net`, *and* relays the engine discovered on its own by reading your follows' kind:10002 lists. You supplied two indexers; the engine navigated the rest.
- Under a relay, **`events kind:1`** with a non-zero count — the actual notes that landed, counted from real ingest.
- **`events kind:3`** and **`kind:10002`** counts — the contact list and relay lists the engine fetched to resolve your `Derived` binding. That's the machinery you'd otherwise hand-roll, made visible.
- The **exact wire filter JSON** sent to each relay. Expand it: you'll see the `authors` set the engine resolved from your follows. You never assembled that list; the compiler did.

If kind:1 counts are climbing and your feed is populated, it worked — and now you can *prove* it worked, which is the whole discipline. If the feed were empty, this same screen would tell you why: no relays planned (no active account), authors uncovered (contact list not yet fetched), or coverage still `unknown` (in flight). "Why is my feed empty?" is always answered by reading, never by `print`. That's *Diagnostics & debugging*.

You now have the entire shape of an NMP app: **construct once, state identity, observe a query value, render raw tokens yourself, and read the diagnostics to trust it.** Everything else in this manual is depth on those five moves.

---

<!-- nav-footer -->
<sub>← [What works today](03-status-map.md) · [Index](README.md) · [The two nouns](05-two-nouns.md) →</sub>
