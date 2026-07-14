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

Thread refusal has three distinct direct-Rust doors:

- `Engine::new`, `Engine::observe`, and `nmp_nip02::observe_following` return `Result<_, EngineError>`; match `EngineError::ThreadUnavailable { component, reason }` for engine/query and NIP-02 observation setup.
- `nmp_nip02::set_following` returns `FollowAction`, not `Result`. Read action-worker refusal through `FollowAction::recv` as `FollowActionStatus::Failed(FollowActionFailure::ThreadUnavailable { component, reason })`.
- `Nip46Invitation::connect*` and `Nip46Signer::connect_bunker*` return `Result<_, Nip46Error>`; match `Nip46Error::ThreadUnavailable { component, reason }` when initial relay/session setup is refused.

These are typed operational failures, not interchangeable `EngineError` cases, panics, or timeouts.

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

Construction, observation, receipt attachment, and both `connectNip46` overloads throw. Handle synchronous outer-bridge refusal as `NMPError.threadUnavailable(component:reason:)`; Swift following actions can terminate with `NMPFollowActionFailure.threadUnavailable(component:reason:)`. After `connectNip46` returns a handle, an inner worker refusal arrives as `.failed(reason:)` and stream completion. Establish the listener before OS handoff, but do not turn either immediate failure shape into a readiness timeout.

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

Kotlin has no checked-exception syntax, but the wrapper maps synchronous raw spawn refusal to `NMPError.ThreadUnavailable(component, reason)` from construction, collection setup, receipt bridges, and NIP-46 connection helpers. Once a NIP-46 handle exists, an inner worker refusal arrives as `NMPNip46ConnectionState.Failed(reason)` then `Closed`. Preserve the distinction at the lifecycle owner and do not retry by creating unbounded collectors.

## Raw UniFFI

Raw UniFFI uses `NmpEngineConfig`, `NmpEngine`, observer callbacks, and `FfiReceiptReattachment`; Rust's `FfiError::ThreadUnavailable` becomes generated Swift/Kotlin exception cases. The raw projection includes `allowedLocalRelayHosts`/`maxRelays` configuration and the private-rejection/over-cap/store-degraded diagnostic fields omitted by the ergonomic native wrappers. Treat this as parity authority for wrapper maintainers, not an alternate app API; Swift apps import `NMP`, and Kotlin apps import `com.nmp.sdk`.
