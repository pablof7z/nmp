# NMPKotlin (Kotlin/JVM falsifier, #40)

The **minimal Kotlin/Flow falsifier** for #40 (parent epic #43) -- proves the
two-noun surface (`observe(filter): Flow<RowBatch>`, `publish(intent):
Flow<WriteStatus>`, `observeDiagnostics(): Flow<DiagnosticsSnapshot>`) ports
cleanly onto Kotlin's `Flow`, using the SAME canonical Rust facade
(`crates/nmp-ffi`) Swift already ships against. This is **not** the M6
Android app -- the root SDK has no Compose dependency, Gradle Android plugin,
AAR, or cargo-ndk cross-compilation. The optional `:ui` child now contains the
narrow controlled relay identity family from #198, using desktop-JVM Compose
only; it is not an Android runtime or packaging claim. Both projects target
desktop JVM, the cheapest
environment that can prove or falsify the Flow mapping before the M5
human library-vs-framework verdict gates building the real app. See
`docs/builder/30-platform-guides.md`'s "Android / Kotlin" section for the
idiom this module now BUILDS (it was PLANNED-shape only until this PR).

`com.nmp.sdk` is the core package a consuming app imports:

```kotlin
import com.nmp.sdk.*

NMPEngine(NMPConfig(indexerRelays = listOf("wss://purplepag.es"))).use { nmp ->
    nmp.setActiveAccount(pubkey)
    val rows: Flow<RowBatch> = nmp.observe(followFeed)
    // caller applies stateIn(scope, WhileSubscribed()) for a hot, shared,
    // latest-value read -- this SDK never invents its own observer type.
}
```

See `src/main/kotlin/com/nmp/sdk/Engine.kt` for the full public surface.

The same package exposes NMP-owned add/remove actions for public `r` tags in
the active account's NIP-51 kind:10009 list:

```kotlin
nmp.addSimpleGroupRelay("wss://relay.example").status.collect { status ->
    // acquisition, no-op, exact-base conflict, and write receipt facts
}
val removal = nmp.removeSimpleGroupRelay("wss://relay.example")
```

These actions preserve remembered `group` entries and unrelated fields; the
JVM caller never constructs tags or chooses a replacement base.

Apps that opt into the separate `:ui` artifact may also import `com.nmp.ui`.
Its relay views accept caller-owned `NmpRelayInformationState`, query-scoped
`NmpRelayRuntimePresentation`, and an optional already-resolved Compose
`Painter`; they own no engine, HTTP, cache, polling, timer, or image loader.
See `docs/builder/36-relay-ui.md`.

Remote-signer discovery is already projected as pure JVM values so an Android
host can execute the OS-specific steps without moving policy out of Rust:

```kotlin
val primal = NMPLocalSignerDiscovery
    .installedAndroid(installedPackageIds)
    .single { it.id == "primal" }
val invitation = nmp.nip46Invitation(relays)
val handoff = invitation.androidHandoff(primal)
val connection = nmp.connectNip46(invitation) // listen before launch
startActivity(Intent(ACTION_VIEW, Uri.parse(handoff.uri)).setPackage(handoff.packageName))
// later: connection.close() // idempotent; emits Closed, then every collector completes
```

The Android app must declare package visibility for the packages/schemes it
queries. Launch acceptance is not connection readiness; collect
`connection.states` until `Ready`. This module remains desktop JVM, so the
`Intent`/`PackageManager` calls above belong to the consuming Android host.
Amber appears in discovery as NIP-55-only and is rejected by
`androidHandoff`; NIP-55 execution belongs to the future Android AAR.
`NMPNip46Connection` is `AutoCloseable`, and closing it detaches only its exact
session even if another connection has since replaced the same pubkey. Its
bounded multicast `Flow` replays lifecycle facts; UI and lifecycle collectors
cannot split `Ready`, `Failed`, or `Closed` between themselves. `Closed` is
terminal: no later callback is delivered and ordinary collection completes.

For explicit personal/development autologin without Keystore, the JVM SDK also
ships a deliberately plaintext file provider:

```kotlin
val accountStore = NMPInsecureFileAccountStore(appSupport.resolve("local-account.nsec"))
NMPEngine(config, accountStore).use { nmp ->
    val restoredPubkey = nmp.activeAccount()
}
```

With that provider configured, a successful `addAccount` is checkpointed and
the next engine construction restores and activates it. Sign-out calls
`clearPersistedAccount()` before closing the credential-owning engine. This is
not encrypted, Keystore-backed, or a secure production-vault claim.

The same package also exposes the optional content substrate:

```kotlin
val content = NMPContentClient(nmp).session(rawContent, viewModelScope)
val occurrence = content.snapshot.value.document.references.first()
val claim = content.claim(occurrence.id)
```

It parses through the shared Rust semantic document and collects only ordinary
NMP demand. `ContentSession.close()` and each `ContentClaim.close()` map
deterministically onto coroutine cancellation; no JVM `Cleaner` owns reference
withdrawal. See `docs/builder/34-content.md`.

## Building from a clean clone

`build.gradle.kts` compiles two things this module does NOT commit (see
`.gitignore`): the uniffi-bindgen-generated Kotlin bindings
(`src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt`) and the compiled native
library (`src/main/resources/<jna-platform>/libnmp_ffi.{dylib,so}`) --
both are build output of the Rust `nmp-ffi` crate, same reasoning as the
Swift package's xcframework: committing a binary would make this SDK's
actual proof-of-correctness (that it's built from the source in this repo)
unverifiable.

That means `./gradlew build` / `./gradlew test` do **not** work straight
after `git clone` until the artifacts exist once. Generate them from the
**repo root**:

```sh
scripts/build-kotlin-jvm.sh
```

This builds `nmp-ffi`'s `cdylib` slice for the host triple, runs
`uniffi-bindgen` in library mode to generate the Kotlin bindings, and
copies the native library into a JNA-resolvable classpath resource path
(`<os>-<arch>/`, computed from `uname` -- no `jna.library.path` system
property or other manual wiring needed). Takes well under a minute on a
warm `cargo` cache.

Once that's done, the ordinary commands work from this directory:

```sh
./gradlew build
./gradlew test
# or exercise only the optional Compose child after core artifacts exist
./gradlew :ui:test
```

Re-run `scripts/build-kotlin-jvm.sh` after any change to `nmp-ffi`'s public
UniFFI surface (new/changed exported types or methods) -- the generated
bindings and the compiled cdylib both need to stay in sync with the Rust
source, same discipline as the Swift xcframework.

CI proves this exact path from a clean checkout on every push/PR (see the
`kotlin-package` job in `.github/workflows/ci.yml`, mirroring the
`swift-package` job).

## Findings (#40's actual purpose -- discovering a bad shape is success)

- **The two nouns port cleanly.** `observe`/`publish`/`observeDiagnostics`
  all map onto `callbackFlow { ... }` with no structural mismatch --
  `RowObserver`/`ReceiptObserver`/`DiagnosticsObserver`'s callback shape is
  exactly what `callbackFlow` exists for.
- **Cold vs. eager subscription is a real, deliberate divergence from
  Swift.** `NMPQuery` (Swift) subscribes eagerly at construction (ARC
  refcounting starts immediately). `observe(filter)` here returns a COLD
  `Flow` -- the underlying `engine.observe()` FFI call happens lazily, on
  `collect()`, and each independent `collect()` opens its own engine-side
  subscription. This isn't a shortcut; it's what
  `docs/builder/30-platform-guides.md`'s pre-existing PLANNED-shape section
  already specified (`stateIn`/`WhileSubscribed` is the intended way to get
  a hot, shared, deduplicated read) -- Kotlin idiom and the pre-agreed
  design converge here.
- **Demand teardown needed a different mechanism than the generated
  wrapper's default, and it matters.** UniFFI's generated
  `NmpQueryHandle`/`NmpDiagnosticsHandle` register a `java.lang.ref.Cleaner`
  action as their only automatic teardown path -- but a JVM `Cleaner` only
  runs once GC actually collects the object, which is unbounded, not a
  substitute for #46's bounded-latest-state contract. This SDK does NOT
  rely on that Cleaner: `observeQuery`/`observeDiagnostics` call
  `handle.cancel()` from `callbackFlow`'s `awaitClose`, which fires
  deterministically the instant the collecting coroutine is cancelled or
  completes. Swift's ARC `deinit` and Kotlin's `awaitClose` both give
  prompt, deterministic teardown -- through genuinely different mechanisms
  (refcounting vs. structured-concurrency cancellation) -- but naively
  trusting the generated wrapper's `Cleaner` alone would NOT have.
- **No JVM `deinit` equivalent for the engine itself.** `NMPEngine` (Kotlin)
  implements `AutoCloseable` and forwards `close()` to `shutdown()`, so
  `NMPEngine(...).use { ... }` is the correct JVM idiom -- but unlike
  `NMPEngine.swift` (whose `deinit` calls `shutdown()` as a safety net),
  there is nothing here that closes the engine on scope-exit if a caller
  doesn't explicitly `.use { }` or call `.close()`/`.shutdown()`. This is
  the sharpest teardown finding of the falsifier: it's a real ergonomic gap
  relative to Swift, not a design choice, and any real Android app consuming
  this surface needs to bind `NMPEngine`'s lifetime to something explicit
  (a `ViewModel.onCleared()`, an `Application`-scoped singleton with a
  documented shutdown point, etc.) -- there is no automatic backstop.
- **`Flow`'s `conflate()` operator already IS the bounded-latest-state
  primitive** Swift had to hand-roll (`FrameCoalescer` + `AsyncStream(...,
  .bufferingNewest(1))`). No coalescer was written for this SDK; `conflate()`
  gets the same "never a growing backlog, always the latest" guarantee
  reactively instead of on Swift's fixed ~16ms timer.
