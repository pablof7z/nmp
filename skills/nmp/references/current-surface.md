# Current surface and gaps

Verified-Revision: `5a508eaf5ad9d75e08645b41975e3312cf96aad7`

Verified on 2026-07-14. Recheck the [source map](source-map.md) when any declared authority changes.

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
| Diagnostics stream | yes | yes, narrower snapshot | yes, narrower snapshot |
| Native task ceiling | `EngineConfig.max_native_tasks`, default 12 | `NMPConfig.maxNativeTasks`, default 12 | `NMPConfig.maxNativeTasks`, default 12 |
| Engine/query observation refusal | ordinary `Engine::observe`: `ThreadUnavailable`; NIP-02 observation: `ExecutorSaturated` or `ThreadUnavailable` | synchronous `NMPError.executorSaturated` or `.threadUnavailable` | synchronous `NMPError.ExecutorSaturated` or `.ThreadUnavailable` |
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
- NIP-02 follow/unfollow preserves an existing contact list and refuses a missing base. First contact-list creation is a separate, unshipped policy.
- Every engine now owns a finite, zero-queue native-task executor for observer/action drains, signer waiters/mappers, and engine-associated NIP-46 work. The default `max_native_tasks`/`maxNativeTasks` is 12. Saturation is a typed `ExecutorSaturated` refusal before ownership transfer; OS spawn refusal remains the separate typed `ThreadUnavailable`. After a native NIP-46 connection handle exists, an inner session/relay-worker failure is instead an immediate streamed failure reason followed by closure; do not parse that string back into a typed error or call it a timeout. Direct-Rust NIP-46 sessions created without an engine each own a finite executor, but the application still controls how many such independent sessions exist, so this is not a process-global thread bound.

## Raw UniFFI parity seam

Generated `NMPFFI` / `uniffi.nmp_ffi` bindings are not the supported ergonomic app tier, but they are the projection seam the native wrappers must preserve. At this revision `NmpEngineConfig` includes `allowed_local_relay_hosts`, `max_relays`, and `max_native_tasks`; `NmpEngine` exposes filter/demand observation, publication, receipt reattachment, diagnostics, lifecycle, following, NIP-29 composition, and NIP-46 connections. `FfiError` distinctly projects `ExecutorSaturated { component, capacity }` and `ThreadUnavailable { component, reason }`. Raw `native_task_census` and `await_native_tasks_idle` provide exact lifecycle falsification; the ergonomic wrappers intentionally keep those two methods internal. Native app code should use `NMP` or `com.nmp.sdk` and must not reach around a wrapper gap by importing generated types.
