# Platforms

## Direct Rust

Depend on the `nmp` crate and construct `Engine::new(EngineConfig)`. The consumer-facing methods are:

```text
reset_persistent_store
new
observe
publish
publish_tracked
reattach_receipt
add_account
add_signer
remove_signer
set_active_account
active_account
observe_diagnostics
shutdown
```

`from_parts` is hidden behind `unstable-mechanism` for in-repo tests and is not an application assembly path. `cargo test -p nmp-consumer-check` is the focused supported-facade proof. Test any other touched Rust crate with `cargo test -p <crate>`; `cargo test --workspace` is the merge gate.

## Swift

Import `NMP`, not `NMPFFI`. `NMPEngine` exposes persistent reset; construction; account add/activate/read/clear-persisted; filter/demand observation; diagnostics; publish; composed publish; receipt reattachment; NIP-29 composition; NIP-46 helpers; and shutdown. Optional products are `NMPContent` and `NMPUI`.

From a clean clone, generate the ignored FFI artifacts from the repo root, then run SwiftPM in its package directory:

```sh
scripts/build-swift-xcframework.sh --sim-only
cd Packages/NMP
swift test
```

Drop `--sim-only` when a physical-device slice is required. Rebuild the xcframework after a UniFFI surface change.

Swift `NMPConfig` has `storePath`, `indexerRelays`, `appRelays`, and `fallbackRelays`; it does not expose Rust's `allowed_local_relay_hosts` or `max_relays`.

## Kotlin/JVM

Import `com.nmp.sdk.*`, not `uniffi.nmp_ffi`. `NMPEngine` implements `AutoCloseable`; prefer `use {}`. Its public methods cover persistent reset; account add/activate/read/clear-persisted; filter/demand observation; diagnostics; publish; NIP-29 composition and composed publish; receipt reattachment; NIP-46 helpers; shutdown/close.

From a clean clone:

```sh
scripts/build-kotlin-jvm.sh
cd Packages/NMPKotlin
./gradlew test
```

Rebuild generated bindings after a UniFFI surface change. This module targets desktop JVM. It does not ship an Android AAR, Compose UI, or Android-owned `Intent`/package-manager calls.

Kotlin `NMPConfig` mirrors Swift's four fields and omits `allowed_local_relay_hosts` and `max_relays`. Its flows are cold; one collection equals one engine observation unless the app shares the flow.
