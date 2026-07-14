# Lifecycle and recovery

## Ownership table

| Resource | Create at | End with | Important consequence |
|---|---|---|---|
| Engine | app/service composition root | `shutdown`/`close`, then release | one owner for store, transport, queries, signers, receipts |
| Swift query/diagnostics/follow observation | feature model or scoped task | `cancel` or release owner | observation is eager |
| Kotlin query/diagnostics flow | collection scope | cancel collector; share deliberately | every unshared collection subscribes |
| Content session/claim | content feature owner | Swift claim `cancel`, then session `stop`; Kotlin claim/session `close` | nested references remain live while claimed |
| NIP-46 connection | account/sign-in owner | close exact connection | OS handoff is not readiness |
| Receipt consumption task/collector | delivery/activity owner | cancel app task/collector | public `Receipt` has no detach handle; cancellation stops app consumption, not the admitted native drain or write |
| Durable receipt id | app durable state | explicit retention policy | required because receipt enumeration is absent |

## Construction and admission refusal

Every engine owns a finite, zero-queue native-task executor. Its `max_native_tasks`/`maxNativeTasks` default is 12, and it covers native observer/action drains, signer waiters/mappers, and engine-associated NIP-46 work. A full executor reports typed `ExecutorSaturated`; an admitted OS thread that cannot start reports typed `ThreadUnavailable`. Standalone direct-Rust NIP-46 sessions each own a separate finite executor, so their individual work is bounded but their process-wide count remains application-owned.

Handle refusal at the operation that actually starts native work. For Swift this is normally the throwing creation call. Kotlin `observe(...)` returns a cold `Flow`, so query refusal occurs when collection starts, not when the flow value is created.

1. Do not store the resource until creation succeeds.
2. Present a bounded operational failure or retry affordance appropriate to the feature.
3. Tear down any earlier sibling resources created by the same feature attempt.
4. Record the component/reason without secrets.
5. Respect each public ownership shape: query/NIP-02 observation and direct NIP-46 setup return no handle on error, while `set_following` always returns a `FollowAction` and reports worker refusal through its terminal status.

Direct Rust `Engine::new` and ordinary `Engine::observe` can return `EngineError::ThreadUnavailable`; ordinary observation does not consume an executor slot. `nmp_nip02::observe_following` can return either `EngineError::ExecutorSaturated` or `EngineError::ThreadUnavailable`. `set_following` returns `FollowAction`, not `Result`; read either refusal from `FollowAction::recv` as `FollowActionStatus::Failed` with the matching `FollowActionFailure` variant. Direct `Nip46Invitation::connect*` and `Nip46Signer::connect_bunker*` return matching `Nip46Error` variants without a signer handle.

Swift NIP-46 connection methods are throwing and Kotlin normalizes synchronous raw exceptions through `nmpRethrowing`. Derive/cache any URI or Android handoff value before invitation connection consumes the invitation; then connect, observe state, and launch the cached handoff. Executor saturation occurs before consumption, while an admitted outer bridge whose OS thread cannot start fails after consumption; only the latter requires a fresh invitation/handoff. If a connection handle returns and inner session/relay setup later fails—including inner executor or OS-thread refusal—consume the immediate streamed `failed(reason)`/`Failed` and closure; do not parse the reason into a typed error or call it a readiness timeout.

Native tracked/composed publish reserves and starts the receipt bridge before calling core acceptance, and composed publish does so before taking its intent. `ExecutorSaturated` or `ThreadUnavailable` therefore returns synchronously with no accepted obligation or consumed composed intent and no receipt handle. After a successful return, persist the id promptly: process loss before app persistence remains unrecoverable because receipt enumeration is absent.

## Background, disconnect, and resume

Keep semantic demand alive exactly while the owning feature needs it. NMP reconnects transport and recompiles/replays still-live demand; the app must not watch socket state and reopen raw subscriptions.

When the app backgrounds:

- keep an observation if the feature genuinely remains live and platform policy permits it;
- otherwise cancel it and recreate the semantic demand on resume;
- never persist query handles across process death;
- do persist the store path, app feature state needed to reconstruct demands, active-account reference, and receipt ids required by product policy.

An in-progress relay reconciliation is connection-local. A replacement connection starts valid fresh work; it does not continue a half-finished exchange by assertion.

## Process restart sequence

1. Recreate the engine over the same persistent store.
2. Restore signer capability from app-owned secure storage, or explicitly opt into the insecure development store for local keys. Swift/Kotlin ship no secure NIP-46 credential vault or automatic remote-signer reconnection; reconnect from host-retained protected material or perform a fresh handoff.
3. Add/select the intended active account.
4. Recreate current feature demands from app state. NMP restores cached facts but does not invent app queries.
5. Reattach retained receipt ids and fold replayed/current facts.
6. Start new UI observers only after the model is ready to own their teardown.

NMP may restore canonical rows, provenance, source evidence, durable write lanes, and retained receipt facts. It does not restore UI navigation, ordering, moderation state, query-handle ownership, or secret material from the event/outbox store.

## Receipt recovery matrix

| Reattachment result | Meaning | App response |
|---|---|---|
| Attached | retained facts are readable | resume observation and fold facts |
| Not found | no retained receipt at that id | show unknown/not retained; do not claim failure or success |
| Retained but unreadable | retained state exists but cannot be decoded/read | surface recovery failure and preserve evidence for diagnosis |

A stream-local id emitted by a pre-acceptance failure may never have had a durable receipt row. Receipt bridge admission now precedes acceptance, so capacity or OS-thread refusal leaves no accepted write. Receipt-channel closure is not ACK. Reattachment reconstructs retained receipt state plus current/persisted `AwaitingRelay`, `AwaitingAuth`, `RetryEligible`, `HandoffAmbiguous`, proven-`Written` `Sent`, terminal-attempt, and persistence-blocked facts; it does not replay transient `Routed` history or invent an ephemeral handoff. `RetryEligible` is the engine-owned scheduler's evidence, not a same-obligation retry door. There is no public enumeration, write cancellation, app-controlled retry, or receipt-observer detach API. Cancelling a Swift task or Kotlin collector stops only that app consumer; the native receipt bridge remains until its channel/engine closes.

Kotlin's receipt bridge uses an unbounded status channel. Keep one collector for an owned receipt alive until terminal/channel/engine closure when feasible, fold statuses promptly, and avoid repeated reattachment to the same id: cancellation does not detach an old bridge, and another attachment creates another long-lived bridge.

## Sign-out

Treat identity persistence and the NMP event store as different authorities:

1. Stop creating new unsigned writes for the account.
2. Decide product policy for unresolved accepted obligations; there is no cancel verb.
3. Clear any separately persisted account credential before engine shutdown when using the insecure file store.
4. Clear/deactivate the active account.
5. Close remote signer connections and observers.
6. Shut down the engine if the app session owns it.

Do not delete the canonical store merely to sign out unless the product explicitly intends to erase cached events, evidence, pending writes, and receipts too.

## Destructive reset

`resetPersistentStore` is an offline filesystem operation:

1. Cancel query, diagnostics, content, following, and receipt observers.
2. Close NIP-46 connections.
3. Shut down and release every engine using the path.
4. Call reset for that store path.
5. Separately clear account/signer persistence if the requested operation is full logout/erase.
6. Construct a new engine only after reset completes.

Reset is not a repair loop for a live engine. It erases canonical events, pending writes, receipts, coverage, and evidence, but not a separately configured account checkpoint.

## Failure classification

Keep recovery owned by the failing layer:

- query source disconnect: transport reconnects while demand remains live;
- missing signer: restore/attach the same expected identity; do not re-author;
- remote signer handoff failure: close that connection attempt and begin a new explicit attempt;
- durable relay failure: outbox owns attempts/backoff and emits receipt facts;
- at-most-once ambiguity: preserve `OutcomeUnknown`; never blind resend;
- replaceable conflict: acquire the new canonical base and make a new user decision;
- executor saturated or thread unavailable: preserve the owning boundary—synchronous creation refusal, terminal follow-action failure, or post-handle NIP-46 streamed failure—and retry only as a new bounded attempt;
- store reset: explicit destructive user/maintenance operation, never automatic fallback.

## Teardown proof

A lifecycle implementation is incomplete until a test proves:

- dropping/cancelling the last query withdraws demand;
- cancelling one shared UI consumer does not accidentally tear down another;
- repeated open/close stays inside the thread/resource budget;
- raw/native lifecycle census reaches exact `admitted == 0 && running == 0` through the event-driven idle barrier after teardown, without polling or sleeps;
- aggregate visible-row content sessions and claimed references stay inside app-owned budgets: cap row sessions separately and charge each distinct claimed target `1 + helpers.count` permits from its reference-demand plan;
- an old signer connection cannot detach a newer replacement;
- app receipt-consumer cancellation leaves durable write ownership intact and is not described as native observer teardown;
- shutdown is deterministic and idempotent; and
- restart reconstructs declared demand and reattaches selected receipts without secret leakage.
