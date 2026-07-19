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
sign_event
set_active_account
active_account
observe_diagnostics
relay_information
shutdown
```

`from_parts` is hidden behind `unstable-mechanism` for in-repo tests and is not an application assembly path. `cargo test -p nmp-consumer-check` is the focused supported-facade proof. Test any other touched Rust crate with `cargo test -p <crate>`; `cargo test --workspace` is the merge gate.

`EngineConfig` has no worker/task capacity field: #704 removed all application-configurable task admission. Observer/action/signer work and NIP-46 sessions run as async tasks on one shared engine-owned runtime, so there is no admission ceiling and no capacity refusal for ordinary operations. The only construction-time infrastructure failure is `EngineError::EngineStartFailed { component, reason }`, returned when the engine itself cannot be built (the OS refused an engine-owned thread, or the relay budget was unrepresentable); it is never raised by an ordinary operation once the engine exists.

`Engine::sign_event(SignEventRequest)` freezes the active author and returns a cancellable `SignEventOperation`; `recv` yields one fully verified event or a typed `SignEventError`. It never accepts or publishes a write. Asynchronous `SigningCapability` implementations create pending work with `SignerOp::pending_channel` or `pending_channel_with_cancel` and resolve the returned opaque `PendingSignerSender`; consumers do not receive or decompose NMP's internal channel.

`Engine::relay_information(relay, policy)` is an async one-shot returning `RelayInformationSnapshot` or `RelayInformationRequestError`. `UseCache` returns an unexpired last-good representation; `Refresh` requests a generation-guarded single flight. Inspect `RelayInformationRequestError::Acquisition` without collapsing `ServiceClosed`, `Http`, `ResponseTooLarge`, or `InvalidDocument`. A stale-on-error success has `freshness: Stale` and `last_error`; `advertises_nip` is document evidence, not behavioral proof.

These infrastructure failures have distinct direct-Rust doors:

- `Engine::new` reports `EngineError::EngineStartFailed` when the engine itself cannot be constructed; no ordinary operation raises it.
- Ordinary `Engine::observe` and `nmp_nip02::observe_following` report `EngineError::ObservationUnavailable` only when a live observation cannot open its required relay connection (or canonical projection). No OS thread is consumed per observation, and there is no task admission ceiling.
- `nmp_nip02::set_following` returns `FollowAction`, not `Result`. It has no capacity or thread refusal; a genuine terminal failure surfaces through `FollowAction::recv` as `FollowActionStatus::Failed` with a `FollowActionFailure` variant.
- `Nip46Invitation::connect*` and `Nip46Signer::connect_bunker*` return `Result<_, Nip46Error>` for genuine relay/session setup failures; `Nip46Error` no longer has any capacity or thread-unavailable variant.

These are typed operational failures, not interchangeable error cases, a hidden task queue, panics, or timeouts. Every observer/action/signer path and NIP-46 session runs as an async task on the shared engine runtime, so ordinary concurrent operations simply make progress.

## Swift

Import `NMP`, not `NMPFFI`. `NMPEngine` exposes persistent reset; construction; account add/activate/read/clear-persisted; filter/demand observation; diagnostics; async governed sign-only; async one-shot relay information; publish; composed publish; receipt reattachment; NIP-29 composition; NIP-46 helpers; and shutdown. Optional products are `NMPContent` and `NMPUI`.

From a clean clone, generate the ignored FFI artifacts from the repo root, then run SwiftPM in its package directory:

```sh
scripts/build-swift-xcframework.sh --sim-only
cd Packages/NMP
swift test
```

Drop `--sim-only` when a physical-device slice is required. Rebuild the xcframework after a UniFFI surface change.

`swift test` above executes on the macOS host. The build script compiles the iOS Simulator slices, but the package currently has no simulator runtime test target; issue #465 tracks that missing qualification harness.

Swift `NMPConfig` has `storePath`, `indexerRelays`, `appRelays`, and `fallbackRelays`; it exposes no worker/task capacity field and does not expose Rust's `allowed_local_relay_hosts` or `max_relays`.

Construction, observation, receipt attachment, and both `connectNip46` overloads throw. Construction can report `NMPError.engineStartFailed(component:reason:)` when the engine itself cannot be built; a live observation that cannot open its relay connection reports `NMPError.observationUnavailable(reason:)`. Neither carries any worker/pool/thread concept, and no ordinary operation is refused for capacity. Swift following actions carry the corresponding `NMPFollowActionFailure` terminal case for genuine failures. Derive/cache the signer handoff URI before invitation connection consumes the invitation; then connect, establish the listener, and launch the cached handoff. After `connectNip46` returns a handle, an inner worker failure arrives as `.failed(NMPNip46Failure)` (a typed enum mirroring `Nip46Error`, #494) and stream completion. Do not turn any immediate failure shape into a readiness timeout.

`relayInformation(for:policy:)` suspends and throws. It has no capacity or thread refusal; credentialed URL, HTTP, document, size, and closed-service failures map to `NMPError.relayInformationUnavailable(RelayInformationErrorKind)` -- a typed kind, not a message string (#494). Treat `RelayInformation.rawJSON` as forward-compatible authority and `lastError: RelayInformationErrorKind?` as stale-on-error evidence.

`signEvent(NMPUnsignedEvent)` is `async throws`. Task cancellation cancels the exact in-flight sign-only operation; completion and cancellation share one terminal state. The returned `NMPSignedEvent` is verified but carries no storage, receipt, routing, or publication claim.

## Kotlin/JVM

Import `com.nmp.sdk.*`, not `uniffi.nmp_ffi`. `NMPEngine` implements `AutoCloseable`; prefer `use {}`. Its public methods cover persistent reset; account add/activate/read/clear-persisted; filter/demand observation; diagnostics; suspending governed sign-only; suspending one-shot relay information; publish; NIP-29 composition and composed publish; receipt reattachment; NIP-46 helpers; shutdown/close.

From a clean clone:

```sh
scripts/build-kotlin-jvm.sh
cd Packages/NMPKotlin
./gradlew test
```

Rebuild generated bindings after a UniFFI surface change. This module targets desktop JVM. It does not ship an Android AAR, Compose UI, or Android-owned `Intent`/package-manager calls.

Kotlin `NMPConfig` mirrors Swift's four fields, exposes no worker/task capacity field, and omits `allowed_local_relay_hosts` and `max_relays`. Its flows are cold; one collection equals one engine observation unless the app shares the flow.

Kotlin has no checked-exception syntax, but the wrapper maps engine construction failure to `NMPError.EngineStartFailed(component, reason)` and a live observation that cannot open its relay connection to `NMPError.ObservationUnavailable(reason)`; there is no capacity or thread refusal on ordinary operations. Call `invitation.androidHandoff(signer)` before `connectNip46(invitation)` consumes the invitation; cache that value, connect/start state collection, then launch it explicitly. Once a NIP-46 handle exists, an inner worker failure arrives as `NMPNip46ConnectionState.Failed(NMPNip46Failure)` (a typed sealed interface mirroring `Nip46Error`, #494) then `Closed`. Preserve the distinction at the lifecycle owner and do not retry by creating unbounded collectors.

The suspending `relayInformation(relay, policy)` call has no capacity or thread refusal. Acquisition failures are `NMPError.RelayInformationUnavailable(kind: RelayInformationErrorKind)` -- a typed kind, not a message string (#494). Preserve `RelayInformation.rawJson`, freshness, and separate `lastError: RelayInformationErrorKind?`; do not turn this one-shot into an unbounded polling flow.

The suspending `signEvent(NMPUnsignedEvent)` call is cancellable and uses one terminal state across callback completion and coroutine cancellation. Its `NMPSignedEvent` is verified sign-only output, not evidence of storage or publication.

## Raw UniFFI

Raw UniFFI uses `NmpEngineConfig`, `NmpEngine`, observer callbacks, and `FfiReceiptReattachment`; Rust's distinct `FfiError::EngineStartFailed` (engine construction) and `FfiError::ObservationUnavailable` (a live observe that cannot open its relay connection) become generated Swift/Kotlin exception cases. The raw projection includes cancellable sign-only observation, async `relayInformation`, `allowedLocalRelayHosts`/`maxRelays` configuration, and the private-rejection/over-cap/store-degraded diagnostic fields omitted by the ergonomic native wrappers. It exposes no worker/task capacity, census, or idle-barrier method: ordinary operations run as async tasks on the shared engine runtime and simply make progress. Treat this as parity authority for wrapper maintainers, not an alternate app API; Swift apps import `NMP`, and Kotlin apps import `com.nmp.sdk`.
