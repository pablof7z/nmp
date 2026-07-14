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
relay_information
shutdown
```

`from_parts` is hidden behind `unstable-mechanism` for in-repo tests and is not an application assembly path. `cargo test -p nmp-consumer-check` is the focused supported-facade proof. Test any other touched Rust crate with `cargo test -p <crate>`; `cargo test --workspace` is the merge gate.

`EngineConfig.max_native_tasks` is the finite zero-queue ceiling for engine-owned observer/action/signer tasks; its default is 12 and zero selects that finite default. A full executor returns `ExecutorSaturated { component, capacity }` before it accepts the associated stream or operation. An OS spawn refusal remains the distinct `ThreadUnavailable { component, reason }`.

`Engine::relay_information(relay, policy)` is an async one-shot returning `RelayInformationSnapshot` or `RelayInformationRequestError`. `UseCache` returns an unexpired last-good representation; `Refresh` requests a generation-guarded single flight. Inspect `RelayInformationRequestError::Acquisition` without collapsing `ExecutorSaturated`, `WaiterSaturated`, `ThreadUnavailable`, `ServiceClosed`, `Http`, `ResponseTooLarge`, or `InvalidDocument`. A stale-on-error success has `freshness: Stale` and `last_error`; `advertises_nip` is document evidence, not behavioral proof.

These failures have three distinct direct-Rust doors:

- `Engine::new` and ordinary `Engine::observe` can report `EngineError::ThreadUnavailable`; the ordinary Rust subscription itself does not consume a native-executor slot. `nmp_nip02::observe_following` also reserves an executor task, so it can return either `EngineError::ExecutorSaturated` or `EngineError::ThreadUnavailable`.
- `nmp_nip02::set_following` returns `FollowAction`, not `Result`. Read action-worker refusal through `FollowAction::recv` as `FollowActionStatus::Failed` with the matching `FollowActionFailure::ExecutorSaturated` or `FollowActionFailure::ThreadUnavailable` value.
- `Nip46Invitation::connect*` and `Nip46Signer::connect_bunker*` return `Result<_, Nip46Error>`; initial relay/session setup uses the matching `Nip46Error::ExecutorSaturated` or `Nip46Error::ThreadUnavailable` value.

These are typed operational failures, not interchangeable error cases, a hidden task queue, panics, or timeouts. The executor census and idle barrier re-exported by `nmp` are doc-hidden implementation/proof seams, not an application scheduler or telemetry contract.

## Swift

Import `NMP`, not `NMPFFI`. `NMPEngine` exposes persistent reset; construction; account add/activate/read/clear-persisted; filter/demand observation; diagnostics; async one-shot relay information; publish; composed publish; receipt reattachment; NIP-29 composition; NIP-46 helpers; and shutdown. Optional products are `NMPContent` and `NMPUI`.

From a clean clone, generate the ignored FFI artifacts from the repo root, then run SwiftPM in its package directory:

```sh
scripts/build-swift-xcframework.sh --sim-only
cd Packages/NMP
swift test
```

Drop `--sim-only` when a physical-device slice is required. Rebuild the xcframework after a UniFFI surface change.

Swift `NMPConfig` has `storePath`, `indexerRelays`, `appRelays`, `fallbackRelays`, and `maxNativeTasks` (default 12); it does not expose Rust's `allowed_local_relay_hosts` or `max_relays`.

Construction, observation, receipt attachment, and both `connectNip46` overloads throw. Construction can report OS spawn refusal; native observer/receipt/connection setup can report a full executor as `NMPError.executorSaturated(component:capacity:)` or OS spawn refusal as `NMPError.threadUnavailable(component:reason:)`. Swift following actions carry the corresponding `NMPFollowActionFailure` terminal case. Derive/cache the signer handoff URI before invitation connection consumes the invitation; then connect, establish the listener, and launch the cached handoff. Invitation connection reserves capacity before consuming the invitation, but an admitted outer bridge can still consume it before an OS spawn refusal. After `connectNip46` returns a handle, an inner worker failure arrives as `.failed(reason:)` and stream completion. Do not turn any immediate failure shape into a readiness timeout. `NMPEngine`'s census and idle-barrier methods are internal lifecycle falsifiers, not public app diagnostics.

`relayInformation(for:policy:)` suspends and throws. Its engine-flight admission maps to `NMPError.executorSaturated`, its per-relay waiter refusal maps to `NMPError.relayInformationWaitersSaturated`, and OS spawn refusal maps to `NMPError.threadUnavailable`; HTTP, document, size, and closed-service failures map to `NMPError.relayInformationUnavailable`. Treat `RelayInformation.rawJSON` as forward-compatible authority and `lastError` as stale-on-error evidence.

## Kotlin/JVM

Import `com.nmp.sdk.*`, not `uniffi.nmp_ffi`. `NMPEngine` implements `AutoCloseable`; prefer `use {}`. Its public methods cover persistent reset; account add/activate/read/clear-persisted; filter/demand observation; diagnostics; suspending one-shot relay information; publish; NIP-29 composition and composed publish; receipt reattachment; NIP-46 helpers; shutdown/close.

From a clean clone:

```sh
scripts/build-kotlin-jvm.sh
cd Packages/NMPKotlin
./gradlew test
```

Rebuild generated bindings after a UniFFI surface change. This module targets desktop JVM. It does not ship an Android AAR, Compose UI, or Android-owned `Intent`/package-manager calls.

Kotlin `NMPConfig` mirrors Swift's five fields, including `maxNativeTasks` (default 12), and omits `allowed_local_relay_hosts` and `max_relays`. Its flows are cold; one collection equals one engine observation unless the app shares the flow.

Kotlin has no checked-exception syntax, but the wrapper maps a full executor to `NMPError.ExecutorSaturated(component, capacity)` and OS spawn refusal to `NMPError.ThreadUnavailable(component, reason)` from synchronous construction, collection setup, receipt bridges, and NIP-46 connection helpers. Call `invitation.androidHandoff(signer)` before `connectNip46(invitation)` consumes the invitation; cache that value, connect/start state collection, then launch it explicitly. Invitation connection reserves capacity before consuming the invitation, but an admitted outer bridge can still consume it before an OS spawn refusal. Once a NIP-46 handle exists, an inner worker failure arrives as `NMPNip46ConnectionState.Failed(reason)` then `Closed`. Preserve the distinction at the lifecycle owner and do not retry by creating unbounded collectors. Census and idle-barrier methods remain internal lifecycle falsifiers.

The suspending `relayInformation(relay, policy)` call maps engine-flight, per-relay waiter, and OS spawn admission to `NMPError.ExecutorSaturated`, `NMPError.RelayInformationWaitersSaturated`, and `NMPError.ThreadUnavailable`. Remaining acquisition failures are `NMPError.RelayInformationUnavailable`. Preserve `RelayInformation.rawJson`, freshness, and separate `lastError`; do not turn this one-shot into an unbounded polling flow.

## Raw UniFFI

Raw UniFFI uses `NmpEngineConfig`, `NmpEngine`, observer callbacks, and `FfiReceiptReattachment`; Rust's distinct `FfiError::ExecutorSaturated`, `FfiError::RelayInformationWaitersSaturated`, and `FfiError::ThreadUnavailable` become generated Swift/Kotlin exception cases. The raw projection includes async `relayInformation`, `allowedLocalRelayHosts`/`maxRelays`/`maxNativeTasks` configuration, `FfiNativeTaskCensus` plus an event-driven idle barrier for lifecycle proof, and the private-rejection/over-cap/store-degraded diagnostic fields omitted by the ergonomic native wrappers. Treat this as parity authority for wrapper maintainers, not an alternate app API; Swift apps import `NMP`, and Kotlin apps import `com.nmp.sdk`.
