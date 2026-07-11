# M4 — Swift SDK boundary: implementation plan

- **Date:** 2026-07-11
- **Status:** Provisional-until-v2 (no self-compat obligation). Builder-facing plan for M4 per `docs/VISION.md` §6.
- **Milestone:** M4 — the FFI seam + a Swift package exposing the **two nouns** (live query, write intent) plus the diagnostic surface as native values. Minimum surface — exactly what the M5 falsifier (`docs/VISION.md` §5) consumes, nothing more.
- **Builds on:** M3 (`nmp-engine`: `EngineThread` + `Handle`; `EngineCore` reducer; `nmp-store`/`nmp-resolver`/`nmp-router`/`nmp-transport`/`nmp-signer`). The M3 runtime `Handle` (`nmp-engine/src/runtime/mod.rs`) is the exact seam M4 wraps — it already returns rows+coverage on a `Receiver<RowsMsg>` and receipt status on a `Receiver<WriteStatus>`. M4 adds NO reducer behaviour; it wraps the existing channel surface and adds one engine change (multi-account signer, §5).
- **Gate:** `docs/VISION.md` §6 M4 — **Tier A on the public SDK shape** (the product's face, expensive to re-cut), **then running** (a tiny Swift harness / XCTest against a local engine, no app UI). Plus **Tier A on the Collection observation-mode result contract** (VISION §10 — a result-contract amendment; §6 below states exactly what the round must resolve).
- **Kill (VISION §6 M4):** delivering native ergonomics forces the SDK to grow app-lifecycle machinery — scene-phase hooks, a mandatory container/environment object, a required "NMPProvider" the app must wrap its view tree in. If the falsifier can't do `import NMP; let q = try await nmp.observe(...)` from a plain view without adopting SDK-shaped scaffolding, the library thesis has failed at the boundary. §7 specifies how a builder detects this early.

M4 is not thesis-level fragile the way M1 (grammar) or M5 (the human judgment) are; the two-noun values already survive a thread boundary as plain `Send` values in M3. The open questions are all *ergonomic shape*, concentrated in two places: the Rust-channel → Swift-`AsyncSequence` bridge (§4) and the Collection observation contract (§6). Both are called out explicitly.

---

## 1. FFI mechanism: **UniFFI** (proc-macro, not UDL)

**Decision: UniFFI 0.28+, proc-macro mode (`#[uniffi::export]` / `#[derive(uniffi::Record)]` / `#[derive(uniffi::Enum)]`), no `.udl` file.** Justification against the two alternatives:

- **vs. manual C-ABI / `cbindgen`.** The whole boundary is *value marshalling*: `Filter`/`Binding`/`Selector` descriptors in, `Row`/`WriteStatus`/coverage out. UniFFI generates the Swift `struct`/`enum` mirror of a Rust `Record`/`Enum` for free, including nested enums (`Binding` is a recursive enum — UniFFI supports this natively via boxing). A manual C-ABI would hand-write dozens of `repr(C)` shims, opaque-pointer lifecycle, and a Swift decoding layer for every type, and would re-introduce exactly the "codegen residue" VISION §4 warns against. The two-noun surface is small but *deeply structured*; that is UniFFI's sweet spot.
- **vs. `cxx`.** `cxx` targets C++ interop and shines when sharing C++ types; it does not generate Swift and has no Kotlin story. M6 is Kotlin/Flow — UniFFI generates Swift **and** Kotlin from one interface, which is the whole reason the boundary must be "Swift-shaped but platform-neutral" (VISION §6 M6's kill). `cxx` would strand M6.
- **The old repo used UniFFI** (`nmp-uniffi`, later folded) and the operational lessons — bindgen invocation, xcframework assembly, the callback-interface pattern — are harvest candidates (§3, §8). We are re-earning a proven toolchain, not adopting an unknown.

**The wrinkle (the crux of "does it feel like a library"):** UniFFI exports **callback interfaces / foreign traits**, not `AsyncSequence`. A live query is a *stream*, and UniFFI has no native stream type. So the plan's load-bearing work is the bridge in §4: a UniFFI **foreign-trait observer** that the Rust side drives (draining M3's `mpsc::Receiver`), which a **thin hand-written Swift layer** adapts into `AsyncStream`. UniFFI's own async support (`async fn` → Swift `async`) is used for the *one-shot* calls (`publish`, `addAccount`); it is **not** used for the streaming reads (an `async fn` yields one value, a query yields forever). This split — UniFFI async for one-shots, callback-interface-drained-into-`AsyncStream` for streams — is the entire architecture of the bridge.

---

## 2. Crate + package layout delta

M4 adds **one Rust crate** and **one Swift package**. The engine crates are untouched except the §5 signer change in `nmp-engine`.

```
nmp-ffi            (M4 NEW) the UniFFI boundary crate — crate-type = ["staticlib","lib"]
  src/lib.rs         uniffi::setup_scaffolding!(); the exported facade
  src/types.rs       #[derive(uniffi::Record/Enum)] mirrors of the two-noun values
  src/facade.rs      `NmpEngine` exported object: observe/publish/addAccount/setActiveAccount/diagnostics
  src/observer.rs    the foreign-trait observers (RowObserver/ReceiptObserver/DiagnosticObserver)
                     + the Rust-side drain threads (channel → observer)
  src/convert.rs     FfiFilter → nmp_grammar::Filter (and back for diagnostics)
  deps: nmp-engine, nmp-grammar, nostr, uniffi
  build.rs: uniffi::generate_scaffolding? NO (proc-macro mode) — build.rs only for the
            bindgen bin target used by the packaging script (§3).

Packages/NMP/         (M4 NEW) the Swift package (SwiftPM)
  Package.swift        two targets: `NMPFFI` (binaryTarget → NMP.xcframework) + `NMP` (Swift)
  Sources/NMP/         the ergonomic hand-written layer — THE part that makes it feel native:
    Engine.swift         `public final class NMPEngine` (wraps the UniFFI object)
    Query.swift          `observe(...) -> NMPQuery`; NMPQuery: AsyncSequence, deinit → unsubscribe
    Collection.swift     observeCollection(...) (Tier-A gated §6) — thin over the same object
    Receipt.swift        publish(...) async -> Receipt; Receipt.status: AsyncStream<WriteStatus>
    Observable.swift     @Observable snapshot adapter ON TOP of NMPQuery (NOT primary API)
    Diagnostics.swift    diagnostics() -> AsyncStream<DiagnosticSnapshot>
  Sources/NMPFFI/       the UniFFI-generated Swift (nmp.swift) + modulemap (checked in or built)
```

**`nmp-ffi` depends on `nmp-engine`; nothing depends on `nmp-ffi`** (it is the top of the graph, replacing what would have been an app). It is a **native-only** crate (staticlib for iOS device+sim); wasm remains out (VISION §8, non-goal §8 below). The Swift `NMP` target is where "library not framework" is won or lost: it holds **zero engine concepts** — only `AsyncSequence` conformances, `deinit` wiring, and the `@Observable` sugar.

---

## 3. Build / packaging: cargo → xcframework → SwiftPM

Toolchain: macOS + Xcode (present). Rust targets: `aarch64-apple-ios` (device), `aarch64-apple-ios-sim` + `x86_64-apple-ios-sim` (simulator). One script, `ci/build-swift-xcframework.sh` (harvest the old repo's equivalent for the invocation shape, re-justified):

1. `cargo build -p nmp-ffi --release --target <each of the 3 triples>` → three `libnmp_ffi.a`.
2. `lipo -create` the two simulator arches into one fat sim staticlib (device stays separate — xcframework requires arch-disjoint slices).
3. **Bindgen:** `cargo run -p nmp-ffi --bin uniffi-bindgen generate --library <one .a> --language swift --out-dir gen/` (library mode — reads the exported metadata straight from the compiled staticlib, no `.udl`). Produces `nmp.swift`, `nmpFFI.h`, `nmpFFI.modulemap`.
4. `xcodebuild -create-xcframework -library <device.a> -headers gen/ -library <sim-fat.a> -headers gen/ -output Packages/NMP/NMP.xcframework`.
5. `Package.swift` `NMPFFI` = `.binaryTarget(path: "NMP.xcframework")`; the generated `nmp.swift` compiles in `Sources/NMPFFI`; `NMP` (the ergonomic target) depends on `NMPFFI`.

**How M5 links it:** M5 is a normal Xcode app that adds the local SwiftPM package `Packages/NMP` and `import NMP`. No build-phase scripts in the app, no bridging header the app maintains — the xcframework carries the static lib, the package carries the Swift. That "add a package, `import`, done" is itself part of the kill test (§7): if M5 needs a Run Script phase or a manual `.a` link, the packaging failed the library bar.

---

## 4. The Rust-channel → Swift-`AsyncStream` bridge (concrete)

This is the heart of M4. M3's `Handle::subscribe` returns `(QueryHandle, Receiver<RowsMsg>)` where `RowsMsg = (Vec<RowDelta>, QueryCoverage)`. UniFFI cannot return a Rust `Receiver`. Bridge in three layers:

**(a) UniFFI foreign-trait observer (Rust-declared, Swift-implemented).**

```rust
#[uniffi::export(callback_interface)]
pub trait RowObserver: Send + Sync {
    fn on_batch(&self, rows: Vec<FfiRow>, coverage: FfiCoverage);
    fn on_closed(&self);                       // engine tore the sub down (grace elapsed / shutdown)
}
```

**(b) Rust-side drain.** `NmpEngine::observe(query, observer)` calls the M3 `Handle::subscribe`, gets the `Receiver<RowsMsg>`, and spawns **one dedicated drain thread** per subscription that blocks on `rx.recv()` (D8: blocking recv, never poll — the same discipline the engine thread already uses) and, for each batch, calls `observer.on_batch(...)` across the FFI. When `recv()` returns `Err` (sender dropped → engine closed the sub), it calls `observer.on_closed()` and exits. `observe` returns an opaque `#[uniffi::export] NmpSubscription` object whose Rust `Drop`/an explicit `cancel()` calls M3 `Handle::unsubscribe` (drops demand).

**(c) Swift `AsyncStream` adapter (hand-written, in `Sources/NMP`).** A private `final class RowBridge: RowObserver` implements the callback interface and holds an `AsyncStream<RowBatch>.Continuation`: `on_batch` → `continuation.yield(...)`; `on_closed` → `continuation.finish()`. `NMPQuery` conforms to `AsyncSequence` (element `[Row]` or `RowBatch` carrying coverage) backed by that stream. **`NMPQuery.deinit` calls `subscription.cancel()`** — deinit-tied demand drop (VISION §6 M4). The last-observer teardown *grace/debounce* is **engine-side** (M3 already owns refcount+debounced teardown per VISION §4/ledger #2); the SDK does not implement grace, it only signals demand-drop on deinit and lets the engine debounce. (This resolves VISION §8's open "where does debounce live" toward **engine-global default**, surfaced no further — recorded as the M4 evidence that question was deferred to.)

**Receipts** use the identical pattern with a `ReceiptObserver` foreign trait draining M3's `Receiver<WriteStatus>`; the Swift side exposes `Receipt.status: AsyncStream<WriteStatus>`. **Diagnostics** likewise (§5 needs a snapshot stream).

**One-shots stay UniFFI-async.** `addAccount` and the `publish` *enqueue call* (returning the receipt handle, not the stream) are plain `#[uniffi::export] async fn` → Swift `async` — no bridge needed; a single value crosses.

---

## 5. The engine change: multi-account signer (closes the known-gap)

`docs/known-gaps.md` flags this as an M4/M5 blocker: M3's `EngineThread::spawn` takes **one** `signer: Sig` fixed at construction, and `Effect::RequestSign` in `runtime/mod.rs::dispatch_effect` calls that one signer. `set_active_pubkey` re-roots only the **read** graph (`EngineCore::on_set_active_pubkey` → `resolver.set_active_pubkey`). So publishing *as the switched-to account* has no path.

**The change is localized to `nmp-engine/src/runtime/mod.rs`** — the signer already lives in the runtime layer, not in `EngineCore` (core only emits `Effect::RequestSign(id, unsigned)`; the runtime picks the capability). Minimum shape:

- The engine thread holds a **`SignerRegistry`**: `HashMap<PublicKey, Box<dyn SigningCapability + Send>>` plus an `active: Option<PublicKey>`, instead of a single `signer` moved into `engine_loop`.
- Two new `Cmd`/`Handle` verbs:
  - `add_signer(signer)` — registers a capability keyed by `signer.public_key()`; returns the pubkey.
  - `set_active_account(Option<PublicKey>)` — **one verb that does both halves**: sends `EngineMsg::SetActivePubkey(pk)` into `EngineCore` (read re-root, already proven in M1/M3) **and** sets the registry's `active` pointer (write capability). This is the structural expression of VISION P3 "identity is one input": one call moves the read root and the signing capability together, so they can never diverge (ledger #10 — no second place account context lives).
- `Effect::RequestSign` dispatch changes from `signer.sign(...)` to `registry.active_signer()?.sign(...)`; if no active signer, the receipt terminates `WriteStatus::Failed("no active signer")` (typed state, no panic — errors are values, VISION §6 M4).

`EngineCore` and every M3 reducer test are **unchanged** — this is purely the runtime layer's capability selection. Spawn signature changes from `spawn(store, dir, cap, pool_cfg, signer)` to `spawn(store, dir, cap, pool_cfg)` + post-spawn `handle.add_signer(...)`; the engine may start with zero accounts (read-only, `ActivePubkey = None`), matching a logged-out launch.

**FFI/Swift surface for it:** `addAccount(secretKeyHex) async throws -> Pubkey` (constructs a `LocalKeySigner` **Rust-side** — the secret crosses the boundary once as a value and thereafter lives in the engine, honouring "the key lives in the engine", VISION §2/ledger #12); `setActiveAccount(_ pubkey: Pubkey?)`. A remote-signer account (NIP-46/55) is a later `addRemoteSigner(observer:)` using the same registry seam — **not built in M4** (non-goal §8).

---

## 6. Tier-A items (flag for propose/refute BEFORE building the affected surface)

Two Tier-A rounds. Mechanical builders do **not** proceed on these two until the round resolves; everything else in §8's build order is running-gated only.

**Tier-A #1 — the public SDK shape (VISION §6 M4 gate).** The `NMPEngine`/`NMPQuery`/`Receipt` Swift signatures in §9 are the product's face. Propose/refute must resolve: (a) is `observe` a method on an engine object or a free function — does the app hold one `NMPEngine` (acceptable: one construction call) or is even that "a container the app must adopt"? (b) does `NMPQuery` yield `[Row]` (full snapshot each time) or `RowBatch` deltas — the SwiftData `@Query` lesson says the *primary* handle is the detachable sequence, snapshot is sugar, but the element type is load-bearing for the `@Observable` adapter; (c) error surface — thrown at `observe` call vs. a terminal `AsyncStream` element. The round's output is the frozen `Sources/NMP` public signatures.

**Tier-A #2 — the Collection observation-mode result contract (VISION §10).** This **amends the result contract** (§10 is provisional-until-M4 by its own terms). The round must resolve, concretely:
  1. **`OrderKey` / `RowKey` as closed UniFFI enums** — the exact variant set (e.g. `OrderKey::CreatedAtDesc | CreatedAtAsc`; `RowKey::EventId | ReplaceableAddress`). They are **values, never Swift comparators/closures** (VISION §10 "values in, code after"; the rejected v1-feed-framework line). Enumerate what M5's two feeds actually need and no more.
  2. **`has_more` / coverage surfacing** — `loadMore` widens the query node's own `since/until/limit` (widen-only, P5) and returns updated `QueryCoverage` (`CompleteUpTo` vs `Unknown` — ledger #7's variant, already computed in `nmp-engine/src/core/coverage_query.rs`). Resolve: is `exhausted` a distinct third state or just `CompleteUpTo(0)`/window-floor-proven?
  3. **Heterogeneous feeds** — `observeCollection([query, …])` unions delivered rows, dedups by `RowKey`. Resolve where the merge lives: engine-side (a new engine observation mode over a shared demand node, VISION §10's position) vs. a **Swift-side** merge of N `NMPQuery` streams keyed by the closed `RowKey`. **Strong prior:** if M5's two feeds are each a *single* query, the Swift-side merge may be sufficient for M4 and defers engine work — the round must decide whether the falsifier truly needs engine-maintained ordering or whether ordered windowing over one query's replica is enough. This is the one place M4 could accidentally re-grow a feed framework; the round exists to prevent it.
  4. Candidate **bug-ledger #13** (two-cursor separation, VISION §10): confirm the SDK never lets an app-computed cursor feed engine correctness.

**Recommendation to the owner:** run **Tier-A #2 as a Fable consult** (see the return note) — it is a reconciliation/deep-contract question and VISION §10 was itself settled by a Fable consult. Tier-A #1 can be the standard two-Opus propose/refute; the AsyncStream ergonomics (§4) are well-trodden and do **not** need Fable.

---

## 7. The kill test — detect it early (VISION §6 M4)

The kill fires if native ergonomics force app-lifecycle machinery into the SDK. Builders check it continuously with a **canary**: a ~15-line SwiftUI view in the running harness that does exactly

```swift
import NMP
struct FeedView: View {
    let nmp: NMPEngine
    @State var rows: [Row] = []
    var body: some View {
        List(rows) { ... }
            .task {
                for await batch in nmp.observe(myFollowsQuery) { rows = batch.rows }
            }
    }
}
```

The kill is **detected** the moment this canary requires ANY of: a scene-phase (`@Environment(\.scenePhase)`) hook to keep subscriptions alive; a mandatory `NMPProvider`/environment-object wrapper around the view tree; an app-owned container the SDK requires the app to construct and thread through; or manual teardown code (the `.task` cancellation on view disappearance must be enough — deinit-tied drop, §4c). If the canary stays this small and needs none of that, M4 passes its own gate; the final human judgment is M5's. Builders run the canary after step C (§8) and treat any scaffolding creep as a stop-and-escalate, not a workaround (`feedback_no_hacks`).

---

## 8. Build order for Sonnet builders

`‖` = parallel (disjoint). Each step independently green (Swift builds + Rust `cargo test -p nmp-ffi` where applicable; **never** `--workspace`, per `AGENTS.md`).

- **Step 0 — scaffold.** Add `nmp-ffi` (crate-type staticlib+lib, `uniffi` dep, `uniffi::setup_scaffolding!()`), empty `Packages/NMP` SwiftPM skeleton, the build script stub. *Green:* `cargo build -p nmp-ffi`.
- **A — the value types (`nmp-ffi/src/types.rs` + `convert.rs`).** `#[derive(uniffi::Record/Enum)]` mirrors: `FfiFilter`, `FfiBinding` (recursive enum), `FfiSelector`, `FfiIdentityField`, `FfiSetAlgebra`, `FfiRow`, `FfiCoverage`, `FfiWriteIntent`, `FfiDurability`, `FfiWriteRouting`, `FfiWriteStatus`, `Pubkey`(string newtype). `convert.rs`: `FfiFilter → nmp_grammar::Filter` and `nostr::Event → FfiRow` (raw tokens only — hex pubkey, unix ts, verbatim content; **no formatted fields**, ledger #12). *Green:* Rust round-trip unit tests (`FfiFilter → Filter → FfiFilter`); bindgen produces Swift structs. **Depends on Tier-A #1** for the exact `FfiRow` shape.
- **B ‖ — the engine signer change (`nmp-engine/src/runtime/mod.rs`).** `SignerRegistry`, `add_signer`, `set_active_account`; `Effect::RequestSign` dispatch via active signer; `Failed("no active signer")` path; `spawn` drops the `signer` arg. *Green:* new runtime tests — publish-after-switch signs with the switched-to key; publish with no active account terminates `Failed`; §5 read re-root still passes M3 tests. (Disjoint from A — different crate.)
- **C — the facade + bridge (`nmp-ffi/src/facade.rs` + `observer.rs`).** `NmpEngine` exported object wrapping `EngineThread`+`Handle`; the three foreign-trait observers; the drain threads (§4); `NmpSubscription`/`NmpReceipt` opaque objects with cancel-on-drop. `addAccount`/`setActiveAccount`/`publish`/`observe`/`diagnostics`. Depends on A + B. *Green:* Rust tests driving `NmpEngine` with a mock `RowObserver` (assert batches + `on_closed` on cancel).
- **D — the Swift ergonomic layer (`Sources/NMP`).** `NMPEngine`, `NMPQuery: AsyncSequence` + deinit→cancel, `Receipt.status`, `diagnostics()`, the `@Observable` snapshot adapter, `DiagnosticSnapshot`. Depends on C + the xcframework (step F). *Green:* the §7 canary compiles; XCTest drives an in-process engine against a local `nak serve` / `MockRelay`.
- **E — packaging (`ci/build-swift-xcframework.sh`).** The full §3 pipeline; wire `Package.swift`. Depends on A/B/C (needs the exported surface stable). *Green:* `swift build` on the package; the generated `nmp.swift` compiles.
- **F — Collection observation mode.** ONLY after **Tier-A #2** resolves: `OrderKey`/`RowKey` UniFFI enums, `observeCollection`, `loadMore`, `has_more`/coverage surfacing per the round's ruling. Engine-side vs Swift-side merge per the round. Depends on D. *Green:* an ordered-window XCTest (insert/remove/move deltas; `loadMore` widens and coverage advances).

**Parallelism:** A ‖ B immediately (different crates). C waits on both. D+E after C. F last, Tier-A-gated. Two–three Sonnet builders (A, B, then C→D→E chain; F after the consult).

**Testable in M4 vs deferred to M5:** *In M4 (a tiny Swift harness / XCTest, NO app UI):* value round-trips; observe→AsyncStream delivery + deinit-drop; publish→receipt stream; multi-account add/switch/sign; diagnostics stream; the §7 canary compiles. *Deferred to M5:* the full falsifier app, device runs, the qualitative "native library or framework?" human judgment, on-demand kind:0 as UI-driven queries, the permanent diagnostics screen's presentation.

---

## 9. The minimal Swift API surface M5 needs (signatures — sketches, not bodies)

```swift
public final class NMPEngine {
    public init(config: NMPConfig) throws            // ONE construction call — indexers only, no content relays
    // ---- identity (P3; §5 multi-account) ----
    public func addAccount(secretKey: String) async throws -> Pubkey   // key lives engine-side after this
    public func setActiveAccount(_ pubkey: Pubkey?)                    // re-roots reads AND signing together
    // ---- read noun ----
    public func observe(_ query: Filter) -> NMPQuery                    // detachable, primary API
    public func observeCollection(_ queries: [Filter],                  // Tier-A #2 gated (§6)
                                  order: OrderKey, rowKey: RowKey) -> NMPCollection
    // ---- write noun ----
    public func publish(_ intent: WriteIntent) async throws -> Receipt
    // ---- diagnostics (VISION §4 "two nouns plus diagnostics") ----
    public func diagnostics() -> AsyncStream<DiagnosticSnapshot>
}

public struct NMPQuery: AsyncSequence {            // element = RowBatch { rows: [Row]; coverage: Coverage }
    // deinit of the underlying iterator/bridge → engine unsubscribe (demand drop). No cancel() call required of the app.
}
@Observable public final class NMPQuerySnapshot {  // sugar ON TOP of NMPQuery — NOT the primary API
    public private(set) var rows: [Row]
    public private(set) var coverage: Coverage      // .completeUpTo(Timestamp) | .unknown  (ledger #7)
}
public struct Receipt {
    public let status: AsyncStream<WriteStatus>     // .accepted/.signed/.routed/.sent/.acked/.rejected/.gaveUp/.failed
}
public struct NMPCollection: AsyncSequence {        // element = CollectionDelta (insert/remove/update/move)
    public func loadMore() async                    // widens the node's own since/until/limit (P5, widen-only)
    public var hasMore: Bool { get }                // ledger #7 coverage variant surfaced
}

// value descriptors (UniFFI records/enums, all Hashable/Sendable):
//
// Superseded by #64: the shipped `NMPFilter`/`NMPSelector` (Packages/NMP/
// Sources/NMP/NMPFilter.swift) key `tags` by `Character` (the wire/local
// indexed-filter alphabet, all 52 ASCII letters, no whitelist) and give
// `Selector.tag` a `String` (an arbitrary already-acquired event-tag key,
// never restricted to one character) -- NOT the single conflated `TagName`
// sketched below, which this plan pre-dated the split by.
public struct Filter { var kinds: [UInt16]?; var authors: Binding?; var ids: Binding?
                       var tags: [TagName: Binding]; var since: UInt64?; var until: UInt64?; var limit: UInt32? }
public indirect enum Binding { case literal(Set<String>); case reactive(IdentityField)
                               case derived(inner: Filter, project: Selector); case setOp(SetAlgebra, [Binding]) }
public enum Selector { case authors; case ids; case tag(TagName); case addressCoord }
public enum IdentityField { case activePubkey }
public enum SetAlgebra { case union; case intersect; case diff }
public struct WriteIntent { var payload: WritePayload; var durability: Durability; var routing: WriteRouting }
public enum WriteRouting { case authorOutbox; case toInboxes([Pubkey]); case privateNarrow([RelayUrl]) }
```

Mapping to the §5 falsifier: multi-nsec + switch = `addAccount`/`setActiveAccount`; editable-kinds-live = `Filter` is a value, re-`observe` a new value; source mode 1/2 = the `Binding`/`Derived` grammar; on-demand kind:0 = ordinary `observe(kinds:[0], authors: literal(visible))` keyed to rendered rows (**no new primitive** — pay-as-you-go via the same noun); diagnostics screen = `diagnostics()`.

---

## 10. Explicit non-goals (do not gold-plate)

- **No app UI / no falsifier app** — that is M5. M4 ships the package + a test harness only.
- **No Android / Kotlin / Flow** — M6. (UniFFI's multi-target generation is *why* the boundary is chosen now, but the Kotlin target is not built or tested in M4.)
- **No wasm / TS SDK** — out of v2 (VISION §8); `nmp-ffi` is native staticlib only.
- **No remote signer (NIP-46/55)** — the `SignerRegistry` seam admits it later (`addRemoteSigner`); M4 ships `LocalKeySigner` accounts only.
- **No new engine reducer behaviour** — M4 wraps M3's channel surface + the one runtime signer change (§5); it does not touch `EngineCore`, `nmp-resolver`, or `nmp-router`.
- **No Collection engine work before Tier-A #2** — step F is gated; if M5 is at risk, a Swift-side single-query ordered window ships first and engine-maintained heterogeneous merge is deferred (VISION §10 stretch).
- **No presentation anywhere in `nmp-ffi`** — `FfiRow` carries raw tokens only (ledger #12); a formatted-string field is a red build.
- **No scene-phase / lifecycle hooks in the SDK** — that is the kill (§7); their appearance is a stop-and-escalate.
```
