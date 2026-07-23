# Eliminating internal task/thread admission capacity (#704)

Follows #680/#693 (which removed one-OS-thread-per-observation and the public
observation capacity). This removes the remaining internal admission-capacity
concept: the 32-slot engine "blocking-adapter" executor, the per-NIP-46-session
12-slot executor, and every `ThreadUnavailable`/`ExecutorSaturated` outcome
caused by them. The design record for why the architecture is shaped this way.

## The finding that shapes everything: category 3 is empty

An exhaustive audit classified every executor user. **No user is a genuinely
blocking foreign call** (category 3):

- Every foreign callback returns *ready-or-pending, non-blocking*:
  `SigningCapability::sign -> SignerOp{Ready|Pending}` (`capability.rs:20`),
  `CryptoCapability` (`capability.rs:27`), `AuthPolicy::evaluate ->
  AuthPolicyOp{Ready|Pending}` with an explicit "must not block" contract
  (`auth.rs:253`), NIP-46 `request_string -> SignerOp` (`nip46.rs:1576`).
- Every thread that "blocks" is parked on a `crossbeam bounded(1)` **completion
  door** (`op.rs` `PendingSignerOp::recv_or_cancel`, `auth.rs`
  `AuthPolicyPendingSender`) or an NMP-internal mpsc/subscription — waiting for a
  result another thread will post, never seizing the thread inside a foreign
  call.
- NIP-11 is natively-async reqwest + hickory (`relay_information.rs:638`) merely
  wrapped in a per-flight `block_on`.

**Consequence: the bounded blocking-worker scheduler is not required for
correctness.** Once the completion doors are waker-aware and a shared async
runtime exists, every user becomes an async task holding no OS thread. A blocking
scheduler would only be justified by a *future* genuinely-blocking foreign
adapter; none exists, so we do not build one. If one is introduced later it gets
a single shared, bounded, async-admission scheduler — designed, not assumed.

## The model

1. **One engine-owned async runtime.** A single `tokio` multi-thread runtime
   (fixed small worker count, default 2 — bounded OS threads) owned by the
   engine, built at construction. It hosts *all* async adapter work: NIP-11
   fetches, signer/AUTH completion awaits, NIP-46 session state machines,
   follow-action acquisition. Thousands of logical operations share these few
   worker threads because each yields at every `.await`. This does **not** impose
   a runtime on applications (they never see it); the single-threaded engine
   reducer stays its own dedicated thread (it is deliberately `!Send`-friendly).

2. **Waker-aware completion doors.** `PendingSignerOp` /
   `AuthPolicyPendingSender` change from `crossbeam bounded(1)` to a waker-based
   oneshot (`futures_channel::oneshot`, already used in `relay_information.rs`).
   `resolve()` fires the oneshot (and wakes the parked task) from whatever thread
   owns the result; the engine `.await`s it on the runtime, holding no thread.
   Drop-runs-cancel semantics map directly onto the future's `Drop`. A blocking
   `recv()` stays available for direct-Rust callers over the same enum-shaped
   completion door and condition variable, never as the architectural centre
   and never by creating another runtime or helper thread.

3. **Per-user conversion** (audit classification):
   - NIP-11 (cat 2): delete `http_runtime`/`block_on`; run `fetch_http` as a task
     on the shared runtime. The `oneshot`-based `AsyncWait`/`get_async` delivery
     side already exists.
   - AUTH policy/signer, sign-event, engine-signer-waiter, durable-publish
     remote signer (cat 1): `.await` the completion oneshot on the runtime,
     forward the result. Deletes the `auth-release-bridge` thread and the whole
     reserve/release edge.
   - NIP-02 follow action (cat 4): rewrite the blocking worker as an async task
     awaiting `AsyncSubscription::next()` (the #680 twin already exists); no
     engine slot.
   - NIP-46 (cat 2): the session worker becomes an async `select!` loop over the
     transport event stream + request channel; forwarder/switch-relays/result-map
     become async awaits. No per-session executor.

4. **NIP-46 async execution is shared; transport identity remains
   per-session and bounded.** Every session worker, result mapper, and
   switch-relays operation runs on the engine runtime (or the one process-wide
   standalone runtime), so logical session count creates no executor thread.
   Each session retains its own finite transport pool, capped at
   `MAX_NIP46_RELAYS`, because sessions on the same provider relay authenticate
   with different client keys and cannot safely share a URL-deduplicated
   socket. The adversarial-review resolution below records why a global pool
   would either alias those identities or reintroduce a public admission
   refusal.

5. **Remove the capacity-refusal concept, not physical bounds.** Delete
   `nmp-executor` (or reduce it to nothing), `ADAPTER_POOL_CAPACITY`, the
   per-session executors, and every
   `ThreadUnavailable`/`ExecutorSaturated`/`Saturated`/`WaiterSaturated`/census/
   idle-barrier surface from Rust, UniFFI, Swift, Kotlin, snapshots, docs, parity.
   Finite network/body/queue resources use private async backpressure and
   coalescing; callers suspend cancellably and never receive scheduler
   saturation. No aliases.

## Failure-mode split (the `ThreadUnavailable` question)

`ThreadUnavailable` today conflates two things; split them:

- **Real infrastructure-start failure** (engine construction): the reducer
  thread, the fixed pool-bridge thread, the transport pool, and now the async
  runtime build. These stay/become an **engine-start** error
  (`Engine::new` fails). The `auth-release-bridge` and the 32-slot executor
  disappear; the one pool bridge remains fixed transport infrastructure rather
  than a logical-work admission mechanism.
- **Operation-level `ThreadUnavailable`** (NIP-11, AUTH, sign-event,
  engine-signer-waiter, NIP-46 connect, follow-action, FFI): these exist *only*
  because a finite admission pool could refuse. With every logical wait
  caller-owned and physical resources backpressured privately, there is no
  admission refusal to expose, so these are **deleted**. Real operation
  failures keep their domain errors: signer unreachable/rejected
  (`SignerError::Unavailable`/`Rejected`), deadline exceeded
  (`SignerError::Timeout`), relay connection failed. "An internal worker was
  unavailable" is never a domain outcome.

## Cancellation, deadlines, fairness, shutdown

- **Cancellation:** each async task holds a cancel token / its future's `Drop`
  runs the adapter cancel hook; cancelling the task (or dropping the awaiting
  future) releases its correlation and any admitted physical permit
  immediately, or withdraws it while still awaiting admission — no thread is
  held.
- **Deadlines:** `tokio::time::timeout` around each finite await (NIP-11 3s,
  switch-relays 10s, connect/request timeouts as today). Long waits (remote
  signing, durable retry) are deadline-free by design and hold no worker.
- **Fairness:** the multi-thread runtime schedules ready tasks; no operation
  holds a runtime worker while waiting. NIP-11 uses 8 physical flight permits
  plus same-relay shared completion; each NIP-46 session uses finite request,
  control, and event queues whose producers await space. The worker stops
  polling requests at its 64-RPC pending envelope while continuing to process
  responses, cancellation, control changes, and shutdown. Saturating one class
  parks its caller-owned futures and leaves unrelated ready work runnable.
- **Shutdown:** the runtime is shut down deterministically after the reducer
  drains: pending awaits resolve to cancelled/disconnected, tasks abort at their
  next poll, no orphaned worker, no leaked permit, no post-shutdown callback
  (the completion doors are closed).

## Acceptance (see #704 issue) mapped to this design

1. Mixed load: `mixed_load_704` holds 1,000 observations while NIP-11, local
   signing, follow observation/action, and a durable receipt progress on one
   engine. AUTH one-shot ownership, remote-signing waits, NIP-46
   session/switching, and foreign-completion isolation have dedicated
   overlapping falsifiers because they require distinct deterministic
   transports; none exposes admission to refuse.
2. Fairness: saturate NIP-11; signing/AUTH/follow/NIP-46 progress (tasks parked
   at await free the workers).
3. Cancellable admission: N concurrent operations >> runtime worker count all
   remain async tasks. NIP-11 callers beyond the 8-flight physical envelope and
   NIP-46 request producers beyond a finite queue await private admission in
   their caller-owned futures; dropping one withdraws it without a public
   saturation result or a blocked OS thread.
4. NIP-46 scaling 1/10/50/100 over deterministic transports: no per-session
   executor; the retained per-session transport envelope is bounded by
   `MAX_NIP46_RELAYS`, measured, proven, and explained.
5. Long waits hold no worker (they are parked futures).
6. Shutdown determinism with queued/running/pending work.
7. Surface audit: no capacity/thread terminology in public/generated SDK.
8. Before/after resource measurements.

## Review resolutions (adversarial pass)

An adversarial review found three load-bearing gaps; resolved as follows and
folded into the plan above.

1. **Foreign blocking completions run on a fresh per-operation OS thread, not
   the runtime.** The direct-Rust `sign_event_with_completion` contract lets the
   app's completion closure block indefinitely and even call `Engine::join()`
   reentrantly (codified by tests at `runtime/mod.rs:1508-1650`). Running it on
   the shared runtime would stall the fixed workers (starvation) and a reentrant
   `join()` from a worker deadlocks tokio. The signing *wait* is async (no
   thread); when the result arrives, the (possibly-blocking) foreign completion
   is invoked on a **dedicated short-lived OS thread spawned for that one
   in-flight app operation** — O(concurrent app sign requests), which the app
   owns, not an NMP internal admission pool. A completion-thread registry
   preserves the reentrant-`join()` exemption the tests require. FFI signing
   uses the async `NmpSignEventHandle::signed()` future (#693) and needs no
   completion thread.

2. **NIP-46 keeps a per-session transport pool (bounded + explained); only the
   per-session executor is removed.** A single shared signer-transport pool is
   unsound: `Pool::ensure_open` dedups by relay URL (`pool.rs:691`), so two
   sessions on the same provider relay would collide on one handle, and
   per-session NIP-42 AUTH answers with each session's own `client_keys`
   (`nip46.rs:1416`) — one deduped socket cannot authenticate as two sessions;
   escaping dedup with session-keyed sockets makes a global cap refuse session
   N+1, an admission refusal in disguise. So each engine-associated session keeps
   its own transport `Pool` (worker envelope bounded by `MAX_NIP46_RELAYS = 8`,
   `nip46.rs:891`; its permissive localhost allowlist stays signer-pool-only,
   never merged into engine `PoolConfig`). The **per-session `nmp_executor::
   Executor` is removed** — the session worker/forwarder/switch/result-map
   become async tasks on the shared runtime. NIP-46 OS-thread growth is then:
   `0` executor threads + a bounded transport envelope per session. That
   transport-thread scaling is the "unavoidable, explained, proven-bounded"
   scaling #704 permits; it is not a generic executor per session.

3. **The pool-bridge stays a dedicated infra thread.** `pool_bridge_loop`
   (`runtime/mod.rs:1727`) is a permanent blocking crossbeam batch loop that IS
   the transport→reducer backpressure; crossbeam has no async recv, so hosting it
   on the runtime would pin a worker forever. #704 forbids admission *capacity*,
   not O(1) fixed infra threads. Fixed infra after this change: engine reducer
   (1) + pool-bridge (1) + per-session transport pools (bounded) + the shared
   runtime's fixed workers (default 2). The `auth-release-bridge` and the 32-slot
   adapter executor are deleted.

4. **Completion ownership is enum-shaped, not a cluster of lifecycle booleans.**
   The signer and AUTH one-shot doors encode open, resolved, cancelled, and
   receiver-gone states—including whether the single resolver claim was spent—
   as closed enums. Pending-operation handles likewise encode `Pending(cancel)`
   versus `Finished`, so `Drop` cannot infer ownership from a nearby `done`
   boolean. The lock-free AUTH-task and sign-event terminals use `repr(u8)`
   enums as the only values written through their atomics, rather than unrelated
   numeric lifecycle constants. Targeted proofs pin the two formerly ambiguous
   edges: cancellation or consumer drop runs the adapter hook exactly once, a
   late resolver receives typed `ReceiverDropped`, and every later resolver
   receives `AlreadyResolved`.

### Primitive, runtime, shutdown (must-fix)

- **Completion door:** a hand-rolled primitive in `nmp-signer` (no new runtime
  dep): `Mutex<{lifecycle enum, waker}>` + `Condvar`. Blocking `recv`/
  `recv_timeout` via `Condvar` (direct-Rust); async `poll_recv`/`Future` via a
  stored `Waker` (the engine's `.await`). Preserves the typed
  `AlreadyResolved`/`ReceiverDropped` semantics, the **cancel-first bias** of
  `recv_or_cancel`, and Drop-runs-cancel (the hook fires when the awaiting future
  is dropped, including at runtime shutdown — then it runs on the join thread). A
  `Future` needs no runtime, so `nmp-signer` stays runtime-free.
- **Runtime:** `tokio` multi-thread, **2 workers** (one worker makes any
  accidental blocking call a total outage; >2 unjustified for µs-scale work).
  `rt-multi-thread` moves from dev-deps to deps in `nmp-engine`. Every spawned
  task is `Send` (they touch only clones + channels; the `!Send` reducer never
  enters the runtime). D8 is restated: **no runtime types in public API** (kept,
  guarded by a surface scan); the stale `op.rs:190` "no tokio (D8)" comment is
  rewritten.
- **Shutdown order (`EngineThread::join`):** reducer observes `Shutdown`, stops
  spawning, closes adapter owners, and shuts down its transport pool → the pool
  bridge joins → the shared runtime is dropped on a fresh joined shutdown
  thread, never a runtime worker → remaining task futures fire their Drop
  guards, delivering `Err(Cancelled/Disconnected)` to each foreign completion
  exactly once. Post-shutdown `Cmd` posts are harmless (`self_inbox`
  is the existing unbounded std mpsc; adapter results are `Cmd`s on it — this is
  the same inbox, not a new admission queue). Confirm hickory resolver tasks
  terminate on client drop (or accept abort-at-shutdown explicitly).
- **CPU rule (why no blocking scheduler is safe):** every adapter task is
  bounded-CPU per poll (nip44 on ~KB payloads, one schnorr AUTH sign, NIP-11 JSON
  parse). Signature verification stays on transport's verifier threads; redb stays
  on the reducer. **Rule:** any future introduced later with >~1ms CPU per poll or
  a blocking-FS/foreign call triggers the designed-not-assumed bounded scheduler —
  it must not be `spawn`ed onto the shared workers.
- **Delete the `EngineThread::native_tasks()` public executor leak
  (`runtime/mod.rs:1472`)** and the whole `nmp-executor` re-export chain.

## What this is NOT

Not raising 32/12, not hiding a saturation refusal behind another constant, not
renaming `ThreadUnavailable`, not one-executor-per-subsystem/session, not
retries around refusal, not `spawn_blocking` for long waits, not an unbounded
queue. Physical threads
(runtime workers = fixed 2; transport = bounded by relay caps; reducer = 1),
memory, and network remain explicitly bounded.
