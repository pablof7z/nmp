# Lifecycle and recovery

## Ownership table

| Resource | Create at | End with | Important consequence |
|---|---|---|---|
| Engine | app/service composition root | `shutdown`/`close`, then release | one owner for store, transport, queries, signers, receipts |
| Swift query/diagnostics/follow observation | feature model or scoped task | `cancel` or release owner | observation is eager |
| Kotlin query/diagnostics flow | collection scope | cancel collector; share deliberately | every unshared collection subscribes |
| Content session/claim | content feature owner | close claims, then session | nested references remain live while claimed |
| NIP-46 connection | account/sign-in owner | close exact connection | OS handoff is not readiness |
| Receipt consumption task/collector | delivery/activity owner | cancel app task/collector | public `Receipt` has no detach handle; cancellation stops app consumption, not the admitted native drain or write |
| Durable receipt id | app durable state | explicit retention policy | required because receipt enumeration is absent |

## Construction and admission refusal

Each engine now owns a finite, zero-queue executor for native observer/action drains, signer waiters/mappers, and engine-associated NIP-46 work. `max_native_tasks`/`maxNativeTasks` defaults to 12. A full executor returns typed `ExecutorSaturated` before ownership transfer; an OS spawn failure remains typed `ThreadUnavailable`. Ordinary direct-Rust query subscriptions do not consume an executor slot, while NIP-02 observation/actions and direct NIP-46 setup expose the corresponding owning error/status variants. Native wrappers map synchronous outer setup refusal to `NMPError`; inner NIP-46 session/relay failure after a handle exists remains a streamed failure followed by closure. Content-session multiplication still needs an app aggregate budget because it creates several admitted native observations across independently limited sessions.

Handle refusal at the operation that actually starts native work. For Swift this is normally the throwing creation call. Kotlin `observe(...)` returns a cold `Flow`, so query refusal occurs when collection starts, not when the flow value is created.

1. Do not store the resource until creation succeeds.
2. Present a bounded operational failure or retry affordance appropriate to the feature.
3. Tear down any earlier sibling resources created by the same feature attempt.
4. Record the component plus capacity or reason without secrets.
5. Do not assume a sentinel or half-open query/follow/signer handle exists; those supported paths refuse before it escapes.

For follow actions, executor saturation or thread refusal is a terminal typed action failure. Swift NIP-46 connection methods are throwing and Kotlin normalizes synchronous raw exceptions through `nmpRethrowing`. Invitation-based connection reserves capacity before consuming the invitation: `ExecutorSaturated` leaves it reusable, while a later `ThreadUnavailable` occurs after take and requires a fresh invitation/handoff. If the outer handle exists but inner session/relay setup fails, consume the immediate streamed `failed(reason)`/`Failed` and closure; do not parse the reason into a typed error or call it a readiness timeout.

Tracked native publish now reserves and starts its receipt drain before core accepts the durable obligation; composed publish does so before taking its intent. Synchronous `ExecutorSaturated` or `ThreadUnavailable` therefore means no write was accepted on that path. After a successful return, persist the id immediately because a process crash before app persistence is still publicly unrecoverable without receipt enumeration.

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

A stream-local id emitted by a pre-acceptance failure may never have had a durable receipt row. Receipt-drain admission failure occurs before write acceptance, but a process crash after a successful return and before app persistence can still lose the id. Receipt-channel closure is not ACK. Reattachment reconstructs retained state, not all transient progress. There is no public enumeration, write cancellation, same-obligation retry, or receipt-observer detach API. Cancelling a Swift task or Kotlin collector stops only that app consumer; the native receipt bridge remains until its channel/engine closes.

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
- repeated open/close stays inside `maxNativeTasks`, refuses without ownership transfer at saturation, and returns to idle after deterministic teardown;
- aggregate visible-row content sessions and claimed references stay inside app-owned budgets: cap row sessions separately and charge each distinct claimed target `1 + helpers.count` permits from its reference-demand plan;
- an old signer connection cannot detach a newer replacement;
- app receipt-consumer cancellation leaves durable write ownership intact and is not described as native observer teardown;
- shutdown is deterministic and idempotent; and
- restart reconstructs declared demand and reattaches selected receipts without secret leakage.
