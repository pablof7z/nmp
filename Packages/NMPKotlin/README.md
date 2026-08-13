# NMPKotlin (Kotlin/JVM falsifier, #40)

The **minimal Kotlin/Flow falsifier** for #40 (parent epic #43) -- proves the
two-noun surface (`observe(filter): Flow<RowBatch>`, `publish(intent):
Flow<WriteStatus>`, `observeDiagnostics(): Flow<DiagnosticsSnapshot>`) ports
cleanly onto Kotlin's `Flow`, using the SAME canonical Rust facade
(`crates/nmp-ffi`) Swift already ships against. This is **not** the M6
Android app -- the root SDK has no Compose dependency. `nmp-native --platform
android` now materializes this same selected facade into an API-26 AAR with
the declared Android ABI libraries; governed emulator/runtime qualification
remains separate. The optional `:ui` child contains the narrow controlled
relay identity family from #198, using desktop-JVM Compose only; it is not an
Android runtime claim. Both checked desktop projects target
desktop JVM, the cheapest
environment that can prove or falsify the Flow mapping before the M5
human library-vs-framework verdict gates building the real app. See
`docs/builder/30-platform-guides.md`'s "Android / Kotlin" section for the
idiom this module now BUILDS (it was PLANNED-shape only until this PR).

`com.nmp.sdk` is the core package a consuming app imports:

```kotlin
import com.nmp.sdk.*

NMPEngine(NMPConfig(appRelays = listOf("wss://purplepag.es"))).use { nmp ->
    nmp.session.add(NMPPublicKey(decodedPublicKeyBytes), makeCurrent = true)
    val rows: Flow<RowBatch> = nmp.observe(followFeed)
    // caller applies stateIn(scope, WhileSubscribed()) for a hot, shared,
    // latest-value read -- this SDK never invents its own observer type.
}
```

See `src/main/kotlin/com/nmp/sdk/Engine.kt` for the full public surface.

NIP-22 comment composition is a protocol-owned top-level function returning
the ordinary write noun:

```kotlin
val intent =
    commentIntent(
        root = root,
        parent = parent,
        authorPubkey = author,
        createdAt = timestamp,
        content = text,
        correlation = correlation,
    )
val receipt = nmp.publish(intent)
```

It does not become an `NMPEngine` method, a `CommentIntent` wrapper, or a
second publication lifecycle.

Apps that opt into the separate `:ui` artifact may also import `com.nmp.ui`.
Its relay views accept caller-owned `NmpRelayInformationState`, query-scoped
`NmpRelayRuntimePresentation`, and an optional already-resolved Compose
`Painter`; they own no engine, HTTP, cache, polling, timer, or image loader.
See `docs/builder/36-relay-ui.md`.

An app stores one complete opaque session value rather than separately storing
each account and signing provider:

```kotlin
val payload = NMPEngine(config).use { nmp ->
    nmp.session.add(NMPPrivateKey(decodedPrivateKeyBytes), makeCurrent = true)
    nmp.session.add(NMPPublicKey(otherDecodedPublicKeyBytes))
    nmp.session.export()
}

NMPEngine(config, sessionPayload = payload).use { restored ->
    val current = restored.session.current
    val accounts = restored.session.accounts
}
```

The payload is sensitive and deliberately opaque: the app stores or passes its
bytes as a whole but never interprets provider restoration material. Durable,
transactional app-owned storage is a separate concern; clearing the session
removes its accounts and current selection without resetting NMP's event or
write store.

The same package also exposes pure content parsing and exact, engine-free
reference locators:

```kotlin
val document = parseNostrContent(rawContent)
val occurrence = document.references.first()

// Only a purpose-owning component/application scope that needs acquisition
// maps the exact variant to a query. This app deliberately treats a bare
// public key as a profile request; decoding itself does not.
val target = occurrence.target as NostrReferenceTarget.Pubkey
val demand = NMPDemand(
    selection = NMPFilter(
        kinds = listOf(0u),
        authors = NMPBinding.Literal(setOf(target.pubkey)),
        limit = 1u,
    ),
    source = NMPSourceAuthority.AuthorOutboxes,
)
val profile = nmp.observe(demand)
```

Parsing opens no query and requires no engine. A literal renderer can use the
authored occurrence and never construct a demand; a component that does
observe owns the ordinary `Flow` collection and its coroutine lifecycle.
Authored relay/author/kind hints remain data until a purpose owner explicitly
validates or promotes them. See `docs/builder/34-content.md`.

## Building from a clean clone

An application checks in one NMP feature manifest and prepares one exact local
Kotlin/JVM build from the repository root:

```sh
nmp --manifest path/to/app/.nmp.toml prepare \
  --output path/to/app/Generated/NMP
```

Consume it as a Gradle composite build with
`includeBuild("Generated/NMP/kotlin-jvm")` and
`implementation("com.nmp:nmp-kotlin:0.0.0")`. The local coordinate is stable;
`nmp-native-provenance.json` identifies the exact selected content. Re-run
prepare after changing the manifest, NMP source, or relevant host/toolchain
inputs. Ordinary Gradle incrementals use the generated project without running
Cargo. See [`native/README.md`](../../native/README.md).

This repository's complete-surface qualification project compiles two things it
does NOT commit (see
`.gitignore`): the uniffi-bindgen-generated Kotlin bindings
(`src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt`) and the compiled native
library (`src/main/resources/<jna-platform>/libnmp_ffi.{dylib,so}`) --
both are build output of the Rust `nmp-ffi` crate, same reasoning as the
Swift package's xcframework: committing a binary would make this SDK's
actual proof-of-correctness (that it's built from the source in this repo)
unverifiable.

That means maintainer `./gradlew build` / `./gradlew test` do **not** work straight
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

The fixed all-feature script is repository qualification machinery, not the app
feature-selection workflow.

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
