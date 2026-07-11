# M5 — The iOS falsifier app (THESIS-GATE) — build plan

- **Date:** 2026-07-11
- **Status:** Minimum-shape build plan for Sonnet builders. Milestone M5 per `docs/VISION.md` §5/§6.
- **Gate:** Tier B — running falsifier on simulator/device **plus the owner's human judgment** ("native library, or framework in disguise?"). This plan makes that judgment *legible*; it does not make it.
- **Reading order for builders:** VISION §5 (the app + what each element proves), §6 M5 (the kill), §2/§3 (principles/ownership); this repo's `docs/known-gaps.md` (what's built).

---

## 0. What is already DONE (build on it — do not rebuild)

Proven end-to-end against real relays (`Packages/NMP/Tests/NMPTests/LiveRelayTests.swift`, `nmp-engine/tests/self_bootstrap_outbox.rs`):

- **`$myFollows` feed from ONLY 2 indexer relays.** The engine self-discovers each author's kind:10002 write relays live and re-routes content atoms to them; the app resolves **zero** relays. `known-gaps.md`'s RelayDirectory self-bootstrap item is **CLOSED**.
- **Multi-account** — `addAccount(secretKey:) -> pubkeyHex`, `setActiveAccount(pubkey:)` re-roots read-root + signer together.
- **Reactive query grammar** — `NMPFilter` + `NMPBinding` (`.literal/.reactive/.derived/.setOp`), delivered as an `AsyncSequence` (`NMPQuery`) with `@Observable` sugar (`NMPQuerySnapshot`), incremental row deltas accumulated to snapshots, deinit-tied teardown.
- **Swift package** — `Packages/NMP` (`NMP` target is the only import), xcframework built by `scripts/build-swift-xcframework.sh` (`--sim-only` = sim+macOS slices, enough for the simulator app).

**Only one real engine/FFI gap blocks M5: the diagnostic surface (§2).** Everything else the app needs is a pure SwiftUI build against the existing SDK.

---

## 1. The single non-negotiable engine/FFI gap: the diagnostic surface

VISION §5 makes the diagnostic screen the flagship ("the acceptance test rendered on screen, permanently"). Today it exists **internally only**:

- `nmp_router::Diagnostics` / `RelayDiagnostics` (`nmp-router/src/diag.rs`) already carry **per-relay wire-sub count, exact filters, `by_lane` counts, and reverse-coverage (`authors_served`)**. Held on `Router::last_diag`, read via `Router::diagnostics()`.
- **NOT exposed:** `EngineCore` has no diagnostics accessor; the runtime `Handle`/FFI/Swift surface never sees it.
- **NOT built at all:** *events received per relay per kind* — there is no counter anywhere. The `RelayMessage::Event` arm in `nmp-engine/src/core/mod.rs` ingests into the resolver without counting.
- **Coverage per (filter, relay)** exists via `EngineCore::get_coverage()` + `QueryCoverage` but is not aggregated into a diagnostic view.

### 1.1 Chosen shape — a **reactive diagnostics stream** (push, not poll)

Mirror the existing `RowObserver`/`NMPQuery` bridge exactly. No pull-accessor + timer (that violates the no-polling rule and reads un-idiomatic). The diagnostics surface is **engine-global** (one stream, not per-query): it is a read-only projection of all three planes.

**Snapshot value (FFI `Record`s, in `nmp-ffi/src/types.rs`):**

```rust
// filters/coverage rendered engine-side to their EXACT wire text — the most
// legible form for the screen and literally "what was asked".
FfiDiagnosticsSnapshot {
    relays: Vec<FfiRelayDiagnostics>,
    uncovered_author_count: u32,
    dropped_merge_rules: Vec<String>,
}
FfiRelayDiagnostics {
    relay: String,
    wire_sub_count: u32,
    authors_served: u32,                 // reverse coverage
    by_lane: Vec<FfiLaneCount>,          // { lane: String, count: u32 }
    filters: Vec<String>,                // ConcreteFilter::to_nostr().as_json()
    events_by_kind: Vec<FfiKindCount>,   // { kind: u16, count: u64 }  ← NEW counter
    coverage: Vec<FfiFilterCoverage>,    // { filter: String, coverage: FfiCoverage }
}
```

This satisfies **every** §5 diagnostic requirement: per-relay sub count ✓, exact filters ✓, events per relay per kind ✓, coverage/watermark per (filter, relay) ✓ — and makes REQ **coalescing observable** (N app queries → few wire filters) and reverse coverage observable.

### 1.2 Rust work (additive, headless-testable)

1. **New state on `EngineCore`** (`core/mod.rs`): `events_by_relay_kind: HashMap<RelayUrl, BTreeMap<u16, u64>>`. Bump it in the `RelayMessage::Event` arm (the `relay` is already resolved there via `slot_to_url`; `event.kind.as_u16()`).
2. **`EngineCore::diagnostics_snapshot(&self) -> DiagnosticsSnapshot`** — combine `self.router.diagnostics()` (subs/filters/lanes/authors_served) + `events_by_relay_kind` + per-(relay,filter) coverage via `self.get_coverage(atom, relay)` over the current `router.plan()` reqs. Define an engine-owned `DiagnosticsSnapshot` struct (plane-neutral; `nmp-ffi` mirrors it, same pattern as `FfiFilter` vs `nmp_grammar::Filter`).
3. **`Effect::EmitDiagnostics(DiagnosticsSnapshot)`** — push it at the end of `recompile()` and after the Event/EOSE ingest arms (subs, counts, coverage change points). Coalesce latest-wins in the runtime, exactly like `EmitRows`.
4. **Runtime (`runtime/mod.rs`):** a single diagnostics channel (`Sender<DiagnosticsSnapshot>`), `Handle::observe_diagnostics() -> Receiver<DiagnosticsSnapshot>`, and a `dispatch_effects` arm forwarding `EmitDiagnostics` to it (latest-wins if the consumer is slow).
5. **FFI (`nmp-ffi`):** `#[uniffi::export(callback_interface)] trait DiagnosticsObserver { fn on_snapshot(&self, snapshot: FfiDiagnosticsSnapshot); fn on_closed(&self); }` in `observer.rs`; `NmpEngine::observe_diagnostics(observer) -> Arc<NmpDiagnosticsHandle>` in `facade.rs` (dedicated drain thread, `Drop`-tied teardown — copy `observe`). Records in `types.rs`; conversion in `convert.rs`.
6. **Regenerate the xcframework** (`scripts/build-swift-xcframework.sh --sim-only`).

### 1.3 Swift work (`Packages/NMP/Sources/NMP/`)

- `Diagnostics.swift`: ergonomic value types `DiagnosticsSnapshot`, `RelayDiagnostics`, `LaneCount`, `KindCount`, `FilterCoverage` (mirror pattern of `Row.swift`).
- `DiagnosticsQuery.swift`: `NMPDiagnostics: AsyncSequence` (Element = `DiagnosticsSnapshot`) bridging the observer to an `AsyncStream` (copy `Query.swift`'s `RowBridge`), plus an `@Observable NMPDiagnosticsSnapshotObserver` (copy `Observable.swift`). `NMPEngine.observeDiagnostics() throws -> NMPDiagnostics`.

### 1.4 Headless tests (no simulator)

- `nmp-engine/tests/`: ingest N synthetic events across 2 relays/3 kinds → assert `events_by_kind`; open 2 overlapping queries → assert `wire_sub_count` reflects coalescing and `filters` carry the exact wire JSON; assert an `EmitDiagnostics` is emitted on recompile.
- `Packages/NMP/Tests/`: a construction/shape test that `observeDiagnostics()` yields a snapshot; extend `LiveRelayTests` to assert a live snapshot shows ≥1 relay with a non-empty `eventsByKind` for the follow feed.

---

## 2. The second verify-only gap: editable-kinds live update

**No engine work expected** — this is a confidence check that the DONE grammar behaves. Editing kinds `X` at runtime = the app builds a **new** `NMPFilter` value and re-`observe`s (dropping the old `NMPQuery`); the M2 router already diffs this into surgical CLOSE(old-kind)/REQ(new-kind) deltas. This is idiomatic (SwiftData: change your `@Query` predicate).

- **Verify headlessly:** a Swift package test that observes kind:[1], then re-observes kind:[1,6] with the same authors binding, asserting the second query delivers rows the first excluded and the first is torn down. If (unexpectedly) the wire churns the unchanged authors, file it — but M2 tests say it won't.

---

## 3. The SwiftUI app — idiomatic, its own architecture

Location: `apps/Falsifier/` (new top-level `apps/` dir). **Greenfield, not derived from any NMP app.** Its architecture is ordinary SwiftUI: a plain `@main App`, one `@Observable AppModel` holding the app's OWN state, per-view `@State`. NMP appears only as `import NMP` + method calls. The app owns a single `NMPEngine` constructed once.

```swift
@Observable final class AppModel {
    let engine: NMPEngine                       // constructed ONCE at launch
    var accounts: [Account] = []                // app's own model: { label, pubkeyHex }
    var activePubkey: String? = nil
    var kinds: [UInt16] = [1]                   // user-editable
    init() throws {
        engine = try NMPEngine(config: .init(
            storePath: /* app Caches dir */,
            indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"]))  // EXACTLY 2
    }
}
```

The engine is a plain `let` on the app's own model — **not** an injected NMP container, **not** an `@Environment` NMP object, **not** a scene-phase hook. That placement is itself thesis evidence.

### Screens and the EXACT SDK calls each uses

| Screen | Purpose / proves | SDK calls (the app touches ONLY the two nouns + account switch + diagnostics) |
|---|---|---|
| **AccountsView** | Multi-nsec login + runtime switch. Proves SignerRegistry + identity-as-input. | `try await engine.addAccount(secretKey: nsec)` → append to app's `accounts`; row tap → `try engine.setActiveAccount(pubkey)` + set `model.activePubkey`. App owns the account list; engine holds keys. |
| **FeedView** (source mode 1, flagship) | `$myFollows` at depth-1 through the real reducer. | `engine.observe(FeedFilters.follows(kinds: model.kinds))` → `for await batch in query` (or `NMPQuerySnapshot`). Renders `Row` raw tokens (hex pubkey, unix ts, verbatim content). |
| **KindsEditorView** | User-editable `X`; descriptors are values. | Edits `model.kinds` (`@State` + steppers/chips). FeedView's `.task(id: model.kinds)` rebuilds the filter and re-observes — no NMP call beyond a fresh `observe`. |
| **RelaysView** (source mode 2, scoped) | Relays derived from follows' kind:10002, ranked by frequency. Proves a depth-1 derived read of a *different* kind and makes outbox discovery legible. | `engine.observe(FeedFilters.followsRelayLists())` (`kinds:[10002], authors := Derived(kinds:[3], authors:[$active] → Tag(p))`); app parses each `Row`'s `r` tags, aggregates + ranks by count, displays. **The app cannot pin reads to a picked relay — there is no `relays:` parameter (ledger #3). That impossibility is positive thesis evidence.** |
| **DiagnosticsView** (permanent) | The acceptance test on screen. | `engine.observeDiagnostics()` → `for await snapshot in diags`. Per-relay rows: `wireSubCount`, `filters` (JSON), `eventsByKind`, `coverage`, `authorsServed`. Coalescing (N queries → few filters) and cache-miss authority (`.unknown` vs `.completeUpTo`) both visible. |

`FeedFilters` is an **app-owned** helper (in the app target, NOT the SDK) that builds the follows/relay-list `NMPFilter`s. Keeping it app-side is deliberate: it proves the app — not NMP — owns its query ergonomics, and keeps the SDK from re-baking "blessed sources" (the exact thing VISION §4 warns against).

### Optional / stretch inside the app (drop first if time-pressed)
- **On-demand kind:0 avatars:** FeedView opens a small `engine.observe(kinds:[0], authors:.literal([visiblePubkeys]))` for currently-rendered rows — the pay-as-you-go loader in miniature. Nice-to-have; the feed reads fine without it.
- **Compose/publish:** out of scope for the feed falsifier (see non-goals).

---

## 4. Xcode project + build/run/screenshot loop

- **Project form:** an **iOS App target** in an `.xcodeproj` that adds the local `Packages/NMP` SwiftPM package as a **local package dependency** (a SwiftPM executable cannot run as an iOS app bundle; the app needs an app target). The xcframework is the sim+macOS slice already built by `scripts/build-swift-xcframework.sh --sim-only`.
- **Generate via `xcodegen`** from a **committed `apps/Falsifier/project.yml`** (source of truth); **gitignore the generated `.xcodeproj`** — memory `xcodegen-pbxproj-churn`: xcodegen churns pbxproj UUIDs, so never commit the pbxproj; regenerate it. `project.yml` sets `IPHONEOS_DEPLOYMENT_TARGET: 17.0` (required: `@Observable`/`NMPQuerySnapshot` need iOS 17; also memory `feedback_ios_sim_build`) and a local package ref to `../../Packages/NMP`.
- **Run loop (XcodeBuildMCP):** `session_set_defaults` (project + scheme `Falsifier` + a booted iOS-17 simulator) → verify with `session_show_defaults` → `build_run_sim`. UI drive per memory gotchas: `open_sim()` once before `snapshot_ui`/`wait_for_ui`; `toggle_connect_hardware_keyboard()` before `type_text` (nsec entry); always use MCP `launch_app_sim`, never raw `simctl` (breaks AXe attach). `screenshot` after each screen.
- **Regenerate xcframework** before the first app build if §1 changed the FFI surface.

---

## 5. Build order for Sonnet builders

Fan out per the dependency edges; **B1 gates B3's DiagnosticsView.**

1. **B1 — Diagnostic surface (Rust + FFI + Swift), headless.** §1.2–1.4. Testable with zero simulator: `cargo test -p nmp-engine -p nmp-ffi` + `swift test`. Rebuild xcframework. **This is the only load-bearing engine gap; do it first.**
2. **B2 — Editable-kinds verify (Swift), headless.** §2. `swift test`. Can run parallel to B1 (independent files); merge after B1 if it touches the same test target.
3. **B3 — The SwiftUI app (simulator).** §3–4. Depends on B1 (DiagnosticsView) landing + xcframework rebuilt. Everything else in the app builds against the DONE SDK, so AccountsView/FeedView/KindsEditorView/RelaysView can start immediately; wire DiagnosticsView last.
4. **B4 — Thesis-gate evidence capture (simulator).** §6 checklist + screenshots. After B3 runs green on the sim.

**Headless vs simulator:** B1/B2 are fully headless (Rust + `swift test` on the macOS slice). B3/B4 require the booted simulator and XcodeBuildMCP.

---

## 6. Thesis-gate evidence — the checklist the owner runs

The kill (VISION §6): *after honest effort, the app still needs NMP-shaped scaffolding, OR a normal iOS dev couldn't have written it from SwiftData/Query knowledge.* This is the owner's human call on the running app. The plan makes it legible via a **framework-smell audit of the app's own code** — builders must be able to answer every row "CLEAN," and any "SMELL" is a reportable finding, not something to paper over.

| Would REVEAL a framework smell (KILL indicators) | What CLEAN looks like here |
|---|---|
| A mandatory NMP app object / container the app must be built around | Engine is a plain `let engine` on the app's OWN `@Observable AppModel`; no NMP base class, no `@Environment` NMP object, no required provider. |
| An inherited NMP state model (AppState/reducer/projection the app subclasses or registers into) | App holds its own `accounts`/`kinds`/`activePubkey` `@State`; NMP holds none of the app's state. |
| The app managing relay/subscription lifecycle (open/close REQs, relay lists, reconnection) | App never names a relay except the 2 indexers at construction; no subscription bookkeeping — `for await`/deinit does it. RelaysView proves there's *no* `relays:` knob to even misuse (ledger #3). |
| Presentation forced into the engine (formatted fields, display helpers on FFI types) | `Row` carries raw tokens only; all formatting is app-side in the View. |
| A second mechanism for read demand or write intent beyond the two nouns | Only `observe(filter)` and (unused here) `publish(intent)` cross the boundary; diagnostics is read-only, off the data path. |
| Adoption cost disproportionate to use / "you must learn NMP's architecture" | `import NMP; let e = try NMPEngine(config:); for await b in e.observe(f)` is the whole story. The one learning surface — the filter-binding grammar — is wrapped by an **app-owned** `FeedFilters`, proving the app owns its own ergonomics. |

**Screenshots to capture (B4):**
1. AccountsView with ≥2 accounts; mid-switch and post-switch feed.
2. FeedView showing live `$myFollows` rows.
3. KindsEditorView changing `[1] → [1,6]` and the feed updating live (before/after pair).
4. RelaysView: follows' 10002 relays ranked by count.
5. DiagnosticsView: per-relay sub counts + exact filters + events-by-kind + coverage — captured (a) at steady state and (b) immediately after an account switch, showing **zero stale subs for the old pubkey** (ledger #10 evidence).

---

## 7. Honest read on whether M5 can pass (friction already visible)

The SDK is genuinely library-shaped — construct-once, values-not-handles, `AsyncSequence`, deinit-tied teardown, no imposed architecture. The strongest pass evidence is structural: several classic Nostr client bugs have **no app-side surface to write** (no `relays:` param, no expansion seam), and RelaysView demonstrates that on screen.

The sharpest **friction** — the one thing a SwiftData dev would *not* write intuitively — is the reactive filter-binding grammar: `.derived(inner: NMPFilter(kinds:[3], authors:.reactive(.activePubkey)), project:.tag("p"))` for "my follows." It is powerful and correct (values, not code) but it is a mini query-algebra with a learning curve. The plan's mitigation (an **app-owned** `FeedFilters` wrapper) is the right answer *and* itself a positive signal (the app, not NMP, owns ergonomics) — but the owner should watch for whether that wrapper feels like "convenience" or like "scaffolding NMP made me write." That is the single place the human judgment will actually bite.

---

## 8. Non-goals (explicit)

- **NIP-17 DMs / private lists** — routing + decrypt feedback are `known-gaps.md` deferrals; the falsifier does feeds.
- **Wallet (NIP-60/61)** — out.
- **NIP-51 bookmarked relay-set mode (kind:30002, ranked by bookmark count)** — **STRETCH / follow-on only.** No engine primitive exists (VISION §8). Source mode 2 proves the **10002-derived** relay list first; flag the bookmarked-set mode as a follow-on if M5 is at risk it drops first (VISION §8).
- **NIP-29 depth-2 groups in the app** — generality was already proven headless in M1; not required for the M5 app (§5's depth-2 lives in the spike, not the falsifier UI).
- **Pre-signed / verbatim publish** — FFI is unsigned-only (`known-gaps.md`); irrelevant to a read-feed falsifier.
- **Android / Kotlin-Flow SDK** — **M6.**
- **A D8-compliant tick driver** for negentropy liveness sweeps — deferred (`known-gaps.md`); the feed path works without it.
