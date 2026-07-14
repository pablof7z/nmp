# Current surface and gaps

Verified-Revision: `bc8fb9738679261cbb811f3ae274040785a8bbfe`

Verified on 2026-07-14. This pins the declared product/source authorities, not the skill package commit. Recheck the [source map](source-map.md) when any declared authority changes; the validator accepts newer skill-only commits when those sources have not drifted.

## Supported consumer tiers

| Tier | Supported entrypoint | Reactive result | Lifecycle |
| --- | --- | --- | --- |
| Rust | `nmp::Engine` | blocking `Subscription` / `DiagnosticsSubscription`; receipt channels | subscriptions withdraw on drop; call `shutdown` or drop engine |
| Swift | `NMPEngine` from `NMP` | `NMPQuery` and `NMPDiagnostics` `AsyncSequence`; `Receipt.status` | query/diagnostics cancellation and ARC teardown; engine `shutdown` with `deinit` backstop |
| Kotlin/JVM | `NMPEngine` from `com.nmp.sdk` | cold `Flow<RowBatch>`, `Flow<DiagnosticsSnapshot>`, `Receipt.status` | cancel collectors; use `NMPEngine.use {}` or call `close`/`shutdown` |

Mechanism crates and generated `NMPFFI`/`uniffi.nmp_ffi` bindings are implementation seams, not alternate application APIs.

## Capability matrix

| Capability | Rust facade | Swift wrapper | Kotlin/JVM wrapper |
| --- | --- | --- | --- |
| Observe `Filter`/default demand | yes | `observe(NMPFilter)` | `observe(NMPFilter)` |
| Explicit `Demand` source/cache | yes | `observe(NMPDemand)` | `observe(NMPDemand)` |
| Derived inner descriptor | full `Demand` | `NMPFilter` only | `NMPFilter` only |
| Query rows + scoped acquisition evidence | yes | yes | yes |
| Publish and stream receipt facts | `publish`, `publish_tracked` | `publish` returns `Receipt` | `publish` returns `Receipt` |
| Durable receipt reattachment | `reattach_receipt` | `reattachReceipt` | `reattachReceipt` |
| Persistent store reset | `reset_persistent_store` | `resetPersistentStore` | `resetPersistentStore` |
| Account add/activate/read | yes | yes | yes |
| Arbitrary signer registration | Rust only, with public `add_signer` | NIP-46 helpers, not arbitrary Rust capabilities | NIP-46 helpers, not arbitrary Rust capabilities |
| Governed sign-only event | `sign_event(SignEventRequest)` returns cancellable `SignEventOperation` | async throwing `signEvent(NMPUnsignedEvent)` | suspending `signEvent(NMPUnsignedEvent)` |
| Diagnostics stream | yes | yes, narrower snapshot | yes, narrower snapshot |
| One-shot NIP-11 relay information | async `relay_information(relay, policy)` | async throwing `relayInformation(for:policy:)` | suspending `relayInformation(relay, policy)` |
| Native task ceiling | `EngineConfig.max_native_tasks`, default 12 | `NMPConfig.maxNativeTasks`, default 12 | `NMPConfig.maxNativeTasks`, default 12 |
| Engine construction / ordinary query refusal | `EngineError::ThreadUnavailable`; ordinary `Engine::observe` uses no native-executor slot | native bridge: synchronous `NMPError.executorSaturated` or `.threadUnavailable` | collection-time `NMPError.ExecutorSaturated` or `.ThreadUnavailable` |
| NIP-02 observation refusal | `EngineError::ExecutorSaturated` or `EngineError::ThreadUnavailable` | synchronous matching `NMPError` | following observation not exposed |
| NIP-02 action-worker refusal | terminal `FollowActionStatus::Failed` with `ExecutorSaturated` or `ThreadUnavailable` | terminal `NMPFollowActionFailure.executorSaturated` or `.threadUnavailable` | following action not exposed |
| Initial NIP-46 connection refusal | matching `Nip46Error` from `Nip46Invitation::connect*` / `Nip46Signer::connect_bunker*` | synchronous matching outer `NMPError`; post-handle inner `.failed` | synchronous matching outer `NMPError`; post-handle inner `Failed` then `Closed` |
| NIP-02 following action | direct Rust protocol facade and Swift | yes | not on ergonomic Kotlin `NMPEngine` |
| NIP-29 composed group send | yes | yes | yes |
| Content parser/reference sessions | Rust crates, Swift `NMPContent`, Kotlin SDK | yes | yes |
| Ready-made UI | optional SwiftUI `NMPUI` | yes | no Compose package |

## Important gaps

- Public `Row` values expose the signed event and relay sources. They do not expose typed `Pending(intentId)`/`Signed(signature)` state, intent ids, or receipt ids. Pending-row mechanics exist internally and in the north-star contract; do not make an app depend on public metadata that is absent.
- Current `WriteStatus` cases are `Accepted`, `AwaitingCapability`, `Signed`, `Routed`, `Sent`, `Acked`, `Rejected`, `GaveUp`, `PersistenceBlocked`, `RoutePersistenceBlocked`, `OutcomeUnknown`, `ReplaceableConflict`, and `Failed` (platform naming differs). There is no public `AttemptStarted`, `RetryEligible`, `Cancelled`, cancel-write method, or app-controlled retry method.
- A tracked/native publish can return a stream-local correlation id for a pre-acceptance conflict or failure; that id has no durable receipt row and later reattachment returns not found. Reattachment replays retained receipt state and terminal/persistence facts, not transient `Routed` or `Sent` history. There is no receipt-enumeration API, so an app crash after acceptance but before persisting the id leaves no public way to rediscover that id.
- Rust diagnostics additionally carry `discovered_private_relays_rejected`, `relays_rejected_over_cap`, `store_degraded`, and `transport_degraded`. Swift/Kotlin currently expose relay summaries, uncovered author count, dropped merge rules, and `transportDegraded`; `store_degraded` is not projected natively.
- `AuthPhase` and AUTH-related `SourceStatus` variants are reserved vocabulary and are not populated by the current engine.
- Rust/raw FFI config supports `allowed_local_relay_hosts`, `max_relays`, and `max_native_tasks`. Hand-written Swift/Kotlin `NMPConfig` exposes store path, indexer/app/fallback relays, and `maxNativeTasks`; it still omits the local-host and relay ceilings.
- Swift/Kotlin ship an opt-in plaintext file account store. Neither is encrypted or a standard secure-storage provider. Secret zeroization and standard native provider restoration remain incomplete.
- Kotlin is a desktop JVM falsifier, not an Android AAR. Android OS handoff code belongs to the host; NIP-55 execution and Compose UI are not shipped.
- Governed sign-only uses the currently active signer, freezes the author before asynchronous work, verifies the returned event against the exact request, and creates no write intent, pending row, receipt, storage mutation, route, relay attempt, or publication. Accepted pending operations consume one bounded native-task slot and are cancellable; direct Rust external signers receive an opaque, cloneable `PendingSignerSender` completion door rather than a channel receiver or decomposable pending operation. Swift and Kotlin cancellation bridges each use one terminal state so completion and cancellation cannot both win.
- NIP-11 snapshots are process-local and engine-owned; cross-process persistence is not shipped. The cache retains at most 256 last-good snapshots, including refreshing entries. Flights use the shared native executor with zero queue, each flight admits at most 64 waiters, and an uncached completion is delivered without retention when every cached entry is refreshing. Reducer evidence is retained only for relays in the current read plan and diagnostic freshness expires from the engine clock at the cited document deadline. The production request uses explicit Hickory DNS plus rustls HTTP under one three-second total deadline, rejects URL credentials, follows no redirect, and performs no automatic retry. Swift tests run on the macOS host and the generated simulator slices compile; an iOS Simulator runtime harness remains open in issue #465. Kotlin tests target desktop JVM, and there is no Android/AAR qualification. Full snapshot values still deep-clone across cache/waiter fan-out; issue #467 tracks reducing that finite byte amplification.
- NIP-02 follow/unfollow preserves an existing contact list and refuses a missing base. First contact-list creation is a separate, unshipped policy.
- Every engine now owns a finite, zero-queue native-task executor for observer/action drains, signer waiters/mappers, and engine-associated NIP-46 work. The default `max_native_tasks`/`maxNativeTasks` is 12. Saturation is a typed `ExecutorSaturated` refusal before ownership transfer; OS spawn refusal remains the separate typed `ThreadUnavailable`. After a native NIP-46 connection handle exists, an inner session/relay-worker failure is instead an immediate streamed failure reason followed by closure; do not parse that string back into a typed error or call it a timeout. Direct-Rust NIP-46 sessions created without an engine each own a finite executor, but the application still controls how many such independent sessions exist, so this is not a process-global thread bound.
- Content-session policy limits remain per session, and each active target may consume one canonical plus multiple helper observations from the shared engine executor. Keep a separate app-level aggregate permit budget. Native receipt bridges still expose no detach handle.
- Native tracked/composed publish reserves and starts its receipt-observer bridge before core acceptance, and composed publish does so before consuming the take-once intent. Executor saturation or bridge-thread refusal therefore returns synchronously without accepting a write or consuming that composed intent; the remaining lost-id risk is process loss after a successful return but before app persistence because receipt enumeration is absent.

## Raw UniFFI parity seam

Generated `NMPFFI` / `uniffi.nmp_ffi` bindings are not the supported ergonomic app tier, but they are the projection seam the native wrappers must preserve. At this revision `NmpEngineConfig` includes `allowed_local_relay_hosts`, `max_relays`, and `max_native_tasks`; `NmpEngine` exposes filter/demand observation, publication, receipt reattachment, governed sign-only through `SignEventObserver` plus a cancellable handle, diagnostics, lifecycle, one-shot relay information, following, NIP-29 composition, and NIP-46 connections. `FfiError` distinctly projects `ExecutorSaturated { component, capacity }`, `RelayInformationWaitersSaturated { capacity }`, and `ThreadUnavailable { component, reason }`; other NIP-11 acquisition failures remain the typed `RelayInformationUnavailable { reason }` boundary rather than an empty document. Raw `native_task_census` and `await_native_tasks_idle` provide exact lifecycle falsification; the ergonomic wrappers intentionally keep those two methods internal. Native app code should use `NMP` or `com.nmp.sdk` and must not reach around a wrapper gap by importing generated types.
