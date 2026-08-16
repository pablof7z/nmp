# Lifecycle and recovery

## Ownership table

| Resource | Create at | End with | Important consequence |
|---|---|---|---|
| Engine | app/service composition root | `shutdown`/`close`, then release | one owner for store, transport, queries, signers, receipts |
| Swift query/diagnostics/follow observation | feature model or scoped task | `cancel` or release owner | observation is eager |
| Kotlin query/diagnostics flow | collection scope | cancel collector; share deliberately | every unshared collection subscribes |
| Parsed content document | content feature owner | plain value; nothing to close | parsing is pure; any live reference is an ordinary query the app owns |
| Receipt fact stream | delivery/activity owner | Swift `ReceiptStatus.cancel()`; Kotlin end the collection scope | detaches the live stream only; the durable write is untouched |
| Durable write obligation | engine, from acceptance | `cancel` before signing, then `removePublishQueueEntry` | the only termination path for a write parked on a missing signer |
| Durable receipt id or correlation token | app durable state | explicit retention policy | either one recovers a write; paging `publishQueue` finds the rest |

## Construction and observation failures

Observers, actions, and signers run as async tasks on one shared engine-owned runtime. No OS thread is consumed per observation while waiting, and there is no worker/task capacity refusal; private NIP-11 bounds backpressure producers. `EngineStartFailed` means the engine itself could not be constructed. `ObservationUnavailable` means only that store degradation prevented an ordinary or windowed observation's initial canonical projection from opening; relay connection/worker failure remains acquisition evidence.

Handle each failure at the operation that owns it. Engine construction is the throwing creation call. For a live observe the failure surfaces where observation starts: for Swift the throwing creation call, and for Kotlin `observe(...)` returns a cold `Flow`, so it surfaces when collection starts, not when the flow value is created.

1. Do not store the resource until creation succeeds.
2. Present a bounded operational failure or retry affordance appropriate to the feature.
3. Tear down any earlier sibling resources created by the same feature attempt.
4. Record the component/reason without secrets.
5. Respect each public ownership shape: query/NIP-02 observation returns no handle on error; native follow/unfollow returns a thin action stream whose successful facts are the ordinary receipt and whose immediate refusal is typed.

Direct Rust `Engine::new` can return `EngineError::InvalidRelayUrl`, `StoreOpenFailed`, `StoreAlreadyOpen`, `StoreUnsupportedSchema { path, expected, found }`, or `EngineStartFailed`. The two store refusals are opposites and must not be handled together: `StoreOpenFailed` is a positive claim that discarding the store is *not* the recovery (damaged current-epoch bytes, a refused lock, an unresolvable path), while `StoreUnsupportedSchema` is the one refusal a fresh store does fix. Nothing is migrated, adopted, drained, or reset on either, and no partial engine escapes. Taking the discard costs the publish queue permanently — accepted-but-unpublished writes, receipts, correlation tokens, route revisions, attempt evidence — so it stays a separate deliberate act through `reset_persistent_store`, never an automatic retry. `found` is `None` when the store carries no marker this build can read, which means "not this epoch", never "no data".

`Engine::observe` refuses `WindowInitialExceedsMax { initial, max }`, `WindowSelectionHasLimit`, `WindowAggregateResultLimit`, `ObservationUnavailable`, and `EngineClosed`. Two families that look adjacent belong elsewhere. The four `LiveQueryError` refusals — `EmptyUnion`, `AggregateResultLimitZero`, `NestedAggregateResultLimit`, `TooManyQueryBranches { requested, maximum }` — are raised by `LiveQuery::union` at declaration time, before an observation, handle, mailbox, graph claim, or wire request can exist; an over-cap declaration installs nothing rather than a subset. `AuthCapabilityRegistryFull { limit }` (the app-configured `max_auth_capabilities` ceiling) and `AuthCapabilityInstanceExhausted` come from capability registration such as `add_auth_policy`, not from observing. An ordinary or windowed `Engine::observe` returns `ObservationUnavailable` only for initial canonical-projection setup failure after store degradation; relay opens do not feed this error, and it is never a worker-pool-busy, task-admission, permit, or queue-full outcome. `set_following` returns `Result<ReceiptStream, FollowActionFailure>`; success is the ordinary durable receipt lifecycle, while pre-custody failure is returned directly. It has no separate acquisition worker, retry state, capacity refusal, or thread refusal.

Kotlin normalizes synchronous raw exceptions through `nmpRethrowing`.

`Ok` from `publish` *is* acceptance; there is no bridge established before it and no capacity or thread refusal on the path. NIP-22 composes an ordinary `WriteIntent` and follows the generic path. NIP-29's `Group::publish` mints its intent privately and returns the same ordinary receipt stream. Neither has a composed carrier or a second lifecycle. Persist the receipt id promptly, but process loss before that is recoverable: mint a `correlation` token, persist it before publishing, and reattach by token — or page `publishQueue` to find it again.

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
2. Restore the engine from the app-stored opaque whole-session payload. NMP ships no session store or remote-signer provider.
3. Confirm the intended current account and provider availability.
4. Recreate current feature demands from app state. NMP restores cached facts but does not invent app queries.
5. Page `publishQueue` to see what is still outstanding — it is a bounded inspection taking a row limit and a receipt-id cursor, so enumerating everything means walking the pages, and it never blocks or waits for settlement. Reattach by retained id or by correlation token, fold the replayed facts, and decide cancel/remove for parked and refused entries.
6. Start new UI observers only after the model is ready to own their teardown.

NMP may restore canonical rows, provenance, source evidence, durable write lanes, and retained receipt facts. It does not restore UI navigation, ordering, moderation state, query-handle ownership, or secret material from the event/delivery store.

## Receipt recovery matrix

| Reattachment result | Meaning | App response |
|---|---|---|
| Attached | retained facts are readable; carries the resolved receipt id and a replay cursor that is `None` once caught up to live work | resume observation and fold facts. When you reattached by correlation token, record the returned id — it is the only place you can learn it |
| Not found | no retained receipt at that id or token | show unknown/not retained; do not claim failure or success |
| Retained but unreadable | retained state exists but the durable receipt or attempt evidence cannot be decoded | surface recovery failure and preserve evidence for diagnosis. Publication and terminal outcome are unknown, so never re-author blindly |

A refusal before acceptance yields a typed error and no id at all, so every id you hold names a write actually in custody. Fact-stream closure is not an ACK. Reattachment traverses the durable `WriteFact` history in finite pages before streaming onward, and lag is the typed `FactStreamLagged` rather than silent loss. `RelayWaiting::BackingOff` is the engine-owned scheduler's evidence, not a same-obligation retry door — app-controlled retry is the one thing on this list that genuinely does not exist. Enumeration (`publishQueue`), write cancellation (`cancel`), and live-stream detachment (Swift `ReceiptStatus.cancel()`, Kotlin collection-scope teardown) all do.

Kotlin's receipt status is a cold `Flow` pull loop that cancels the underlying stream when its collection scope ends. The live fact channel is finite: a consumer that falls behind gets `NMPError.FactStreamLagged` rather than unbounded growth or silent drops. Keep one collector per owned receipt and fold facts promptly.

## Sign-out

Treat identity persistence and the NMP event store as different authorities:

1. Stop creating new unsigned writes for the account.
2. Resolve unsigned obligations for the departing account: `cancel(receiptId:)` each one, then `removePublishQueueEntry(receiptId:)`. That two-call pair is the only way such a write ends.
3. Remove the account from the session and persist the resulting whole-session payload.
4. Clear current selection if another account should not become current.
5. Close remote signer connections and observers.
6. Shut down the engine if the app session owns it.

Do not delete the canonical store merely to sign out unless the product explicitly intends to erase cached events, evidence, pending writes, and receipts too.

## Destructive reset

`resetPersistentStore` is an offline filesystem operation:

1. Cancel query, diagnostics, content, following, and receipt observers.
2. Shut down and release every engine using the path.
3. Call reset for that store path.
4. Separately clear the app-stored session payload if the requested operation is full logout/erase.
5. Construct a new engine only after reset completes.

Reset is not a repair loop for a live engine, and it says so rather than half-working: a path any engine in this or another process still owns is refused with `StoreStillOpen`, and a removal that fails is `StoreResetFailed`. It erases canonical events, pending writes, receipts, coverage, and evidence, but not an app-stored session payload.

## Failure classification

Keep recovery owned by the failing layer:

- query source disconnect: transport reconnects while demand remains live;
- unavailable signer provider: restore the same session and wait for that account's provider; do not re-author;
- remote signer handoff failure: close that connection attempt and begin a new explicit attempt;
- durable relay failure: delivery owns attempts/backoff and emits receipt facts;
- unacked relay handoff: `RelayState::Sent` already means written-but-unacked and is not terminal; let the lane run, never blind resend;
- replaceable-coordinate conflicts: there is no caller-facing compare-and-swap payload to lose one. Replayable `ReplaceableOperation` capabilities own an explicit first-value instead. NIP-02 supplies a complete empty kind:3, retains the operation, and automatically reapplies it over later source truth, so its typed follow/unfollow action has no stale-base retry workflow;
- engine-start or observation infra failure (`EngineStartFailed` at construction, `ObservationUnavailable` for ordinary or windowed initial canonical-projection setup): preserve the owning boundary and retry only as a new bounded attempt; relay connection failure remains acquisition evidence and no operation is refused for worker/task capacity;
- unsupported store schema epoch: `StoreUnsupportedSchema` at construction is the one failure whose recovery genuinely is a fresh store, and it is still the app's call — close every owner, then reset deliberately, having told the person the publish queue does not survive it;
- store reset: explicit destructive user/maintenance operation, never automatic fallback.

## Teardown proof

A lifecycle implementation is incomplete until a test proves:

- dropping/cancelling the last query withdraws demand;
- cancelling one shared UI consumer does not accidentally tear down another;
- repeated open/close stays inside the thread/resource budget;
- after the last observer/session ends, the shared engine runtime has no lingering async work for it and no OS thread is retained on its behalf, proven by an event rather than polling or sleeps;
- content parsing holds nothing to leak: any live reference is an ordinary query with ordinary teardown;
- an old signer connection cannot detach a newer replacement;
- detaching a receipt fact stream leaves the durable write intact, and `cancel` + `removePublishQueueEntry` genuinely terminate a signer-parked write so it disappears from `publishQueue()`;
- shutdown is deterministic and idempotent; and
- restart reconstructs declared demand and reattaches selected receipts without secret leakage.
