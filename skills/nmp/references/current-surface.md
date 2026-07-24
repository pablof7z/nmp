# Current surface and gaps

Verified-Revision: `b37d8f2b251b2593713bcb243e542d298af51b71`

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
| Derived inner descriptor | full `Demand` | full `NMPDemand` | full `NMPDemand` |
| Query rows + scoped acquisition evidence | yes | yes | yes |
| Publish and stream receipt facts | `publish`, `publish_tracked` | `publish` returns `Receipt` | `publish` returns `Receipt` |
| Durable receipt reattachment | `reattach_receipt` | `reattachReceipt` | `reattachReceipt` |
| Persistent store reset | `reset_persistent_store` | `resetPersistentStore` | `resetPersistentStore` |
| Account add/activate/read | yes | yes | yes |
| Arbitrary signer registration | Rust only, with public `add_signer` | NIP-46 helpers, not arbitrary Rust capabilities | NIP-46 helpers, not arbitrary Rust capabilities |
| Governed sign-only event | `sign_event(SignEventRequest)` returns cancellable `SignEventOperation` | async throwing `signEvent(NMPUnsignedEvent)` | suspending `signEvent(NMPUnsignedEvent)` |
| Diagnostics stream | yes | yes, narrower snapshot | yes, narrower snapshot |
| One-shot NIP-11 relay information | async `relay_information(relay, policy)` | async throwing `relayInformation(for:policy:)` | suspending `relayInformation(relay, policy)` |
| Engine construction failure | `EngineError::EngineStartFailed` | construction throws `NMPError.engineStartFailed` | construction throws `EngineStartFailed` |
| Windowed `observe` cannot open its canonical projection after store degradation | `EngineError::ObservationUnavailable` | `NMPError.observationUnavailable` | collection-time `ObservationUnavailable` |
| Relay connection/worker unavailable during observation | acquisition evidence | source evidence | source evidence |
| NIP-02 action terminal failure | terminal `FollowActionStatus::Failed` with a `FollowActionFailure` variant (no capacity or thread refusal) | terminal `NMPFollowActionFailure` | following action not exposed |
| Initial NIP-46 connection refusal | matching `Nip46Error` from `Nip46Invitation::connect*` / `Nip46Signer::connect_bunker*` | synchronous matching outer `NMPError`; post-handle inner `.failed` | synchronous matching outer `NMPError`; post-handle inner `Failed` then `Closed` |
| NIP-02 following action | direct Rust protocol facade and Swift | yes | not on ergonomic Kotlin `NMPEngine` |
| NIP-29 composed group send | yes | yes | yes |
| Content parser/reference sessions | Rust crates, Swift `NMPContent`, Kotlin SDK | yes | yes |
| Ready-made UI | optional SwiftUI `NMPUI` | yes | no Compose package |

## Important gaps

- Public `Row` values expose the signed event and relay sources. They do not expose typed `Pending(intentId)`/`Signed(signature)` state, intent ids, or receipt ids. Pending-row mechanics exist internally and in the north-star contract; do not make an app depend on public metadata that is absent.
- Current `WriteStatus` cases are `Accepted`, `AwaitingCapability`, `Signed`, `Routed`, `AwaitingRelay`, `AwaitingAuth`, `RetryEligible`, `HandoffAmbiguous`, `Sent`, `Acked`, `Rejected`, `GaveUp`, `PersistenceBlocked`, `RoutePersistenceBlocked`, `OutcomeUnknown`, `ReplaceableConflict`, and `Failed` (platform naming differs). `RetryEligible(relay, attempt, eligibleAt)` reports the durable scheduler's persisted attempt ordinal and deadline; it is not an app-controlled retry method. There is no public `AttemptStarted`, `Cancelled`, cancel-write method, or app-controlled retry method.
- `AwaitingRelay(relay)` means the durable lane is waiting for connectivity; offline time itself consumes no attempt. `AwaitingAuth(relay)` means the lane is paused for relay authentication and arms no polling deadline. `HandoffAmbiguous(relay, attempt, observedAt)` preserves an unproven transport handoff without calling it sent. `Sent(relay, attempt, writtenAt)` is emitted or replayed only from the persisted `Written` handoff for that exact durable lane ordinal; queue acceptance, an `AttemptStarted` row, ambiguity, and ephemeral transport work cannot mint it.
- A tracked/native publish can return a stream-local correlation id for a pre-acceptance conflict or failure; that id has no durable receipt row and later reattachment returns not found. Reattachment reconstructs retained receipt state, current/persisted relay and AUTH waits, exact retry eligibility, ambiguous handoffs, proven `Sent` facts, terminal attempt outcomes, and persistence blockage. It does not reconstruct transient `Routed` history or invent an ephemeral handoff fact. There is no receipt-enumeration API, so an app crash after acceptance but before persisting the id leaves no public way to rediscover that id.
- Rust diagnostics additionally carry `discovered_private_relays_rejected`, `relays_rejected_over_cap`, `store_degraded`, and `transport_degraded`. Swift/Kotlin currently expose relay summaries, uncovered author count, dropped merge rules, and `transportDegraded`; `store_degraded` is not projected natively.
- `AuthPhase` and AUTH-related `SourceStatus` variants are reserved vocabulary and are not populated by the current engine.
- Rust/raw FFI config supports `allowed_local_relay_hosts` and `max_relays`. Hand-written Swift/Kotlin `NMPConfig` exposes store path and indexer/app/fallback relays; it still omits the local-host and relay ceilings. No tier exposes any worker/task/thread capacity: #704 removed all application-configurable task admission.
- Swift/Kotlin ship an opt-in plaintext file account store. Neither is encrypted or a standard secure-storage provider. Secret zeroization and standard native provider restoration remain incomplete.
- Kotlin is a desktop JVM falsifier, not an Android AAR. Android OS handoff code belongs to the host; NIP-55 execution and Compose UI are not shipped.
- Governed sign-only uses the currently active signer, freezes the author before asynchronous work, verifies the returned event against the exact request, and creates no write intent, pending row, receipt, storage mutation, route, relay attempt, or publication. Accepted pending operations run as async tasks on the shared engine runtime and are cancellable; direct Rust external signers receive an opaque, cloneable `PendingSignerSender` completion door rather than a channel receiver or decomposable pending operation. Swift and Kotlin cancellation bridges each use one terminal state so completion and cancellation cannot both win.
- NIP-11 snapshots are process-local and engine-owned; cross-process persistence is not shipped. The cache retains at most 256 last-good snapshots. One engine admits at most 8 live distinct-relay HTTP/DNS/body flights; `Refresh` coalesces same-relay callers onto one generation-guarded completion and excess callers suspend cancellably in their own futures without a public refusal. Reducer evidence is retained only for relays in the current read plan and diagnostic freshness expires from the engine clock at the cited document deadline. The production request uses explicit Hickory DNS plus rustls HTTP under one three-second total deadline, rejects URL credentials, follows no redirect, and performs no automatic retry. Swift tests run on the macOS host and the generated simulator slices compile; an iOS Simulator runtime harness remains open in issue #465. Kotlin tests target desktop JVM, and there is no Android/AAR qualification.
- NIP-02 follow/unfollow preserves an existing contact list and refuses a missing base. First contact-list creation is a separate, unshipped policy.
- Observations and logical adapter work run as async tasks on one shared engine-owned runtime. No OS thread is consumed per observation/session wait and ordinary operations have no capacity refusal; private NIP-11 and NIP-46 bounds backpressure producers. `EngineStartFailed { component, reason }` is construction-only. `ObservationUnavailable { reason }` means only that store degradation prevented a windowed observation's canonical projection from opening; relay connection/worker failure is ordinary acquisition evidence. After a native NIP-46 connection handle exists, an inner session/relay-worker failure is instead an immediate streamed failure reason followed by closure; do not parse that string back into a typed error or call it a timeout.
- Content-session policy limits remain per session, and each active target may open one canonical plus multiple helper observations on the shared engine runtime. Keep a separate app-level aggregate permit budget. Native receipt bridges still expose no detach handle.
- Native tracked/composed publish starts its receipt-observer bridge as async work on the shared runtime before core acceptance, and composed publish does so before consuming the take-once intent. There is no capacity or thread refusal on this path, so a returned handle reflects an accepted write; the remaining lost-id risk is process loss after a successful return but before app persistence because receipt enumeration is absent.

## Raw UniFFI parity seam

Generated `NMPFFI` / `uniffi.nmp_ffi` bindings are not the supported ergonomic app tier, but they are the projection seam the native wrappers must preserve. At this revision `NmpEngineConfig` includes `allowed_local_relay_hosts` and `max_relays`; `NmpEngine` exposes filter/demand observation, publication, receipt reattachment, governed sign-only through `SignEventObserver` plus a cancellable handle, diagnostics, lifecycle, one-shot relay information, following, NIP-29 composition, and NIP-46 connections. `FfiError` distinctly projects `EngineStartFailed { component, reason }` (engine construction only) and `ObservationUnavailable { reason }` (canonical history-projection setup only); every other NIP-11 acquisition failure crosses as the typed `RelayInformationUnavailable { kind: FfiRelayInformationErrorKind }` boundary (#494) rather than an empty document or a message string. `FfiRelayInformation.lastError` and `FfiNip46Failure`/`FfiBunkerParseError` (the NIP-46 connection observer's `on_failed`) are likewise typed enums, not strings. There is no worker/task census, idle-barrier method, or capacity refusal: private physical bounds backpressure without becoming app scheduling vocabulary. Native app code should use `NMP` or `com.nmp.sdk` and must not reach around a wrapper gap by importing generated types.
