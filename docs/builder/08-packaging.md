# Packaging and distribution

An application should consume one supported NMP product for its platform. The
mechanism crates, generated FFI layer, and native binary are implementation
details of that product, not alternate ways to assemble an engine.

The exact package coordinates remain provisional. See
[Current implementation status](03-status-map.md) before integrating a shipping
build.

## One facade on every platform

Every supported projection must preserve the same behavioral contract:

```text
canonical Rust facade
  -> Swift value and AsyncSequence projection
  -> Kotlin value and Flow projection
  -> direct Rust API
```

Swift and Kotlin wrappers may use native naming, error, cancellation, and secure
storage conventions. They must not define a second routing, receipt, query, or
identity model. Direct Rust consumers use the same facade projected over FFI;
they do not assemble the resolver, store, router, and transport crates by hand.

## Swift

The intended consumer experience is one Swift Package Manager product:

```swift
import NMP

let engine = try NMPEngine(configuration: configuration)
```

That product contains a matched native Rust binary, generated bindings, and a
small hand-written Swift layer. Applications import only the public `NMP`
module. Generated `Ffi` records and callback protocols do not enter app code.

The package must support device and simulator builds and expose observations as
native asynchronous sequences and values. Secret-backed signer providers use
platform secure storage; the event/outbox database does not become a key vault.

## Kotlin and Android

The intended Android product is an AAR containing the native libraries for its
supported ABIs, generated Kotlin bindings, and the hand-written `Flow`
projection:

```kotlin
val engine = NMPEngine(configuration)
engine.observe(demand).collect { snapshot ->
    appState = appState.withSnapshot(snapshot)
}
```

The Compose app owns coroutine and UI scope. NMP owns demand lifetime beneath
the observation. Android secure signer providers belong behind Keystore-backed
capabilities, not in application event storage.

Desktop JVM proof does not by itself make the Android package complete. The AAR,
ABI matrix, cancellation, process restart, secure storage, and real-device
falsifier all belong to the Android acceptance gate.

## Rust

Rust applications depend on the canonical `nmp` facade crate. Mechanism crates
remain available to repository contributors and narrowly scoped advanced test
harnesses, but they are not a second supported product surface.

An explicitly unstable mechanism feature may expose construction seams while
the engine is developed. It must be clearly gated, type-complete for its stated
purpose, and excluded from the normal compatibility promise.

## Optional protocol modules

Core remains content-neutral. Protocol behavior is packaged as opt-in modules
that depend on the canonical facade:

```text
nmp core
nmp-nip29
nmp-nip68
nmp-nip17
...
```

Names are illustrative. The invariant is not.

A module owns only its exact schemas, reconstruction, validation, semantic
operations, and protocol authority. Enabling NIP-29 may add group operations and
group-host context. It must not pull a preferred timeline into core or own a
foreign NIP-68 photo schema merely because a group can publish one.

Platform distribution should preserve opt-in composition where practical. It
must not require app-side module registration, callbacks, or a second engine
container.

## Binary and binding versions move together

Generated bindings and their native core are one release artifact. A package
must not combine bindings generated from one facade revision with a different
binary.

The release pipeline therefore:

1. builds the Rust facade and native library;
2. generates bindings from that exact build;
3. runs Rust/Swift/Kotlin parity fixtures over the same contract;
4. packages the binary and ergonomic layer together; and
5. publishes one version with checksums for every binary artifact.

Public-shape changes require explicit review because they affect direct Rust,
FFI metadata, platform projections, persisted data, diagnostics, and examples.
That review is governance, not a promise that provisional v2 names cannot
change.

## Persistence belongs to the engine instance

One persistent store path has one live engine owner. The app chooses the path
and backup/reset policy; NMP owns the file format and atomic event/outbox
transactions. Applications must not open or mutate the database directly.

Cold construction may return cached rows before network acquisition completes.
That is a local replica with scoped evidence, never an authoritative global
snapshot.

## Web is not implied

A native Rust core does not automatically produce a supported browser product.
WebAssembly, browser persistence, background execution, socket behavior,
cryptographic capabilities, and package-size limits need their own thesis gate.
Until that work is explicitly accepted, this guide promises Swift, Kotlin, and
Rust shapes only.

---

<!-- nav-footer -->
<sub>[Index](README.md) · [Live queries](09-binding-grammar.md) →</sub>
