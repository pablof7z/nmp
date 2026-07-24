# Finite native task ownership

> **SUPERSEDED in full by
> [`async-observation-handles.md`](async-observation-handles.md) (#680) and
> [`internal-executor-elimination.md`](internal-executor-elimination.md)
> (#704).**
> Observers no longer own an OS thread or an executor slot: query, window,
> diagnostics, receipt, follow, and follow-action delivery are pull-based
> waker-driven async handles that cost **zero** NMP-owned threads. The
> app-visible native-task capacity surface (`max_native_tasks`/`maxNativeTasks`,
> `native_task_census`/`FfiNativeTaskCensus`, `await_native_tasks_idle`, and the
> `ExecutorSaturated`/`executorSaturated` refusal) has been **removed**.
> `nmp-executor` now retains only a proof-oriented thread counter; it owns no
> executor, reservation, or admission pool. Logical adapter work runs on one
> shared async runtime, NIP-11 uses a private eight-flight network/body bound,
> and NIP-46 uses finite backpressured transport queues without per-session
> executors. The text below is the historical #442/#446 record, not current
> architecture.

Issue #446 closes the remaining per-stream/per-operation OS-thread growth left
after #442 bounded transport and verifier ownership.

## Why this is not a worker pool

Native observer bridges and remote-signer waiters block on one receiver for
their whole lifetime. A conventional fixed worker pool would let the first
long-lived drains occupy every worker while later, already-accepted streams
sat in a queue that could never run. That would hide accepted facts and violate
`bounded-delivery.md`.

`nmp-executor` therefore has no task queue. Each engine owns one executor with
a finite admission count (`max_native_tasks`, default 12). Admission reserves
one immediately-startable task slot. A native bridge starts and registers its
OS thread before the query, receipt, reattachment, or action transfers
ownership; logical saturation returns `ExecutorSaturated { component,
capacity }`, while an OS spawn refusal remains the distinct
`ThreadUnavailable { component, reason }`. An accepted durable signer request
that encounters saturation retains its obligation and receives the existing
retryable `SignerError::Unavailable` result.

The default is deliberately not derived from host CPU count. Twelve covers a
representative steady composed app peak of nine tasks -- query, demand, the
two-task FFI follow path, receipt, plus an established NIP-46 session and one
in-flight sign operation. It also covers the eleven-task transient in which
the connection and `switch_relays` tasks have not yet reaped when signing
starts, plus one concurrent NIP-11 acquisition. That NIP-11 flight consumes the
twelfth slot rather than owning a hidden pool; another flight is synchronously
refused without publishing a request or altering any accepted durable
obligation. Eight fails the measured signing role count. A host with measured
demand may raise the ceiling explicitly.

## Ordering and ownership

One admitted task exclusively drains one receiver, preserving that receiver's
FIFO order. There is no cross-stream ordering claim. Query and diagnostics
handles retain their existing idempotent cancellation. Receipt streams remain
durable engine facts; cancelling the native drain detaches observation and
does not cancel the obligation. Every native observer loop invokes its one
terminal callback after its receiver disconnects.

The same engine executor owns:

- FFI row, demand, follow, receipt, reattachment, and diagnostics bridges;
- NIP-02 follow projection and action workers;
- engine-owned NIP-11 DNS/HTTP flights;
- engine remote-signer result waiters;
- engine-associated NIP-46 connection, session, result-map, switch-relay, and
  event-forwarding tasks.

Standalone direct-Rust NIP-46 construction preserves its public API and gives
each independent session one session-owned finite executor. Engine-associated
native construction injects the engine executor instead, so it never creates
a hidden second executor.

## Shutdown and exact accounting

The executor owns one fallibly-created reaper. Completed task handles remain
charged until that reaper joins them; only then does the admission count fall.
Shutdown has two phases:

1. refuse new work, invalidate unstarted reservations, and invoke every
   registered cancellation/producer-teardown action;
2. join every admitted task and the reaper, then expose exact
   `admitted == 0 && running == 0` accounting through the lifecycle census.

A callback that initiates engine shutdown cannot wait for its own join. It
performs phase one and returns; the reaper joins that callback task after it
returns, and an external lifecycle barrier proves phase two. No timeout or
polling establishes the baseline.

## Native OS-thread envelope

For one native engine without NIP-46 sessions, the current worst-case envelope
is explicit:

- engine runtime + pool bridge: 2;
- verifier workers: 2 (1 on the sequential wasm path);
- transport translator + relay reaper: 2;
- live relay workers: at most `max_relays`;
- charged retiring relay workers: at most `max_relays`;
- native-task reaper: 1;
- admitted native task threads: at most `max_native_tasks`.

That is `7 + 2 * max_relays + max_native_tasks` native threads on the ordinary
native path, rather than an unbounded stream-proportional count.

NIP-11 adds no term to that formula. Each flight is already one of the
`max_native_tasks` admitted threads and runs a current-thread Tokio reactor on
that owned thread; it creates no Tokio worker pool or blocking DNS thread.
Hickory performs asynchronous DNS, and executor cancellation races the whole
DNS/request/status/body operation under a three-second total deadline.

An engine-associated NIP-46 session adds one separately owned transport pool.
Its pool configuration sets `R46 = MAX_NIP46_RELAYS = 8`, the same bound
enforced while parsing invitations/bunker URIs and applying `switch_relays`.
Its conservative native envelope is `4 + 2 * R46 = 20`: two verifier workers,
translator, relay reaper, and at most eight live plus eight charged-retiring
relay workers.
Every supported engine-associated session pool continuously consumes at least
two engine-executor slots (session worker plus connection/event-forwarding
work), so `S <= floor(max_native_tasks / 2)`. The odd-capacity falsifier
constructs two forwardable sessions in five slots, then proves the third
session's mandatory event-forwarder reservation is refused. The conservative
whole-engine formula including those pools is therefore
`7 + 2 * max_relays + max_native_tasks + 20 * S`. At default limits the
ordinary no-session case is 39. One steady one-relay session is 44 (four pool
infrastructure threads plus one live relay); one max-relay session at its full
live-plus-retiring envelope is 59. The adversarial six-session extreme is 159;
it is an explicit refusal boundary for hostile/misconfigured use, not a safe
operating target or expected mobile footprint. Lowering either host knob
lowers the corresponding term.

Direct-Rust NIP-46 sessions constructed independently of an engine are not
part of that engine formula. Each owns a separate executor and transport pool,
so each is individually finite, but the number an application constructs is
application-owned; this design makes no false process-global bound for those
independent sessions.

The demo and BDD harness retain their fixed app/test-owned forwarding threads;
they are not caller-created SDK bridge mechanisms. Transport, verifier,
runtime, and executor/reaper spawns are the intentional production survivors.
