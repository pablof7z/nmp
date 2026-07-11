# Packaging, build & distribution

**Status: BUILT** for iOS/Swift (xcframework + SwiftPM) and Rust (Cargo). The Android AAR (cargo-ndk), web (wasm-bindgen), and per-NIP module packaging are **PLANNED-shape** — the intended pipelines, marked as such. Everything in the Swift and Rust sections is the real, current build.

After this chapter you'll know how the compiled Rust core is packaged for each platform, how to regenerate it, how to pin the FFI version so a core/binding mismatch can't ship, and why the modularity principle means *you compose only the modules you enable* — which is a packaging fact, not just an API one.

---

## The shape of an NMP build

Every platform is the same two layers:

1. **The Rust core** (`nmp-engine` and its sibling crates), compiled to a native library for the target.
2. **A thin binding layer** that presents the two nouns in the platform's idiom — hand-written ergonomics over UniFFI-generated (Swift/Kotlin) or wasm-bindgen-generated (web) glue.

You never ship Rust source to an app developer. You ship the compiled core plus the binding package. The Swift path below is fully wired; the others follow the identical pattern with a different codegen.

## iOS / Swift — xcframework + SwiftPM (BUILT)

The Swift package at `Packages/NMP` has three targets (`Package.swift`):

```swift
targets: [
    .binaryTarget(name: "nmp_ffiFFI", path: "NMP.xcframework"),  // compiled Rust core
    .target(name: "NMPFFI", dependencies: ["nmp_ffiFFI"]),       // uniffi-generated bindings
    .target(name: "NMP", dependencies: ["NMPFFI"]),              // hand-written ergonomics
]
// Only `NMP` is imported by a consuming app. NMPFFI/nmp_ffiFFI are plumbing.
```

`NMP.xcframework` and the generated `Sources/NMPFFI/nmp_ffi.swift` are **build artifacts, not committed** (they're in `.gitignore`). Regenerate both with one script:

```bash
scripts/build-swift-xcframework.sh            # device + sim + macOS slices
scripts/build-swift-xcframework.sh --sim-only # skip device (no signing needed)
```

What that script does, in order:

1. `cargo build -p nmp-ffi --release` for each Apple target — `aarch64-apple-ios` (device), `aarch64-apple-ios-sim` + `x86_64-apple-ios` (simulator), `aarch64-apple-darwin` (so `swift test`'s host process, which runs on your Mac's own arch, can link the binary at all).
2. `lipo` the two simulator arches into one fat staticlib (an xcframework requires arch-disjoint slices, so device stays separate).
3. Run `uniffi-bindgen` in **library mode** against a compiled staticlib — it reads exported metadata straight out of the binary, no `.udl` file — producing `nmp_ffi.swift` (into the `NMPFFI` target) and the C header + `module.modulemap` (into the xcframework's headers slice).
4. `xcodebuild -create-xcframework` the device + fat-sim + macOS slices into `Packages/NMP/NMP.xcframework`.

### Binary vs source distribution

The `nmp-ffi` crate is built as both (`Cargo.toml`):

```toml
[lib]
crate-type = ["staticlib", "lib"]   # staticlib → the xcframework; lib → round-trip unit tests
```

- **Binary distribution** (a prebuilt `NMP.xcframework` behind a versioned `binaryTarget` URL) is the shape a *published* SDK takes — an app pulls the package and never compiles Rust. Startup is a plain `dlopen` of an already-linked static core.
- **Source/local distribution** (a `path:` dependency on `Packages/NMP`, as the Falsifier's `project.yml` uses) is the shape you use *inside the monorepo* or when you want to rebuild the core yourself. `packages: NMP: { path: ../../Packages/NMP }` — four lines, and you're building against local Rust.

Either way the app imports exactly one module (`NMP`) and sees zero `Ffi`-prefixed types.

## Rust — crate + feature flags (BUILT)

A Rust consumer depends on the crates directly (`nmp-engine`, `nmp-grammar`, `nmp-signer`, `nmp-store`, `nmp-router`, `nmp-transport`, `nmp-resolver`) and holds the `Handle`. There's no binding layer — the two nouns are the native API. This is the leanest possible build: you link the core and nothing else.

The Rust build is also where the **modularity principle becomes a packaging lever** (see below): protocol-specific functionality is gated behind Cargo features / separate crates, so a Rust binary that never reacts never compiles reaction code.

## Android / Compose — cargo-ndk + AAR (PLANNED-shape)

> **PLANNED-shape.** Not built yet (M6). Intended pipeline, mirroring the Swift path with Android codegen:

1. `cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 build --release` to produce the JNI `.so` per Android ABI.
2. `uniffi-bindgen` (Kotlin mode) to generate the Kotlin bindings.
3. Package the `.so`s + generated Kotlin + a thin hand-written ergonomic layer (cold `Flow` delivery) as an **AAR** consumed by a Compose app via Gradle.

The two nouns keep their names and shapes; only the codegen and the reactive wrapper (`Flow` instead of `AsyncSequence`) differ.

## TypeScript / web — wasm-bindgen (PLANNED-shape)

> **PLANNED-shape.** Unconfirmed for v2. Intended pipeline:

1. `wasm-pack build --target web` (wasm-bindgen) to compile the core to `wasm32` + generated JS/TS glue.
2. A thin hand-written layer presenting the two nouns as async iterators.
3. Distributed as an npm package; the store backs onto OPFS-SQLite in the browser.

## FFI version-pinning — the mismatch you must make unshippable

The generated bindings and the compiled core are a *matched pair*: the Swift/Kotlin binding assumes the exact FFI ABI the core exports. Ship a binding generated against core v0.3 over a core binary built at v0.4 and you get memory corruption, not a clean error. So the pin is not optional hygiene — it's a safety boundary.

The rule: **the binding layer and the core binary are versioned and released together, never independently.** Concretely —

- The bindings are *regenerated from the very binary they'll ship with* (step 3 of the Swift script reads metadata out of the compiled staticlib), so a hand-edited or stale `nmp_ffi.swift` can't silently drift — it's overwritten on every build.
- A binary-distributed `binaryTarget` pins the xcframework by version (and checksum, for a remote URL). Bump the core → rebuild the xcframework → bump the package version → regenerate bindings, as one atomic release. There is no supported path where an app resolves a binding at one version and a core at another.
- New or changed FFI surface follows the same discipline as any grammar change: it's a deliberate, versioned event, not an incidental PR. The FFI seam is where the "nouns are the invariant" contract is physically enforced.

Treat "the binding and the core came from the same build" as an invariant your release process guarantees, and the mismatch class of bug never reaches a user.

## Binary size & startup budget — and why modularity is the lever

Two budgets matter for an embedded engine: the **binary size** it adds to your app, and the **startup cost** to construct it. Construction is cheap — `NMPEngine(config:)` spins up the engine's interior threads and (with a `storePath`) opens the SQLite store; there's no network round-trip on the critical path, and cold-start reads serve from the persisted cache immediately. The size budget is where your choices show up, and that's the **modularity principle** as a packaging fact:

> **You compose only the modules you enable.** The engine core is the two nouns plus the hard concerns (store, routing/outbox, sync, coverage, identity, diagnostics, capability seams). Everything protocol-specific and non-primitive — reactions, reposts, follow packs, highlights, long-form, lists — is **opt-in**. A minimal timeline app that never reacts links **zero** reaction code, and adding follow-pack support taxes only the apps that enable it.

- **Rust (BUILT today):** this is a Cargo-feature / separate-crate boundary. Enable a protocol crate/feature → its recipes and kind handling compile in; leave it out → it's absent from the binary. The dead-code you never enabled is never linked.
- **Swift/Kotlin/web (PLANNED module mechanism):** the *intended* shape is that each per-NIP module is a separable package/target you add explicitly. Enabling it is how its recipes (`.reactions(to:)`, `.react()`) and kinds appear; not enabling it keeps them out of your app's binary. See *Extending NMP: protocol modules & recipes* for the design preview and *The two nouns and the ownership table* for the principle in full.

The practical upshot: your binary size is roughly *core + exactly the protocol modules you chose*, and it doesn't grow because some *other* app needed highlights or long-form. The expensive, permanent thing — the core and the two nouns — stays small on every platform; the protocol surface is à la carte. That's the same win the old NMP genuinely had (an app that didn't care about reactions didn't pack them), now stated as a durable packaging rule rather than an accident of crate layout.

---

<!-- nav-footer -->
<sub>← [Adding NMP to an app you own](07-brownfield.md) · [Index](README.md) · [Live queries & the binding grammar](09-binding-grammar.md) →</sub>
