# Platforms

## Direct Rust

Depend on the `nmp` crate and construct `Engine::new(EngineConfig)`. The consumer-facing methods are:

```text
reset_persistent_store
new
new_with_session
observe
publish
cancel
publish_queue
publish_queue_for_event
remove_publish_queue_entry
reattach_receipt
receipt_result
reattach_by_correlation
session
export_session
add_private_key_account
add_public_key_account
make_current_account
remove_account
clear_session
sign_event
add_auth_policy
remove_auth_policy
observe_diagnostics
relay_information
shutdown
```

`from_parts` is hidden behind `unstable-mechanism` for in-repo tests and is not an application assembly path. `cargo test -p nmp-consumer-check` is the focused supported-facade proof. Test any other touched Rust crate with `cargo test -p <crate>`; `cargo test --workspace` is the merge gate.

`EngineConfig`'s fields are `store_path`, `indexer_relays` (the exact operator sources handed to the optional NIP-65 coordinator; generic routing never reads or adds them), `app_relays`, `fallback_relays`, `max_relays`, `max_auth_capabilities`, and `max_publish_attempts` (how many failed attempts at ONE relay terminalise that lane as `RelayState::GaveUp`, default 16 — it counts observations, never wall-clock, so offline and AUTH-parked time spends nothing, and a write with no resolved route or no attached signer has no ceiling at all). There is no worker/task capacity field: #704 removed application-configurable task admission and all saturation outcomes. Observer/action/signer work runs as async tasks on one shared engine-owned runtime; private physical bounds backpressure rather than refusing ordinary operations. `EngineError::EngineStartFailed { component, reason }` is returned when the engine itself cannot be built (the OS refused an engine-owned thread, or the relay budget was unrepresentable) and is never raised by an ordinary operation once the engine exists — but it is not the only construction failure: `Engine::new` also reports `StoreOpenFailed`, `StoreAlreadyOpen`, `StoreUnsupportedSchema`, and `InvalidRelayUrl`. `AuthCapabilityRegistryFull { limit }` is a real capacity refusal, bounded by the app-set `max_auth_capabilities`; what does not exist is a worker/task ceiling.

`Engine::sign_event(SignEventRequest)` freezes the current session author and returns a cancellable `SignEventOperation`; `recv` yields one fully verified event or a typed `SignEventError`. It never accepts or publishes a write. The production session surface currently installs local-key providers; provider families may implement asynchronous capability work internally without exposing NMP's channels to consumers.

`Engine::relay_information(relay, policy)` is an async one-shot returning `RelayInformationSnapshot` or `RelayInformationRequestError`. `UseCache` returns an unexpired last-good representation; `Refresh` requests a generation-guarded single flight. Inspect `RelayInformationRequestError::Acquisition` without collapsing `ServiceClosed`, `CredentialedRelayUrl`, `Http`, `ResponseTooLarge`, or `InvalidDocument`. A stale-on-error success has `freshness: Stale` and `last_error`; `advertises_nip` is document evidence, not behavioral proof.

These infrastructure failures have distinct direct-Rust doors:

- `Engine::new` reports `EngineError::EngineStartFailed` when the engine itself cannot be constructed; no ordinary operation raises it. Store and relay-URL problems are their own variants.
- An ordinary or windowed `Engine::observe` reports `EngineError::ObservationUnavailable` only when store degradation prevents its initial canonical projection from opening. Relay connection/worker failure remains acquisition evidence. Window and `LiveQuery` validation refuse through their own variants. No OS thread is consumed per observation, and there is no worker/task-capacity refusal.
- `nmp_nip02::set_following` returns `Result<ReceiptStream, FollowActionFailure>`. Success is the ordinary durable receipt stream; signed-out, closed-engine, and pre-custody receipt failures are returned directly. It has no separate acquisition worker, retry lifecycle, capacity refusal, or thread refusal.

These are typed operational failures, not interchangeable error cases, a hidden task queue, panics, or timeouts. Every observer/action/signer path runs as an async task on the shared engine runtime, so ordinary concurrent operations simply make progress.

## Swift

Import `NMP`, not `NMPFFI`. `NMPEngine`'s always-present surface is persistent reset; construction; account generate/add/activate/read/remove/detach-persisted; auth-policy add/remove; filter/demand/live-query observation, windowed and unwindowed; diagnostics; async sign-only; async one-shot relay information; publish; write cancellation; publish-queue enumeration by cursor and by event id; queue-entry removal; receipt reattachment by id and by correlation token; and shutdown.

Everything protocol-shaped is compile-time selected, not always there — see "Selecting the surface" below. When their capabilities are selected: NIP-02 projects `observeFollowing`/`follow`/`unfollow` on `NMPEngine` plus the Combine `NMPFollowing` `ObservableObject`; NIP-22 comment composition is the top-level `commentIntent(...) -> WriteIntent`, not an engine method, and publishes through ordinary `NMPEngine.publish`; NIP-29's full read-and-write door is `NMPRelayScope`/`NMPGroup`/`NMPGroups`/`NMPGroupPredicate`/`NMPGroupIds` (#1033, #1252), where `NMPRelayScope.on(hosts)` names the relays once, `group(_:)` narrows to one group, `read(_:)` takes an app-supplied selection and returns an `NMPLiveQuery` for the ordinary observe door, `observeRecords(engine:matching:records:limit:)` is the `AsyncSequence` of `NMPGroupSnapshot`s for the relay-signed records, and the group's write methods return the ordinary receipt stream; the same `nip29` capability also projects NIP-51 simple-groups lists; NIP-18 `repost(_:)`, NIP-25 `react(to:with:)`, and NIP-C7 `chat()`/`chatReply(to:)` are top-level composers returning `WritePayload` alongside the generic tagging door; Blossom and verified assets are projected too.

`publishQueue(afterReceiptID:limit:)` is a bounded page, not a whole-queue call: `limit` is a required `UInt8` and `afterReceiptID` is the exclusive cursor. `publishQueue(forEventID:afterReceiptID:limit:)` is the separate join from one rendered row's event id to the receipts that still own it — more than one receipt can own identical bytes, so it too is paged rather than choosing one.

### Selecting the surface

An application does not consume `Packages/NMP` directly. It commits one `.nmp.toml` naming compile-time capabilities and products, then prepares an exact local Swift package with the first-class `nmp` CLI (installed once with `cargo install --locked --path crates/nmp-cli`):

```sh
nmp init --product apple --capability groups --capability "outbox routing"
nmp prepare --output Generated/NMP
nmp verify --output Generated/NMP
```

Add `Generated/NMP/apple` as a local Swift package dependency. Capability keys are `asset`, `blossom`, `content`, `nip02`, `nip18`, `nip22`, `nip25`, `nip29`, `nip65`, and `nipc7`; `nmp capability list/add/remove` edits the declaration. NIP-51 simple-groups lists are part of `nip29`, not a separate capability. Only the Swift sources Cargo resolves are materialized, so a name from an unselected capability is absent at compile time rather than failing at runtime. `nmp-native-provenance.json` identifies the exact selected content, and `nmp verify` refuses a prepared product whose library, wrappers, or inventory drifted.

The prepared Apple product has two targets: `NMP` and `NMPContent` (parser only, present with the `content` capability). `NMPUI` is not in that catalog — the SwiftUI family lives in this repository's own qualification package and is not something `nmp prepare` can hand an app today.

Rebuilding the repository's own complete-surface qualification package is separate machinery, not the app workflow. From a clean clone, generate the ignored FFI artifacts from the repo root, then run SwiftPM in its package directory:

```sh
scripts/build-swift-xcframework.sh --macos-only
cd Packages/NMP
swift test
```

`--macos-only` builds just the host slice `swift build`/`swift test` need. `--sim-only` adds the iOS-simulator slices; no flag at all also builds the `aarch64-apple-ios` device slice, which needs a signing identity to run on hardware. Rebuild after any change to `nmp-ffi`'s UniFFI surface.

`swift test` above executes on the macOS host. The macOS qualification workflow also runs the public-hostname NIP-11 falsifier through the supported Swift facade on an actual iOS Simulator. Physical-device qualification remains separate.

Swift `NMPConfig` has `storePath`, `appRelays`, `fallbackRelays`, `maxRelays` (default 10), and `maxAuthCapabilities` (default 64), plus `outboxRouting: OutboxRoutingConfig?` when the `nip65` capability is selected — `nil` constructs an explicit-routing-only engine, and a configured value must name at least one app-owned indexer or construction throws. It exposes no worker/task capacity field. The only Rust `EngineConfig` field with no Swift counterpart is `max_publish_attempts`; Rust's `indexer_relays` is what `outboxRouting.indexers` carries.

Construction, observation, and receipt attachment throw. Construction can report `NMPError.engineStartFailed(component:reason:)` when the engine itself cannot be built; `NMPError.observationUnavailable(reason:)` means only that store degradation prevented an ordinary or windowed observation's initial canonical projection from opening. Relay connection/worker failure remains acquisition evidence, and no operation is refused for worker/task capacity. Swift following actions carry the corresponding `NMPFollowActionFailure` terminal case for genuine failures. Do not turn any immediate failure shape into a readiness timeout.

`relayInformation(for:policy:)` suspends and throws. It has no capacity or thread refusal; credentialed URL, HTTP, document, size, and closed-service failures map to `NMPError.relayInformationUnavailable(RelayInformationErrorKind)` -- a typed kind, not a message string (#494). Treat `RelayInformation.rawJSON` as forward-compatible authority and `lastError: RelayInformationErrorKind?` as stale-on-error evidence.

`signEvent(NMPUnsignedEvent)` is `async throws`. Task cancellation cancels the exact in-flight sign-only operation; completion and cancellation share one terminal state. The returned `NMPSignedEvent` is verified but carries no storage, receipt, routing, or publication claim.

## Kotlin/JVM

Import `com.nmp.sdk.*`, not `uniffi.nmp_ffi`. `NMPEngine` implements `AutoCloseable`; prefer `use {}`. Its always-present methods cover persistent reset; account generate/add/activate/read/remove/detach-persisted; auth-policy add/remove; filter/demand/live-query observation, windowed and unwindowed; diagnostics; suspending sign-only; suspending one-shot relay information; publish; write cancellation; `publishQueue(afterReceiptId, limit)` and `publishQueueForEvent(...)` enumeration plus `removePublishQueueEntry`; receipt reattachment by id and by correlation token; shutdown/close.

The protocol families are compile-time selected exactly as on Swift (see "Selecting the surface" below). When selected: NIP-02 projects `observeFollowing`/`follow`/`unfollow` on `NMPEngine`; NIP-22 comment composition is the top-level `commentIntent(...) -> WriteIntent`, published through ordinary `NMPEngine.publish`; NIP-29's full read-and-write door is `NMPRelayScope`/`NMPGroup`/`NMPGroups`/`NMPGroupPredicate`/`NMPGroupIds` (#1033, #1252), where `NMPRelayScope.on(hosts)` names the relays once, `group(...)` narrows to one group, `read(...)` takes an app-supplied selection and returns an `NMPLiveQuery` for the ordinary observe door, `observeRecords(engine, predicate, records, limit)` is the `Flow` of `NMPGroupSnapshot`s for the relay-signed records, and the group's write methods return the ordinary receipt stream; the same `nip29` capability also projects NIP-51 simple-groups lists; NIP-18 `repost(target)`, NIP-25 `react(target, reaction)`, and NIP-C7 `chat()`/`chatReply(target)` are top-level composers returning `WritePayload`; Blossom and verified assets are projected too.

### Selecting the surface

The same committed `.nmp.toml` and `nmp` CLI drive Kotlin. `--product kotlin-jvm` prepares a desktop-JVM module; `--product android` prepares an Android library:

```sh
nmp init --product kotlin-jvm --capability groups
nmp prepare --output Generated/NMP
```

Consume the JVM output as a Gradle composite build — `includeBuild("Generated/NMP/kotlin-jvm")` and `implementation("com.nmp:nmp-kotlin:0.0.0")`. The local coordinate is deterministic and is not a published Maven version.

The Android product is real: `nmp prepare --product android` materializes the same feature-selected `com.nmp.sdk` sources, generated UniFFI binding, and `libnmp_ffi.so` slices into a release AAR plus a local Maven repository. It is API 26+, compile SDK 35, NDK 27.2.12479018, with exactly `arm64-v8a` and `x86_64`. Applications import only `com.nmp.sdk`; `uniffi.nmp_ffi` is implementation plumbing. The catalog-pinned side-by-side NDK is authoritative — `NMP_ANDROID_NDK_HOME` may name that exact revision, and a runner's generic `ANDROID_NDK_HOME` cannot silently select another. `.github/workflows/android-emulator.yml` qualifies the generated artifact as an external API-35 application. There is still no NIP-55 Android intent-based signing.

Rebuilding this repository's own desktop-JVM qualification project is separate machinery, not the app workflow:

```sh
scripts/build-kotlin-jvm.sh
cd Packages/NMPKotlin
./gradlew test
```

Rebuild generated bindings after a UniFFI surface change. That project targets desktop JVM only and is not an Android runtime qualification; a narrow desktop-JVM Compose library is its separate `:ui` child project (`com.nmp.ui`, relay identity/list primitives from #198, exercised by `./gradlew :ui:test`) so Compose never becomes a dependency of the core SDK. Like Swift's `NMPUI`, `com.nmp.ui` is not in the prepared-product catalog.

Kotlin `NMPConfig` mirrors Swift's exactly — `storePath`, `appRelays`, `fallbackRelays`, `maxRelays` (default 10), `maxAuthCapabilities` (default 64), and `outboxRouting: OutboxRoutingConfig?` under the `nip65` capability — and exposes no worker/task capacity field. Unwindowed observation returns a cold flow, so one collection equals one engine observation unless the app shares it; windowed observation instead returns an `NMPQuery` whose `frames` flow can be collected only once.

Kotlin has no checked-exception syntax, but the wrapper maps engine construction failure to `NMPError.EngineStartFailed(component, reason)` and ordinary or windowed initial canonical-projection setup failure to `NMPError.ObservationUnavailable(reason)`. Relay connection/worker failure remains acquisition evidence and there is no worker/task refusal on ordinary operations.

The suspending `relayInformation(relay, policy)` call has no capacity or thread refusal. Acquisition failures are `NMPError.RelayInformationUnavailable(kind: RelayInformationErrorKind)` -- a typed kind, not a message string (#494). Preserve `RelayInformation.rawJson`, freshness, and separate `lastError: RelayInformationErrorKind?`; do not turn this one-shot into an unbounded polling flow.

The suspending `signEvent(NMPUnsignedEvent)` call is cancellable and uses one terminal state across callback completion and coroutine cancellation. Its `NMPSignedEvent` is verified sign-only output, not evidence of storage or publication.

## Raw UniFFI

Raw UniFFI uses `NmpEngineConfig`, `NmpEngine`, single-consumer pull handles, and `FfiReceiptReattachment`. The read path is not observer callbacks: `observe` returns an `NmpRowStream` driven by a `begin_next`/`receive`/`commit`/`abort` cycle, which is exactly why `FfiError::ConcurrentNext` exists; diagnostics and receipts pull the same way. Rust's distinct `FfiError::EngineStartFailed` (engine construction) and `FfiError::ObservationUnavailable` (ordinary or windowed initial canonical-projection setup) become generated Swift/Kotlin exception cases. The raw projection includes cancellable sign-only observation, async `relayInformation`, engine-free NIP-22 `commentIntent(...) -> FfiWriteIntent`, the full read-and-write NIP-29 `FfiRelayScope`/`FfiGroup`/`FfiGroups`/`FfiGroupPredicate`/`FfiGroupIds` door (#1033, #1252), write cancellation, publish-queue enumeration by cursor and by event id, entry removal, correlation-token reattachment, and the `sessions_rejected_over_cap` diagnostic field omitted by the ergonomic native wrappers. `store_degraded` and `sessions_refused_by_subscription_budget` are not in the raw projection either — they are reachable only from direct Rust, and `maxRelays` is on both hand-written configs as well. `NmpEngine` itself has no NIP-22 or NIP-29 composer/publish method — those are minted by the free `comment_intent`/`nip29` doors, not the engine. The raw engine exposes no worker/task capacity, census, idle-barrier method, or worker saturation refusal; private physical bounds backpressure internally. Treat this as parity authority for wrapper maintainers, not an alternate app API; Swift apps import `NMP`, and Kotlin apps import `com.nmp.sdk`.
