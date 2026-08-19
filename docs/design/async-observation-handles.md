# Pull-based async observation handles (#680)

Supersedes the earlier OS-thread-per-observation ownership model (#442/#446) for the
observation-delivery path. Progresses #46 (bounded delivery). This document is
the durable record of the replacement architecture.

> **Runtime follow-up:** #704 subsequently removed the rescoped
> blocking-adapter pool and per-session executors described below. Current
> logical adapter work runs on the shared async runtime with private physical
> backpressure.
> The observation-handle and mailbox contract in this record remains current.

## The defect being removed

NMP bridged every long-lived Rust subscription across UniFFI by spawning **one
dedicated OS thread per observer**, each blocking on `recv()` for the
observation's lifetime and invoking a foreign callback. A global zero-queue
executor (`nmp-executor`, default capacity 12) capped those threads and exposed
that cap to apps as `maxNativeTasks`/`max_native_tasks`, returning
`ExecutorSaturated` when exceeded. Ordinary composition — app, group, inbox,
timeline, profile observers on one engine — made opening an *unrelated* query
fail on the ~13th observation.

Structural equation being broken:

> live app observations == dedicated NMP OS threads

## Primary invariant (the contract this design meets)

1. Opening ordinary live observations creates **no** NMP-owned OS thread per
   observation. NMP OS-thread count is `O(1)` engine infrastructure +
   `O(handles)` lightweight metadata — never `O(handles)` OS threads.
2. Applications never see a global native-task capacity concept: no config, no
   census, no idle barrier, no capacity error, no retry.
3. Hundreds of idle/intermittent observation handles coexist on one engine
   without admission refusal or linear thread growth.

Semantic bounds are kept/strengthened: latest-state mailboxes for query &
diagnostics; durable reattachable receipts; bounded relay/network concurrency;
deterministic cancellation/shutdown; isolation from slow foreign consumers.

## Ownership model

- **The engine owns bounded delivery state.** Each live observation is one
  single-slot latest-state mailbox in engine-owned memory. An observation
  *handle* is lightweight: an `Arc` to that mailbox plus an idempotent cancel
  token. Awaiting `next()` reserves **no** native thread and **no** runtime.
- **Delivery is pull-based.** The foreign consumer requests the next value by
  awaiting `next_*()`. The future is waker-aware and completes when: a retained
  value is available; the stream closes (engine dropped the producer);
  the handle is cancelled; or the engine shuts down.
- **Current-state streams (row/window/diagnostics) conflate.** The single slot
  holds one pending latest value (row deltas fold in place; window/diagnostics
  keep the newest complete snapshot). A slow consumer skips obsolete
  intermediates and still converges to the newest exact local state. One slow
  consumer cannot delay the engine reducer or another consumer.
- **Receipt facts remain durable.** Receipts are not disposable frames: the
  persisted outbox/redb store is the source of truth; a detached consumer
  reattaches and traverses durable `WriteStatus` facts through finite pages.
  Live buffering is fixed-capacity and lag is typed; correctness never depends
  on an unbounded queue.

## The waker-aware mailbox primitive

`crates/nmp-engine/src/runtime/diagnostics_channel.rs` (`latest_channel`) is the
one primitive under `RowsReceiver`, `HistoryReceiver`, and diagnostics. It gains
async wakeup **directly** (not a second queue, no polling, no timer):

- `Slot<T>` gains `waker: Option<Waker>` beside `value`/`state`.
- `LatestSender::update`/`send`, `LatestSender::Drop`, and
  `LatestReceiver::close` take the waker out and `wake()` it after releasing
  the slot lock (in addition to `Condvar::notify`).
- `LatestReceiver::poll_recv(cx) -> Poll<Option<T>>`: returns `Ready(Some(v))`
  if a value is pending, `Ready(None)` if the producer is gone with no pending
  value or the consumer cancelled, else registers `cx.waker()` and returns
  `Pending`.
- `LatestReceiver::close()`: consumer-initiated idempotent close — sets
  `Cancelled`, discards the pending value, and wakes any pending waker and all
  condvar waiters. This is what `cancel()` uses to wake a pending `next()`
  *immediately*, before the engine teardown has dropped the producer.
- Blocking `recv`/`recv_timeout`/`try_recv` are unchanged and layered over the
  same slot — direct-Rust consumers keep them; async is additive, not the
  center.

### Async receiver wrappers (Send + Sync)

The blocking `RowsReceiver`/`HistoryReceiver`/`LatestReceiver` are deliberately
`!Sync` (single consumer). The direct-Rust async facade (`AsyncSubscription`,
`AsyncDiagnosticsSubscription` in `crates/nmp/src/subscription.rs`) instead
uses `Async*Receiver` wrappers that hold the mailbox `Arc<Inner<T>>` (which
*is* `Send + Sync`: `Mutex` + `Condvar` + `Waker`) plus:

- an `AtomicBool reading` guard: a second concurrent `next()` while one is in
  flight is **structurally rejected** with a typed misuse error
  (`ConcurrentNext`). We never accumulate pending readers or a second waker.
- for `AsyncHistoryReceiver` only, a `Mutex<BTreeMap<EventId, Row>>` for the
  receiver-side `delivered` reconcile state (row/diagnostics receivers have no
  receiver-side state — the row fold is entirely sender-side).

`next()` is `async fn`: it acquires the `reading` guard (RAII-released on
completion/drop), polls the mailbox via a small `poll_fn`, applies the
receiver-side transform (`PendingRows::into_message` / history `reconcile` /
`DiagnosticsSnapshot::from_engine`) under the guard, and returns
`Ok(Some(frame))` / `Ok(None)` (closed).

### Termination states (two, not one bool)

Cancellation is a distinct terminal from producer teardown, so the slot carries
a three-state `SlotState { Open, ProducerGone, Cancelled }` — an enum, not a
`closed` bool that would conflate the two terminal states:

- **`ProducerGone`** (sender `Drop` — natural teardown / engine shutdown):
  deliver any pending value *first*, then `None`. Preserves the existing
  `pending_transition_is_delivered_before_disconnect` falsifier.
- **`Cancelled`** (consumer `close()` — an explicit `cancel()`): end *now*.
  `close()` discards the pending value under the slot lock, and `update()`
  becomes a no-op once `Cancelled`, so a cancelled 10k-change query's producer
  stops rebuilding the slot immediately (no memory drift until the async
  unsubscribe lands). `poll_recv`/`recv` check `Cancelled` first → `None`
  unconditionally, so **no post-cancel frame is ever observed.**

### Race correctness

`next()` pending, an arriving frame, `cancel()`/`close()`, sender `Drop`
(engine shutdown), and the guard release may interleave. Correctness rests on
the single `Slot` mutex: every state transition (`value` set, `state` changed,
`waker` registered/taken) happens under it, so there is exactly one owner of the
pending value and the terminal state. A wake can never be lost (the
waker is registered before `Pending` is returned and taken under the same lock
that sets `value`/`state`); a terminal `None` can never be lost (poll re-checks
the state on every wake); no post-cancel frame is delivered (`close()` changes
the state and discards the slot under the same lock, and later producer updates
are no-ops). No spin loop, no timer.

## Cancellation & lifecycle

- `cancel()` is idempotent (the existing `ObservationCancel` `AtomicBool` guard)
  and calls the mailbox `close()` (immediate pending-`next()` wakeup to `None`)
  **and** the engine withdrawal (`unsubscribe`/`unsubscribe_history`/diag cancel).
- Dropping the handle's `Arc` runs its `Drop`, which calls `cancel()` — closing
  the Rust handle.
- Engine shutdown drops every producer `LatestSender`, closing every mailbox and
  waking every pending `next()` to `None`.

## The rescoped blocking-adapter pool

The generic `nmp-executor` is **removed from the observation path entirely**.
What remains genuinely blocks on foreign systems or a reactor and is bounded by
writes/sessions/fetches (never by observation count):

- NIP-11 HTTP/DNS flights (per-flight `block_on` current-thread runtime);
- remote-signer result waiters and the sign-event drain (blocking `recv_or_cancel`);
- AUTH policy/signer foreign-capability calls;
- NIP-46 session worker + event forwarder + connect/switch/result helpers.

These form **one resource class — blocking foreign/reactor adapters** — kept on a
renamed, internal pool (`nmp-executor` → rescoped; type names lose the
"native task"/observation vocabulary). Properties:

- fixed internal capacity, **not** app-configurable, **not** CPU-derived, **not**
  surfaced (no census/idle/capacity field anywhere in the SDK);
- observations never touch it, so opening a query can never fail because of it;
- its saturation is an internal-adapter concern; the public
  `executorSaturated` / "native task executor is at capacity" strings are
  removed — remaining adapter-busy conditions surface (if at all) as
  class-specific errors (e.g. relay-info/ signer busy), never a global
  native-task ceiling.

> **Note / open call for review:** the alternative is to also convert NIP-11 and
> the signing waiters to pure async futures on one shared engine Tokio I/O
> runtime (the audit flags NIP-11 as the strongest candidate), leaving only
> NIP-46's long-lived session actor on a session-scoped pool. That is a larger,
> higher-risk change to `nmp-signer`/transport internals and is **out of scope
> for #680**, whose invariant is fully met by moving observations off the pool.
> Recorded here so the follow-up is not lost.

## Review resolutions (adversarial architecture pass)

An independent adversarial review (recorded here so the reasoning is not lost)
confirmed the core cut and forced these corrections, all folded into the
implementation:

1. **Post-cancel-frame race** — fixed by the three `SlotState` values above
   (consumer `Cancelled` discards the value and no-ops the producer; producer
   `ProducerGone` drains-then-ends). Verified by mailbox unit tests.
2. **Wake-after-unlock is load-bearing.** A waker may resume its continuation
   inline on the waking thread, which re-enters `poll_recv` and re-takes the
   slot lock; waking under the lock would self-deadlock a non-reentrant
   `std::Mutex`. Every producer transition therefore takes the waker out under
   the lock and wakes it *after* releasing.
3. **Receipts & follow-actions are FIFO fact streams, not latest-wins.** They
   use a distinct waker-aware **fixed-capacity FIFO** (`fifo_channel`, capacity
   32), not the conflating `latest_channel`. Retry concurrency is bounded, but
   retry *count* is not; the earlier claim that retry caps made a receipt's
   history finite was false. When a paused consumer fills the live FIFO, the
   sender retains the already-buffered prefix, rejects later sends, is pruned
   from the producer, and the consumer receives typed `FactStreamLagged` after
   draining that prefix. Receipt reattachment reconstructs deterministic
   durable pages of at most 32 facts using an identity-stable, per-lane
   continuation bounded by relay fan-out. It does not use a count into the
   mutable reconstructed history: durable facts added between pages remain
   unseen and are delivered exactly once. A caught-up check after a full page
   atomically attaches live work. Persisted attempt-history retention/GC
   remains the separate #46 concern; the live delivery edge neither grows
   without bound nor claims a dropped fact was delivered. Each live receipt
   delivery registration has a private identity tied to the consumer FIFO's close/drop hook;
   cancellation sends an exact detach command, so a pending write cannot
   accumulate dead registrations while parked without another status
   (bounded-delivery.md §3).
4. **`sign_event` stays a handle (`SignEventOperation`), not a callback.**
   `recv()` consumes the operation for the one signed result — a second call
   is a compile error, since `recv` takes `self` by value — replacing the
   observer-callback shape. Dropping the operation before completion cancels
   the exact signer RPC.
5. **NIP-46 session-lifetime workers move to session-owned threads.** Permanent
   slot occupants (session worker + event forwarder, session-lifetime) behind one
   fixed counter would be a miniature of the very defect — an unrelated
   `relay_information()` refused because two signer sessions are open. The
   engine-associated NIP-46 path therefore uses the already-existing
   `SessionExecutor::Owned` seam (its own threads, bounded by app-identity
   session count) instead of the shared pool. The remaining internal pool then
   hosts only genuinely *transient* blocking adapters (NIP-11 flights, AUTH
   foreign calls, remote-signer result waiters) — one honest class.

## Falsifiers

See `crates/*/tests`. The acceptance tests in #680
(thread-scaling ≥1000 handles, real 64+ composition, slow 10k-change consumer,
cancellation races, finite FIFO lag under 40 durable
retry cycles, paged receipt replay, receipt detach/reattach, 128 alternating
close/drop reattachments while permanently parked at `AwaitingCapability`,
shutdown with pending `next()`, retained-frame replay, commit/abort/cancel races, repeated
cancellation bounds, window-snapshot non-replay, 29er reproduction, symbol audit)
fail under the old one-thread-per-observer/unbounded-fact-delivery design and
pass under this one.
