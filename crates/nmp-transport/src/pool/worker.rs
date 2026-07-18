//! The per-relay worker thread: a `mio`-driven blocking-socket readiness
//! loop that dials, reconnects (backoff+jitter), keeps the connection alive,
//! and ferries frames to/from the [`super::inner::PoolInner`] translator.
//!
//! HARVEST source: the old repo's `crates/nmp-network/src/relay_worker/`
//! (`mod.rs`, `io_ready.rs`, `socket_io.rs`) — the mio `Poll`/`Waker`
//! readiness pattern (edge-triggered read-drain-unconditionally lesson),
//! the reconnect/backoff/keepalive integration, and the reconnect-preamble
//! replay-at-front-of-queue mechanism are carried over. Two things are
//! deliberately simplified relative to the harvested source:
//!
//! 1. **One thread per worker, not two.** The old repo runs a small
//!    "forward_commands" proxy thread per worker solely to trigger the
//!    `mio::Waker` on every enqueued command (a layering artifact of that
//!    codebase). Here, [`super::inner::PoolInner`] holds the waker directly
//!    (via [`WorkerHandle`]) and wakes it immediately after enqueueing —
//!    no proxy thread needed.
//! 2. **Generation bumps on every reconnect, not only on an explicit
//!    pool-level reopen.** The old repo's worker generation is fixed for the
//!    worker's whole lifetime; only `Pool::close` + a fresh `ensure_open`
//!    bumps it. M3's plan (§3.2, tests 6/7) calls for the stronger
//!    invariant: ANY reconnect — including an automatic mid-session one —
//!    must invalidate stale handles. See [`pack_generation`] for how this is
//!    made safe without an extra thread of coordination with the pool.

use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use nostr::RelayUrl;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

use crate::backoff;
use crate::keepalive::{
    apply_resume_gap, KeepaliveAction, KeepaliveState, SuspendGapDetector, SUSPEND_GAP_THRESHOLD,
};

use super::connect::{open_relay_socket, RelaySocket};
use super::frame::classify_message;
use super::spawn::ThreadSpawner;
use super::{AttemptCorrelation, EphemeralSendOutcome, HandoffResult, RelayFrame};
use super::{ThreadRole, ThreadSpawnError};

const SOCKET: Token = Token(0);
const CONTROL: Token = Token(1);
/// Per-protected-generation transport-owned first-frame observation contract.
/// Unlike an engine deadline, the relay worker remains the sole producer
/// throughout this interval, so any frame it observes is enqueued before
/// completion. Public generations do not pay this interval.
const INITIAL_READ_OBSERVATION_WINDOW: Duration = Duration::from_millis(250);

/// Command the pool pushes to one relay worker.
pub(super) enum WorkerCommand {
    Send(String),
    Shutdown,
    /// Frames replayed at the front of the outbound queue on every
    /// (re)connect, before any newly-enqueued `Send`. Registered by the
    /// engine after observing `Connected` so the current live subscriptions
    /// survive a reconnect without the engine racing the socket.
    SetReconnectPreamble(Vec<String>),
    /// Open the ordinary outbound gate for one exact connected generation
    /// after the consumer has applied its ordered initial-read edge.
    ReleaseInitialRead {
        generation: u64,
    },
    /// A durable `EVENT` handoff (issue #93), scoped to the generation the
    /// caller observed when it submitted this. Tracked in a queue entirely
    /// separate from the plain `Send` deque above: it never survives a
    /// reconnect, and it is the ONLY command that produces a
    /// [`WorkerEventKind::EventHandoff`] result. `generation` is checked
    /// against the worker's OWN current `pack_generation(worker_id, attempt)`
    /// the moment this is drained from the command channel -- a command
    /// that raced a reconnect (queued for generation G, drained after the
    /// worker already moved to G+1) is resolved `NotHandedOff` immediately,
    /// never silently attempted against the new connection.
    SendDurable {
        generation: u64,
        correlation: AttemptCorrelation,
        frame: String,
    },
    /// A one-shot connection-scoped handoff. This never enters `pending`,
    /// `preamble`, or the durable EVENT queues. The completion is resolved
    /// only by this exact generation's write+flush boundary, or unavailable
    /// when the command is stale / the generation ends first.
    SendEphemeral {
        generation: u64,
        frame: String,
        completion: EphemeralCompletion,
    },
}

pub(super) struct EphemeralCompletion {
    callback: Option<Box<dyn FnOnce(EphemeralSendOutcome) + Send + 'static>>,
}

impl EphemeralCompletion {
    pub(super) fn new(callback: impl FnOnce(EphemeralSendOutcome) + Send + 'static) -> Self {
        Self {
            callback: Some(Box::new(callback)),
        }
    }

    fn resolve(mut self, outcome: EphemeralSendOutcome) {
        if let Some(callback) = self.callback.take() {
            // A consumer callback is an off-transport notification edge. A
            // panic there must not kill the relay worker and strand unrelated
            // traffic or future exact completions.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback(outcome);
            }));
        }
    }

    pub(super) fn disarm(mut self) {
        self.callback.take();
    }
}

impl Drop for EphemeralCompletion {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback(EphemeralSendOutcome::Unavailable);
            }));
        }
    }
}

struct EphemeralFrame {
    frame: String,
    completion: EphemeralCompletion,
}

/// What happened, tagged with the worker's packed `(worker_id, attempt)`
/// generation at the time it happened.
pub(super) enum WorkerEventKind {
    /// Emitted by the pool's single retirement reaper only after the relay
    /// OS thread has exited and its join completed.
    Retired {
        worker_id: u32,
    },
    Connected,
    /// This protected generation completed its initial socket observation and
    /// final nonblocking read-drain. Any observed frame was emitted before
    /// this edge on the same FIFO worker event stream. Public generations skip
    /// the handshake and never emit this marker.
    InitialReadCompleted,
    /// `permanent` mirrors [`backoff::is_permanent_error`] (HTTP 401/403):
    /// the pool must not keep auto-reconnecting on its own. `retry_in` is
    /// the (pre-jitter) delay before the next reconnect attempt, `None` for
    /// a permanent failure (there won't be one).
    Failed {
        message: String,
        permanent: bool,
        retry_in: Option<Duration>,
    },
    Frame(RelayFrame),
    /// The one, ever, resolution of a `SendDurable` command's
    /// `AttemptCorrelation` (issue #93). See [`super::PoolEvent::EventHandoff`]
    /// for the delivery contract (never gated on generation/slot staleness
    /// at the pool-translator level).
    EventHandoff {
        correlation: AttemptCorrelation,
        result: HandoffResult,
    },
}

pub(super) struct WorkerEvent {
    pub(super) slot: u32,
    pub(super) generation: u64,
    pub(super) kind: WorkerEventKind,
}

/// Pack a worker instance id (bumped by the pool on every fresh spawn — a
/// brand-new open OR an explicit reopen after `close`) with a per-worker
/// local reconnect-attempt counter (bumped by the worker itself on every
/// internal reconnect) into one comparable generation.
///
/// This is the generation-safety scheme's core: two different worker
/// *instances* (before/after an explicit close+reopen) can never collide —
/// `worker_id` occupies the high bits — and within one worker instance every
/// reconnect strictly increases the value, because `attempt` only ever
/// increments. The pool's translator can therefore validate every event with
/// a single `u64` compare against the slot's currently-accepted generation;
/// see `pool::inner::apply_worker_event`.
pub(super) fn pack_generation(worker_id: u32, attempt: u32) -> u64 {
    (u64::from(worker_id) << 32) | u64::from(attempt)
}

/// Extract the worker-instance id a packed generation was produced by.
/// Two different worker instances (before/after an explicit close+reopen)
/// never share a `worker_id`, so this is the check that tells apart a
/// zombie event from a just-superseded worker from a legitimate event of
/// the currently active one.
pub(super) fn worker_id_of(generation: u64) -> u32 {
    (generation >> 32) as u32
}

/// Handle the pool keeps per slot to talk to its worker thread: a command
/// channel plus a shared slot for whatever `mio::Waker` the worker currently
/// has registered (installed fresh each time the worker builds a new
/// `RelayPoller` for a freshly opened socket; cleared while the worker is in
/// its backoff wait between sockets, where it just blocks on `recv_timeout`).
pub(super) struct WorkerHandle {
    command_tx: SyncSender<WorkerCommand>,
    /// Out-of-band terminal signal (issue #506). Retirement must NEVER travel
    /// through the bounded `command_tx` data lane: a caller retires a worker
    /// while holding the pool `Mutex<PoolInner>` (every `retire` call site
    /// does), so a blocking send here — if the bounded command queue were
    /// full and the worker were transitively blocked draining it (its own
    /// `event_tx.send` waits on the translator, which needs that same pool
    /// lock) — would be a whole-pool circular-wait deadlock. This atomic is
    /// the source of truth the worker checks at EVERY drain/wait point; it is
    /// set (and the worker woken) without ever touching the data queue.
    shutdown: Arc<AtomicBool>,
    waker: Arc<Mutex<Option<Waker>>>,
    join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    /// Enqueue `command` and wake the worker if it is currently parked in
    /// `mio::Poll::poll`. Returns `false` if the worker thread is already
    /// gone (channel disconnected) OR — issue #506's HIGH finding — if the
    /// bounded outbound queue is currently full: a stalled-but-connected
    /// relay (TCP send window full, so the worker's `flush_writes` keeps
    /// returning `Blocked`) must surface backpressure to the caller instead
    /// of growing this queue without bound. `Pool::send`/`send_durable`
    /// already have a typed "not handed off" outcome for exactly this case;
    /// this is the seam that makes it reachable.
    pub(super) fn push(&self, command: WorkerCommand) -> bool {
        self.try_push(command).is_ok()
    }

    /// Enqueue while preserving command ownership on a refused enqueue.
    /// BOTH refusal shapes — a full bounded queue (the #506 backpressure
    /// signal) and a gone worker thread (channel disconnected) — return the
    /// command so exact ephemeral callers can disarm their callback before
    /// reporting a synchronous refusal. The mio waker fires only on a
    /// successful enqueue.
    pub(super) fn try_push(&self, command: WorkerCommand) -> Result<(), WorkerCommand> {
        self.command_tx
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(command) | TrySendError::Disconnected(command) => command,
            })?;
        self.wake();
        Ok(())
    }

    /// Wake the worker if it is parked in `mio::Poll::poll` for a live
    /// socket. During the backoff wait between sockets the waker slot is
    /// empty (the worker blocks on `command_rx.recv_timeout` there instead —
    /// see [`RelayPoller`]'s doc); the retirement nudge below handles that
    /// case, so a no-op here is correct, not a missed wake.
    fn wake(&self) {
        if let Ok(guard) = self.waker.lock() {
            if let Some(waker) = guard.as_ref() {
                let _ = waker.wake();
            }
        }
    }

    /// Request shutdown and return the worker's join handle. NON-BLOCKING and
    /// lock-safe by construction — this is the whole point of the #506 Fix 2
    /// correction.
    ///
    /// Every caller runs while holding the pool `Mutex<PoolInner>`
    /// (`PoolInner::close`/`shutdown` and the permanent-`Failed` arm of the
    /// translator, which locks `PoolInner` to apply the event). So retirement
    /// must not perform ANY operation that could block on the bounded data
    /// queue: doing so risks a cross-channel circular wait (this thread waits
    /// on a full `command_tx`; the worker that would drain it is blocked on a
    /// full `event_tx`; the translator that would drain THAT needs the pool
    /// lock this thread is holding). Instead:
    ///
    /// 1. Set the terminal `shutdown` atomic — the source of truth the worker
    ///    re-checks at every drain/wait point.
    /// 2. Wake the mio waker so a worker parked in `poll` returns at once.
    /// 3. Best-effort `try_send(Shutdown)` — NEVER a blocking send — purely to
    ///    nudge a worker parked in a `command_rx.recv`/`recv_timeout` (the
    ///    backoff wait or the permanent-failure drain, where the mio waker is
    ///    inactive). If the queue is full this `try_send` is simply dropped,
    ///    and that is safe: a full queue means `recv` already has a command to
    ///    return, so the worker wakes on its own and observes the atomic on
    ///    the very next loop iteration. A dropped nudge therefore costs at
    ///    most one already-queued command of latency, never correctness.
    ///
    /// All three steps are non-blocking, so `retire` cannot stall the pool
    /// lock. The returned `JoinHandle` is joined LATER, off-lock, by the
    /// retirement reaper (`spawn_reaper`).
    pub(super) fn retire(mut self) -> JoinHandle<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake();
        // Best-effort nudge for a recv-parked worker; dropped-if-full is safe
        // (see the doc above). Deliberately `try_send`, never `send`.
        let _ = self.command_tx.try_send(WorkerCommand::Shutdown);
        self.join
            .take()
            .expect("a live relay worker owns exactly one join handle")
    }
}

/// Spawn the worker thread for one relay slot.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn(
    slot: u32,
    worker_id: u32,
    url: RelayUrl,
    initial_gate_required: bool,
    event_tx: SyncSender<WorkerEvent>,
    keepalive_idle: Duration,
    keepalive_pong_timeout: Duration,
    reconnect_delay_initial: Duration,
    reconnect_jitter_max: Duration,
    command_queue_capacity: usize,
    allowed_local_hosts: Arc<BTreeSet<String>>,
    committed_observations: Arc<super::committed_observations::CommittedObservationCache>,
    spawner: &dyn ThreadSpawner,
) -> Result<WorkerHandle, ThreadSpawnError> {
    // Bounded (issue #506's HIGH finding): this was the one unbounded queue
    // in the whole pool. `command_queue_capacity` is `PoolConfig::
    // command_queue_capacity`, already normalized to at least 1 by the
    // caller (`PoolInner::spawn_worker`) the same way every other queue
    // knob is.
    let (command_tx, command_rx) = mpsc::sync_channel::<WorkerCommand>(command_queue_capacity);
    let waker_slot: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
    let waker_for_thread = Arc::clone(&waker_slot);
    // Out-of-band terminal signal (issue #506 Fix 2). Shared with the
    // `WorkerHandle` the pool keeps; `retire` sets it without ever touching
    // the bounded `command_tx`, and the worker re-checks it at every
    // drain/wait point so shutdown never depends on the data queue.
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_thread = Arc::clone(&shutdown);
    let join = spawner
        .spawn(
            thread::Builder::new().name(format!("nmp-transport-relay-{slot}")),
            Box::new(move || {
                run_worker(
                    slot,
                    worker_id,
                    url,
                    initial_gate_required,
                    event_tx,
                    command_rx,
                    waker_for_thread,
                    &shutdown_for_thread,
                    keepalive_idle,
                    keepalive_pong_timeout,
                    reconnect_delay_initial,
                    reconnect_jitter_max,
                    &allowed_local_hosts,
                    &committed_observations,
                );
            }),
        )
        .map_err(|error| ThreadSpawnError {
            role: ThreadRole::RelayWorker,
            reason: error.to_string(),
        })?;
    Ok(WorkerHandle {
        command_tx,
        shutdown,
        waker: waker_slot,
        join: Some(join),
    })
}

/// Read the out-of-band retirement signal. Every `command_rx.recv`/
/// `recv_timeout`/`try_recv` wait in this module pairs with a check of this
/// so a retired worker exits promptly regardless of the bounded data queue's
/// occupancy (issue #506 Fix 2).
fn shutdown_requested(shutdown: &AtomicBool) -> bool {
    shutdown.load(Ordering::SeqCst)
}

enum ConnectedOutcome {
    /// Explicit `Shutdown` command processed — the worker returns for good.
    Shutdown,
    /// Socket dropped (error, peer close, or keepalive timeout) — the caller
    /// applies backoff and redials.
    Reconnect { message: String, permanent: bool },
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    slot: u32,
    worker_id: u32,
    url: RelayUrl,
    initial_gate_required: bool,
    event_tx: SyncSender<WorkerEvent>,
    command_rx: Receiver<WorkerCommand>,
    waker_slot: Arc<Mutex<Option<Waker>>>,
    shutdown: &AtomicBool,
    keepalive_idle: Duration,
    keepalive_pong_timeout: Duration,
    reconnect_delay_initial: Duration,
    reconnect_jitter_max: Duration,
    allowed_local_hosts: &BTreeSet<String>,
    committed_observations: &super::committed_observations::CommittedObservationCache,
) {
    let relay_scope = super::committed_observations::RelayScope::new(&url);
    let mut pending: VecDeque<String> = VecDeque::new();
    let mut preamble: Vec<String> = Vec::new();
    // Durable EVENT tracking (issue #93): entirely separate from `pending`
    // above, and NEVER carried across a reconnect — each `run_connected`
    // call starts these two empty and `resolve_generation_end` drains both
    // (firing `NotHandedOff`/`Ambiguous`) the instant that call returns, no
    // matter which internal path produced the outcome.
    let mut durable: VecDeque<(AttemptCorrelation, String)> = VecDeque::new();
    let mut write_accepted: Vec<AttemptCorrelation> = Vec::new();
    // Ephemeral (exact-generation) lane: same never-carried-across-a-
    // reconnect discipline as the durable pair above, resolved
    // `Unavailable` instead of `NotHandedOff`/`Ambiguous`.
    let mut ephemeral: VecDeque<EphemeralFrame> = VecDeque::new();
    let mut ephemeral_write_accepted: Vec<EphemeralCompletion> = Vec::new();
    let mut attempt: u32 = 0;
    let mut backoff_delay = reconnect_delay_initial;

    loop {
        // Retired between sockets (e.g. during a backoff wait that returned to
        // reconnect): never dial again. Settle any durables still queued in the
        // narrow window between `wait_before_reconnect` returning and this
        // re-check before exiting (#506 Fix 2) — a `Queued` correlation must
        // never be abandoned on retirement. `EventHandoff` delivery ignores the
        // tag generation (`apply_worker_event` resolves it before any slot
        // lookup), so this attempt's generation is a fine label.
        if shutdown_requested(shutdown) {
            resolve_queued_durables_on_shutdown(
                &command_rx,
                &event_tx,
                slot,
                pack_generation(worker_id, attempt),
            );
            return;
        }
        let generation = pack_generation(worker_id, attempt);
        match open_relay_socket(url.as_str(), allowed_local_hosts) {
            Ok(mut socket) => {
                let connected_at = Instant::now();
                // REQ-before-EVENT: inject the registered preamble at the
                // FRONT of the outbound queue before any newly-posted Send
                // commands can be drained.
                for frame in preamble.iter().rev() {
                    pending.push_front(frame.clone());
                }
                if event_tx
                    .send(WorkerEvent {
                        slot,
                        generation,
                        kind: WorkerEventKind::Connected,
                    })
                    .is_err()
                {
                    return;
                }
                let mut keepalive =
                    KeepaliveState::new(Instant::now(), keepalive_idle, keepalive_pong_timeout);
                // Resume-gap heuristic (issue #4): a fresh detector per
                // connected generation, seeded from wall-clock `now` at
                // connect time so a suspension DURING the reconnect/backoff
                // wait doesn't retroactively look like a gap the instant the
                // new socket comes up.
                //
                // `SUSPEND_GAP_THRESHOLD`'s safety margin (never firing on an
                // ordinary idle wait) is only sound relative to whatever
                // idle/pong timeouts THIS pool is actually configured with --
                // its doc assumes the production defaults. A `PoolConfig`
                // override that pushes either past the threshold would let a
                // legitimate idle wait masquerade as a resume gap; debug
                // builds catch that misconfiguration here rather than
                // silently changing ping cadence in production.
                debug_assert!(
                    SUSPEND_GAP_THRESHOLD > keepalive_idle
                        && SUSPEND_GAP_THRESHOLD > keepalive_pong_timeout,
                    "SUSPEND_GAP_THRESHOLD ({SUSPEND_GAP_THRESHOLD:?}) must exceed the configured \
                     keepalive idle/pong timeouts ({keepalive_idle:?}/{keepalive_pong_timeout:?}), \
                     or an ordinary idle wait under this config can spuriously trip the resume-gap \
                     heuristic"
                );
                let mut suspend_gap =
                    SuspendGapDetector::new(SystemTime::now(), SUSPEND_GAP_THRESHOLD);
                let outcome = run_connected(
                    slot,
                    generation,
                    &event_tx,
                    &command_rx,
                    &waker_slot,
                    shutdown,
                    &mut pending,
                    &mut socket,
                    &mut keepalive,
                    &mut suspend_gap,
                    &mut preamble,
                    &mut durable,
                    &mut write_accepted,
                    &mut ephemeral,
                    &mut ephemeral_write_accepted,
                    initial_gate_required,
                    relay_scope,
                    committed_observations,
                );
                match outcome {
                    ConnectedOutcome::Shutdown => return,
                    ConnectedOutcome::Reconnect { message, permanent } => {
                        let retry_in = (!permanent).then(|| {
                            backoff::advance(&mut backoff_delay, Some(connected_at.elapsed()))
                        });
                        let _ = event_tx.send(WorkerEvent {
                            slot,
                            generation,
                            kind: WorkerEventKind::Failed {
                                message,
                                permanent,
                                retry_in,
                            },
                        });
                        if permanent {
                            drain_permanently_disconnected(
                                &command_rx,
                                &event_tx,
                                shutdown,
                                slot,
                                generation,
                            );
                            return;
                        }
                        let base = retry_in.expect("retry_in set above for non-permanent");
                        let delay = backoff::jittered(base, url.as_str(), reconnect_jitter_max);
                        attempt = attempt.wrapping_add(1);
                        if !wait_before_reconnect(
                            &command_rx,
                            &mut pending,
                            &mut preamble,
                            delay,
                            &event_tx,
                            shutdown,
                            slot,
                            pack_generation(worker_id, attempt),
                        ) {
                            return;
                        }
                    }
                }
            }
            Err(message) => {
                let permanent = backoff::is_permanent_error(&message);
                let retry_in = (!permanent).then(|| backoff::advance(&mut backoff_delay, None));
                if event_tx
                    .send(WorkerEvent {
                        slot,
                        generation,
                        kind: WorkerEventKind::Failed {
                            message,
                            permanent,
                            retry_in,
                        },
                    })
                    .is_err()
                {
                    return;
                }
                if permanent {
                    drain_permanently_disconnected(
                        &command_rx,
                        &event_tx,
                        shutdown,
                        slot,
                        generation,
                    );
                    return;
                }
                let base = retry_in.expect("retry_in set above for non-permanent");
                let delay = backoff::jittered(base, url.as_str(), reconnect_jitter_max);
                attempt = attempt.wrapping_add(1);
                if !wait_before_reconnect(
                    &command_rx,
                    &mut pending,
                    &mut preamble,
                    delay,
                    &event_tx,
                    shutdown,
                    slot,
                    pack_generation(worker_id, attempt),
                ) {
                    return;
                }
            }
        }
    }
}

/// Keep the worker's command receiver alive after a permanent connection
/// failure until the pool explicitly retires the slot. This closes the race
/// between `Pool::send_durable` successfully enqueueing a command and the
/// worker returning after its final dial/session failure: every command the
/// sender accepted before the pool observed the permanent failure is
/// drained and resolved `NotHandedOff`, while commands submitted after the
/// health transition are rejected synchronously by `PoolInner`.
///
/// Terminates on the out-of-band `shutdown` atomic (issue #506 Fix 2), NOT
/// solely on a queued `Shutdown` command: `retire`'s nudge `try_send` is
/// best-effort and may be dropped if the bounded command queue is full, so
/// the atomic — re-checked before every blocking `recv` and after every
/// command — is the authoritative exit. When the atomic is set, `recv`
/// either already has the dropped-nudge's would-be slot's worth of data to
/// return (queue was full) or the nudge landed; either way this loop wakes
/// and observes the flag rather than blocking forever.
fn drain_permanently_disconnected(
    command_rx: &Receiver<WorkerCommand>,
    event_tx: &SyncSender<WorkerEvent>,
    shutdown: &AtomicBool,
    slot: u32,
    generation: u64,
) {
    loop {
        if shutdown_requested(shutdown) {
            // Retired: settle any durables still queued before exiting (#506
            // Fix 2). Without this the worst case — flag observed on the first
            // check, zero commands drained — abandons the whole queued durable
            // burst.
            resolve_queued_durables_on_shutdown(command_rx, event_tx, slot, generation);
            return;
        }
        match command_rx.recv() {
            Ok(WorkerCommand::SendDurable { correlation, .. }) => resolve_correlation(
                event_tx,
                slot,
                generation,
                correlation,
                HandoffResult::NotHandedOff,
            ),
            Ok(WorkerCommand::SendEphemeral { completion, .. }) => {
                completion.resolve(EphemeralSendOutcome::Unavailable);
            }
            Ok(
                WorkerCommand::Send(_)
                | WorkerCommand::SetReconnectPreamble(_)
                | WorkerCommand::ReleaseInitialRead { .. },
            ) => {}
            Ok(WorkerCommand::Shutdown) | Err(_) => return,
        }
    }
}

/// Fire the one, ever, [`WorkerEventKind::EventHandoff`] for `correlation`.
/// The receiving end is `[super::inner::apply_worker_event`], which
/// delivers every `EventHandoff` unconditionally (never gated on slot/
/// generation staleness) — losing this send (a disconnected `event_tx`,
/// meaning the whole pool is gone) is the only way it's ever NOT delivered,
/// which is the same fate every other `WorkerEvent` already has.
fn resolve_correlation(
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
    correlation: AttemptCorrelation,
    result: HandoffResult,
) {
    let _ = event_tx.send(WorkerEvent {
        slot,
        generation,
        kind: WorkerEventKind::EventHandoff {
            correlation,
            result,
        },
    });
}

/// Drain whatever commands are still queued at a flag-observed exit and
/// resolve every durable `EVENT` among them `NotHandedOff` (issue #506 Fix 2,
/// upholding issue #93).
///
/// A retired worker exits via the out-of-band `shutdown` atomic, which is
/// checked at the TOP of each drain/wait loop — so it can return with
/// `SendDurable` commands STILL sitting in the bounded command channel (worst
/// case: `drain_permanently_disconnected` sees the flag on its very first
/// check and has drained zero). Each of those commands already returned
/// [`super::DurableSendOutcome::Queued`] to the engine, whose contract
/// (`Pool::send_durable`) is that the worker now OWNS the attempt and WILL
/// emit exactly one [`super::PoolEvent::EventHandoff`]. If the worker returned
/// without draining them, `command_rx` would drop and those correlations would
/// be lost forever — silently violating #93's resolve-exactly-once invariant.
/// [`resolve_generation_end`] only drains the worker-LOCAL `durable`/
/// `write_accepted` state, never the channel, so this is the one place the
/// channel remainder is settled.
///
/// Deadlock-safe (the whole point of #506 Fix 2): this runs only AFTER
/// `retire` set the flag, and `retire` is non-blocking and has already taken
/// `state.worker` out of the slot, so `command_tx_for` refuses every new
/// producer — the channel can only drain here, never refill — and the
/// resolving `event_tx.send`s complete because the translator is no longer
/// blocked behind the (already-released) pool lock.
fn resolve_queued_durables_on_shutdown(
    command_rx: &Receiver<WorkerCommand>,
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
) {
    loop {
        match command_rx.try_recv() {
            Ok(WorkerCommand::SendDurable { correlation, .. }) => resolve_correlation(
                event_tx,
                slot,
                generation,
                correlation,
                HandoffResult::NotHandedOff,
            ),
            // A channel-resident ephemeral handoff is settled with the same
            // explicit discipline as a durable one — its `Drop` backstop
            // would fire anyway, but retirement must never rely on it.
            Ok(WorkerCommand::SendEphemeral { completion, .. }) => {
                completion.resolve(EphemeralSendOutcome::Unavailable);
            }
            // Non-durable traffic (`Send`/`SetReconnectPreamble`) and the
            // `Shutdown` nudge itself carry no correlation to resolve; simply
            // discard them.
            Ok(_) => {}
            // `Empty` (fully drained) or `Disconnected` — nothing more to
            // settle.
            Err(_) => return,
        }
    }
}

/// Resolve every durable `EVENT` still tracked for this generation the
/// instant it ends (issue #93's core invariant — nothing is ever silently
/// carried into the next connection):
/// - `durable` (still queued, never reached `socket.write()`) resolves
///   `NotHandedOff` — provably safe to resubmit under a fresh generation.
/// - `write_accepted` (its own `write()` succeeded, but the shared flush
///   that would confirm it never completed before this generation ended)
///   resolves `Ambiguous` — the bytes MAY have reached the relay, so
///   nothing may treat it as a fresh, never-attempted send.
///
/// Ephemeral (exact-generation) handoffs share the same boundary but not
/// the same vocabulary: queued and write-accepted-but-unflushed entries
/// alike resolve `Unavailable` — the frame's authority died with the
/// generation, so there is no ambiguity worth reporting.
fn resolve_generation_end(
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
    ephemeral: &mut VecDeque<EphemeralFrame>,
    ephemeral_write_accepted: &mut Vec<EphemeralCompletion>,
) {
    for (correlation, _frame) in durable.drain(..) {
        resolve_correlation(
            event_tx,
            slot,
            generation,
            correlation,
            HandoffResult::NotHandedOff,
        );
    }
    for correlation in write_accepted.drain(..) {
        resolve_correlation(
            event_tx,
            slot,
            generation,
            correlation,
            HandoffResult::Ambiguous,
        );
    }
    for pending in ephemeral.drain(..) {
        pending
            .completion
            .resolve(EphemeralSendOutcome::Unavailable);
    }
    for completion in ephemeral_write_accepted.drain(..) {
        completion.resolve(EphemeralSendOutcome::Unavailable);
    }
}

/// Wait for the reconnect delay to elapse, buffering incoming `Send`
/// commands and updating `preamble` if `SetReconnectPreamble` arrives
/// (stored, never discarded — a fast-flap registration during the wait must
/// still apply to the next connect). A durable `EVENT` (`SendDurable`)
/// resolves `NotHandedOff` immediately — there is no live connection to
/// queue it against during backoff, and buffering it here would be exactly
/// the hidden carry-over queue issue #93 removes.
#[allow(clippy::too_many_arguments)]
fn wait_before_reconnect(
    command_rx: &Receiver<WorkerCommand>,
    pending: &mut VecDeque<String>,
    preamble: &mut Vec<String>,
    delay: Duration,
    event_tx: &SyncSender<WorkerEvent>,
    shutdown: &AtomicBool,
    slot: u32,
    generation: u64,
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        // Authoritative terminal check (issue #506 Fix 2): a retirement during
        // the backoff wait sets this atomic and nudges the channel; the mio
        // waker is inactive here (no live socket), so the atomic — checked
        // before every blocking `recv_timeout` and after every command — is
        // what guarantees a prompt exit rather than sleeping out `remaining`.
        if shutdown_requested(shutdown) {
            // Settle any durables still queued before exiting (#506 Fix 2), so
            // a retirement never abandons a `Queued` correlation.
            resolve_queued_durables_on_shutdown(command_rx, event_tx, slot, generation);
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        match command_rx.recv_timeout(remaining) {
            Ok(WorkerCommand::Send(text)) => pending.push_back(text),
            Ok(WorkerCommand::SetReconnectPreamble(frames)) => *preamble = frames,
            Ok(WorkerCommand::ReleaseInitialRead { .. }) => {}
            Ok(WorkerCommand::SendDurable { correlation, .. }) => {
                resolve_correlation(
                    event_tx,
                    slot,
                    generation,
                    correlation,
                    HandoffResult::NotHandedOff,
                );
            }
            Ok(WorkerCommand::SendEphemeral { completion, .. }) => {
                completion.resolve(EphemeralSendOutcome::Unavailable);
            }
            Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Thin wrapper: run one connected generation, then unconditionally resolve
/// whatever durable EVENT state is still outstanding the instant it ends —
/// regardless of WHICH internal path produced the outcome. Centralizing the
/// resolution here (once) rather than at every internal early-return inside
/// [`run_connected_inner`] is what makes "every generation end resolves
/// everything, exactly once" true by construction instead of by care at
/// each call site.
#[allow(clippy::too_many_arguments)]
fn run_connected(
    slot: u32,
    generation: u64,
    event_tx: &SyncSender<WorkerEvent>,
    command_rx: &Receiver<WorkerCommand>,
    waker_slot: &Arc<Mutex<Option<Waker>>>,
    shutdown: &AtomicBool,
    pending: &mut VecDeque<String>,
    socket: &mut RelaySocket,
    keepalive: &mut KeepaliveState,
    suspend_gap: &mut SuspendGapDetector,
    preamble: &mut Vec<String>,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
    ephemeral: &mut VecDeque<EphemeralFrame>,
    ephemeral_write_accepted: &mut Vec<EphemeralCompletion>,
    initial_gate_required: bool,
    relay: super::committed_observations::RelayScope,
    committed_observations: &super::committed_observations::CommittedObservationCache,
) -> ConnectedOutcome {
    let mut outbound_released = !initial_gate_required;
    let outcome = run_connected_inner(
        slot,
        generation,
        event_tx,
        command_rx,
        waker_slot,
        shutdown,
        pending,
        socket,
        keepalive,
        suspend_gap,
        preamble,
        durable,
        write_accepted,
        ephemeral,
        ephemeral_write_accepted,
        &mut outbound_released,
        initial_gate_required,
        relay,
        committed_observations,
    );
    resolve_generation_end(
        event_tx,
        slot,
        generation,
        durable,
        write_accepted,
        ephemeral,
        ephemeral_write_accepted,
    );
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_connected_inner(
    slot: u32,
    generation: u64,
    event_tx: &SyncSender<WorkerEvent>,
    command_rx: &Receiver<WorkerCommand>,
    waker_slot: &Arc<Mutex<Option<Waker>>>,
    shutdown: &AtomicBool,
    pending: &mut VecDeque<String>,
    socket: &mut RelaySocket,
    keepalive: &mut KeepaliveState,
    suspend_gap: &mut SuspendGapDetector,
    preamble: &mut Vec<String>,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
    ephemeral: &mut VecDeque<EphemeralFrame>,
    ephemeral_write_accepted: &mut Vec<EphemeralCompletion>,
    outbound_released: &mut bool,
    initial_gate_required: bool,
    relay: super::committed_observations::RelayScope,
    committed_observations: &super::committed_observations::CommittedObservationCache,
) -> ConnectedOutcome {
    let mut poller = match RelayPoller::new(socket, waker_slot) {
        Ok(poller) => poller,
        Err(error) => {
            return ConnectedOutcome::Reconnect {
                message: format!("readiness setup failed: {error}"),
                permanent: false,
            }
        }
    };

    if initial_gate_required {
        // Arbitrate a protected generation's first inbound frame before
        // accepting any ordinary outbound command. Control wakeups only
        // buffer commands during this worker-owned interval; they cannot
        // terminate it or flush ordinary wire. Public sessions skip this
        // entire handshake and enter the established read/write loop below.
        let initial_read_deadline = Instant::now() + INITIAL_READ_OBSERVATION_WINDOW;
        loop {
            // Authoritative terminal check (issue #506 Fix 2), mirrored from
            // the established loop below: a retirement during this bounded
            // observation window may have had its best-effort `Shutdown`
            // nudge dropped by a full command queue, so the atomic — not the
            // 250 ms deadline — is what guarantees a prompt exit.
            if shutdown_requested(shutdown) {
                resolve_queued_durables_on_shutdown(command_rx, event_tx, slot, generation);
                let _ = socket.close(None);
                return ConnectedOutcome::Shutdown;
            }
            match drain_commands(
                command_rx,
                pending,
                preamble,
                durable,
                ephemeral,
                outbound_released,
                event_tx,
                slot,
                generation,
            ) {
                Drain::Continue => {}
                Drain::Shutdown | Drain::Disconnected => {
                    let _ = socket.close(None);
                    return ConnectedOutcome::Shutdown;
                }
            }
            let remaining = initial_read_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match poller.wait(remaining) {
                Ok(true) => {
                    if let Some(outcome) = complete_initial_read(
                        slot,
                        generation,
                        event_tx,
                        socket,
                        keepalive,
                        relay,
                        committed_observations,
                    ) {
                        return outcome;
                    }
                    break;
                }
                Ok(false) => {
                    if Instant::now() >= initial_read_deadline {
                        break;
                    }
                }
                Err(error) => {
                    return ConnectedOutcome::Reconnect {
                        message: format!("initial readiness wait failed: {error}"),
                        permanent: false,
                    }
                }
            }
        }
        if let Some(outcome) = complete_initial_read(
            slot,
            generation,
            event_tx,
            socket,
            keepalive,
            relay,
            committed_observations,
        ) {
            return outcome;
        }
    }

    // Resume-gap heuristic (issue #4): sticky across iterations, NOT a
    // one-shot mirror of `suspend_gap.observe`'s return value. A detected
    // gap sets this; it is cleared only once a ping actually reaches the
    // wire (`FlushResult::Flushed`), never merely attempted. Without this,
    // a `Blocked` write on the very first post-resume iteration -- likely
    // exactly then, since `flush_generation_writes` above already tried to
    // push any suspension-queued writes into the same still-dead socket --
    // would silently drop the accelerated probe: `suspend_gap.observe`
    // already consumed the gap for this iteration, so nothing would
    // re-arm the upgrade on the next one, and detection would quietly fall
    // back to the ordinary ~60s idle+pong schedule this heuristic exists
    // to cut.
    let mut pending_gap = false;

    loop {
        // Authoritative terminal check (issue #506 Fix 2): `retire` wakes the
        // mio waker (unparking `poller.wait` below) and sets this atomic. The
        // best-effort `Shutdown` nudge may be dropped if the bounded command
        // queue is full, so a queued `Shutdown` alone is NOT relied on — this
        // check is what guarantees the loop exits even when the nudge was
        // dropped and `drain_commands` only saw ordinary data.
        if shutdown_requested(shutdown) {
            // Settle any durables still in the CHANNEL before exiting (#506
            // Fix 2). `resolve_generation_end` (called by `run_connected`
            // right after this returns) only drains the worker-local `durable`
            // VecDeque, never `command_rx`, so channel-resident `SendDurable`s
            // would otherwise be lost on retirement.
            resolve_queued_durables_on_shutdown(command_rx, event_tx, slot, generation);
            let _ = socket.close(None);
            return ConnectedOutcome::Shutdown;
        }
        match drain_commands(
            command_rx,
            pending,
            preamble,
            durable,
            ephemeral,
            outbound_released,
            event_tx,
            slot,
            generation,
        ) {
            Drain::Continue => {}
            Drain::Shutdown | Drain::Disconnected => {
                let _ = socket.close(None);
                return ConnectedOutcome::Shutdown;
            }
        }

        let flush = flush_generation_writes(
            *outbound_released,
            pending,
            durable,
            write_accepted,
            ephemeral,
            ephemeral_write_accepted,
            socket,
            event_tx,
            slot,
            generation,
        );
        let mut wants_write = match flush {
            FlushResult::Flushed => false,
            FlushResult::Blocked => true,
            FlushResult::Broken(message) => {
                return ConnectedOutcome::Reconnect {
                    message,
                    permanent: false,
                }
            }
        };

        // Resume-gap heuristic (issue #4): always observe this iteration's
        // wall-clock reading (so the detector's baseline never goes stale
        // across an iteration that happened not to trip it). A fresh
        // detection latches the sticky `pending_gap` flag (see its doc
        // above the loop); `apply_resume_gap` reads that sticky flag, not
        // the one-shot `observe` result, so a gap that couldn't be probed
        // this iteration (ping write `Blocked`) stays armed for the next
        // one instead of silently expiring.
        if suspend_gap.observe(SystemTime::now()) {
            pending_gap = true;
        }
        let action = keepalive.step(Instant::now());
        let action = apply_resume_gap(action, keepalive.ping_in_flight(), pending_gap);
        match action {
            KeepaliveAction::Idle => {}
            KeepaliveAction::EmitPing => {
                match flush_message(
                    socket,
                    Message::Ping(Vec::new().into()),
                    write_accepted,
                    ephemeral_write_accepted,
                    event_tx,
                    slot,
                    generation,
                ) {
                    FlushResult::Flushed => {
                        keepalive.on_ping_flushed(Instant::now());
                        // The probe this heuristic exists to send actually
                        // reached the wire -- whether this ping was ordinary
                        // or gap-upgraded, the pending intent is satisfied.
                        pending_gap = false;
                    }
                    FlushResult::Blocked => wants_write = true,
                    FlushResult::Broken(message) => {
                        return ConnectedOutcome::Reconnect {
                            message,
                            permanent: false,
                        }
                    }
                }
            }
            KeepaliveAction::Dead => {
                return ConnectedOutcome::Reconnect {
                    message: "keepalive timeout (no inbound frame within pong window)".to_string(),
                    permanent: false,
                }
            }
        }

        if let Err(error) = poller.set_wants_write(socket, wants_write) {
            return ConnectedOutcome::Reconnect {
                message: format!("readiness update failed: {error}"),
                permanent: false,
            };
        }

        let timeout = keepalive
            .next_deadline()
            .saturating_duration_since(Instant::now());
        if let Err(error) = poller.wait(timeout) {
            return ConnectedOutcome::Reconnect {
                message: format!("readiness wait failed: {error}"),
                permanent: false,
            };
        }

        // Edge-triggered platforms (kqueue's EV_CLEAR) can coalesce a
        // readable event with a control/writable event in the same mio
        // batch, so drain reads unconditionally on every wakeup rather than
        // gating on a readable flag — an inbound frame arriving
        // simultaneously with a waker must never be silently skipped. A
        // non-readable socket's `read()` just returns `WouldBlock`
        // immediately, so this is cheap.
        if let Some(outcome) = drain_reads(
            slot,
            generation,
            event_tx,
            socket,
            keepalive,
            Some(&mut pending_gap),
            relay,
            committed_observations,
        ) {
            return outcome;
        }
    }
}

fn complete_initial_read(
    slot: u32,
    generation: u64,
    event_tx: &SyncSender<WorkerEvent>,
    socket: &mut RelaySocket,
    keepalive: &mut KeepaliveState,
    relay: super::committed_observations::RelayScope,
    committed_observations: &super::committed_observations::CommittedObservationCache,
) -> Option<ConnectedOutcome> {
    if let Some(outcome) = drain_reads(
        slot,
        generation,
        event_tx,
        socket,
        keepalive,
        None,
        relay,
        committed_observations,
    ) {
        return Some(outcome);
    }
    event_tx
        .send(WorkerEvent {
            slot,
            generation,
            kind: WorkerEventKind::InitialReadCompleted,
        })
        .err()
        .map(|_| ConnectedOutcome::Shutdown)
}

enum Drain {
    Continue,
    Shutdown,
    Disconnected,
}

/// `generation` is the CURRENT worker generation this call is draining
/// for. A `SendDurable` command whose own `generation` field doesn't match
/// is stale — it raced a reconnect between the caller reading its
/// `RelayHandle` and this drain running — and resolves `NotHandedOff`
/// immediately rather than ever being attempted against a connection it
/// was never actually meant for.
#[allow(clippy::too_many_arguments)]
fn drain_commands(
    command_rx: &Receiver<WorkerCommand>,
    pending: &mut VecDeque<String>,
    preamble: &mut Vec<String>,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    ephemeral: &mut VecDeque<EphemeralFrame>,
    outbound_released: &mut bool,
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
) -> Drain {
    loop {
        match command_rx.try_recv() {
            Ok(WorkerCommand::Send(text)) => pending.push_back(text),
            Ok(WorkerCommand::Shutdown) => return Drain::Shutdown,
            Ok(WorkerCommand::SetReconnectPreamble(frames)) => *preamble = frames,
            Ok(WorkerCommand::ReleaseInitialRead {
                generation: release_generation,
            }) => {
                if release_generation == generation {
                    *outbound_released = true;
                }
            }
            Ok(WorkerCommand::SendDurable {
                generation: cmd_generation,
                correlation,
                frame,
            }) => {
                if cmd_generation == generation {
                    durable.push_back((correlation, frame));
                } else {
                    resolve_correlation(
                        event_tx,
                        slot,
                        generation,
                        correlation,
                        HandoffResult::NotHandedOff,
                    );
                }
            }
            Ok(WorkerCommand::SendEphemeral {
                generation: cmd_generation,
                frame,
                completion,
            }) => {
                if cmd_generation == generation {
                    ephemeral.push_back(EphemeralFrame { frame, completion });
                } else {
                    completion.resolve(EphemeralSendOutcome::Unavailable);
                }
            }
            Err(TryRecvError::Empty) => return Drain::Continue,
            Err(TryRecvError::Disconnected) => return Drain::Disconnected,
        }
    }
}

enum FlushResult {
    Flushed,
    Blocked,
    Broken(String),
}

#[allow(clippy::too_many_arguments)]
fn flush_generation_writes(
    outbound_released: bool,
    pending: &mut VecDeque<String>,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
    ephemeral: &mut VecDeque<EphemeralFrame>,
    ephemeral_write_accepted: &mut Vec<EphemeralCompletion>,
    socket: &mut RelaySocket,
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
) -> FlushResult {
    if outbound_released {
        flush_writes(
            pending,
            durable,
            write_accepted,
            ephemeral,
            ephemeral_write_accepted,
            socket,
            event_tx,
            slot,
            generation,
        )
    } else {
        flush_ephemeral_writes(
            ephemeral,
            write_accepted,
            ephemeral_write_accepted,
            socket,
            event_tx,
            slot,
            generation,
        )
    }
}

/// Write every pending REQ frame, then every queued durable EVENT frame,
/// then flush the socket ONCE for the whole batch — durable frames whose
/// OWN `write()` succeeds move to `write_accepted` (awaiting THIS shared
/// flush to confirm them); once ANY socket flush reports `Flushed` they
/// resolve `Written` through [`flush_socket_and_settle`] (including a later
/// keepalive/control flush). A `Blocked`/`Broken` flush leaves them in
/// `write_accepted` for the caller to resolve later (a subsequent flush
/// attempt, or — on `Broken` — [`resolve_generation_end`] once the
/// connection actually ends): never resolved twice, never resolved early.
#[allow(clippy::too_many_arguments)]
fn flush_writes(
    pending: &mut VecDeque<String>,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
    ephemeral: &mut VecDeque<EphemeralFrame>,
    ephemeral_write_accepted: &mut Vec<EphemeralCompletion>,
    socket: &mut RelaySocket,
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
) -> FlushResult {
    while let Some(text) = pending.pop_front() {
        match socket.write(Message::Text(text.clone().into())) {
            Ok(()) => {}
            Err(error) if is_nonblocking_io(&error) => {
                pending.push_front(text);
                return FlushResult::Blocked;
            }
            Err(error) => return FlushResult::Broken(error.to_string()),
        }
    }
    while let Some((correlation, text)) = durable.pop_front() {
        match socket.write(Message::Text(text.clone().into())) {
            Ok(()) => write_accepted.push(correlation),
            Err(error) if is_nonblocking_io(&error) => {
                durable.push_front((correlation, text));
                return FlushResult::Blocked;
            }
            Err(error) => {
                // This exact frame's OWN write() call failed outright --
                // never accepted by the socket library at all, unlike the
                // entries already sitting in `write_accepted` (which DID
                // succeed their own write() and are merely unconfirmed).
                // Pushing it back means `resolve_generation_end` resolves
                // it `NotHandedOff`, not `Ambiguous`.
                durable.push_front((correlation, text));
                return FlushResult::Broken(error.to_string());
            }
        }
    }
    flush_ephemeral_writes(
        ephemeral,
        write_accepted,
        ephemeral_write_accepted,
        socket,
        event_tx,
        slot,
        generation,
    )
}

#[allow(clippy::too_many_arguments)]
fn flush_ephemeral_writes(
    ephemeral: &mut VecDeque<EphemeralFrame>,
    write_accepted: &mut Vec<AttemptCorrelation>,
    ephemeral_write_accepted: &mut Vec<EphemeralCompletion>,
    socket: &mut RelaySocket,
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
) -> FlushResult {
    while let Some(pending) = ephemeral.pop_front() {
        match socket.write(Message::Text(pending.frame.clone().into())) {
            Ok(()) => ephemeral_write_accepted.push(pending.completion),
            Err(error) if is_nonblocking_io(&error) => {
                ephemeral.push_front(pending);
                return FlushResult::Blocked;
            }
            Err(error) => {
                ephemeral.push_front(pending);
                return FlushResult::Broken(error.to_string());
            }
        }
    }
    flush_socket_and_settle(
        socket,
        write_accepted,
        ephemeral_write_accepted,
        event_tx,
        slot,
        generation,
    )
}

#[allow(clippy::too_many_arguments)]
fn flush_message(
    socket: &mut RelaySocket,
    message: Message,
    write_accepted: &mut Vec<AttemptCorrelation>,
    ephemeral_write_accepted: &mut Vec<EphemeralCompletion>,
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
) -> FlushResult {
    match socket.write(message) {
        Ok(()) => flush_socket_and_settle(
            socket,
            write_accepted,
            ephemeral_write_accepted,
            event_tx,
            slot,
            generation,
        ),
        Err(error) if is_nonblocking_io(&error) => FlushResult::Blocked,
        Err(error) => FlushResult::Broken(error.to_string()),
    }
}

/// The single successful-flush boundary for a connected generation. A
/// flush confirms every prior socket-accepted durable frame, regardless of
/// which message caused the flush (EVENT batch, keepalive ping, or future
/// control traffic). Keeping settlement here prevents a later successful
/// control flush from being forgotten and mislabeled `Ambiguous` at teardown.
fn flush_socket_and_settle(
    socket: &mut RelaySocket,
    write_accepted: &mut Vec<AttemptCorrelation>,
    ephemeral_write_accepted: &mut Vec<EphemeralCompletion>,
    event_tx: &SyncSender<WorkerEvent>,
    slot: u32,
    generation: u64,
) -> FlushResult {
    let result = flush_socket(socket);
    if matches!(result, FlushResult::Flushed) {
        for correlation in write_accepted.drain(..) {
            resolve_correlation(
                event_tx,
                slot,
                generation,
                correlation,
                HandoffResult::Written,
            );
        }
        for completion in ephemeral_write_accepted.drain(..) {
            completion.resolve(EphemeralSendOutcome::Accepted);
        }
    }
    result
}

fn flush_socket(socket: &mut RelaySocket) -> FlushResult {
    match socket.flush() {
        Ok(()) => FlushResult::Flushed,
        Err(error) if is_nonblocking_io(&error) => FlushResult::Blocked,
        Err(error) => FlushResult::Broken(error.to_string()),
    }
}

// These are the worker loop's already-borrowed state owners. Grouping them
// behind another context object would add indirection without reducing
// ownership or lifetime complexity at this private boundary.
#[allow(clippy::too_many_arguments)]
fn drain_reads(
    slot: u32,
    generation: u64,
    event_tx: &SyncSender<WorkerEvent>,
    socket: &mut RelaySocket,
    keepalive: &mut KeepaliveState,
    mut pending_gap: Option<&mut bool>,
    relay: super::committed_observations::RelayScope,
    committed_observations: &super::committed_observations::CommittedObservationCache,
) -> Option<ConnectedOutcome> {
    loop {
        match socket.read() {
            Ok(message) => {
                keepalive.on_inbound(Instant::now());
                // Resume-gap heuristic (issue #4): any inbound frame proves
                // the socket is alive and responsive, so the sticky
                // pending-gap flag (see its doc above the established loop)
                // is satisfied the same as an actually-flushed ping would --
                // there is nothing left for the accelerated probe to prove.
                // `None` during the initial-read handshake window: that
                // phase predates the flag entirely.
                if let Some(flag) = pending_gap.as_mut() {
                    **flag = false;
                }
                if matches!(message, Message::Close(_)) {
                    return Some(ConnectedOutcome::Reconnect {
                        message: "peer closed websocket".to_string(),
                        permanent: false,
                    });
                }
                if let Some(frame) = classify_message(message, relay, committed_observations) {
                    if event_tx
                        .send(WorkerEvent {
                            slot,
                            generation,
                            kind: WorkerEventKind::Frame(frame),
                        })
                        .is_err()
                    {
                        return Some(ConnectedOutcome::Shutdown);
                    }
                }
            }
            Err(error) if read_error_disposition(&error) == ReadErrorDisposition::Retry => continue,
            Err(error) if read_error_disposition(&error) == ReadErrorDisposition::Drained => {
                return None;
            }
            Err(error) => {
                let message = error.to_string();
                let permanent = backoff::is_permanent_error(&message);
                return Some(ConnectedOutcome::Reconnect { message, permanent });
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadErrorDisposition {
    Retry,
    Drained,
    Fatal,
}

fn read_error_disposition(error: &tungstenite::Error) -> ReadErrorDisposition {
    match error {
        tungstenite::Error::Io(error) if error.kind() == io::ErrorKind::Interrupted => {
            ReadErrorDisposition::Retry
        }
        tungstenite::Error::Io(error) if error.kind() == io::ErrorKind::WouldBlock => {
            ReadErrorDisposition::Drained
        }
        _ => ReadErrorDisposition::Fatal,
    }
}

fn is_nonblocking_io(error: &tungstenite::Error) -> bool {
    matches!(
        error,
        tungstenite::Error::Io(io)
            if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
    )
}

/// mio readiness wrapper: one `Poll` per connected socket, registered for
/// `READABLE` always and `WRITABLE` only while there is queued output, plus
/// a `Waker` (installed into the shared `waker_slot` for the duration of
/// this socket's session) so `WorkerHandle::push` can interrupt a blocked
/// `wait`.
struct RelayPoller<'a> {
    poll: Poll,
    events: Events,
    wants_write: bool,
    waker_slot: &'a Mutex<Option<Waker>>,
}

impl<'a> RelayPoller<'a> {
    fn new(socket: &mut RelaySocket, waker_slot: &'a Mutex<Option<Waker>>) -> io::Result<Self> {
        socket_tcp(socket)?.set_nonblocking(true)?;
        let poll = Poll::new()?;
        register_socket(&poll, socket, false, false)?;
        let waker = Waker::new(poll.registry(), CONTROL)?;
        if let Ok(mut guard) = waker_slot.lock() {
            *guard = Some(waker);
        }
        Ok(Self {
            poll,
            events: Events::with_capacity(16),
            wants_write: false,
            waker_slot,
        })
    }

    fn set_wants_write(&mut self, socket: &mut RelaySocket, wants_write: bool) -> io::Result<()> {
        if self.wants_write == wants_write {
            return Ok(());
        }
        register_socket(&self.poll, socket, wants_write, true)?;
        self.wants_write = wants_write;
        Ok(())
    }

    /// Block until the socket is ready, the waker fires, or `timeout`
    /// elapses. The caller doesn't need to know WHICH woke it — every
    /// wakeup unconditionally re-drains commands, writes, and reads (see
    /// the call site's comment on why that's both correct and cheap).
    fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
        self.poll.poll(&mut self.events, Some(timeout))?;
        Ok(self
            .events
            .iter()
            .any(|event| event.token() == SOCKET && event.is_readable()))
    }
}

impl Drop for RelayPoller<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.waker_slot.lock() {
            *guard = None;
        }
    }
}

fn register_socket(
    poll: &Poll,
    socket: &mut RelaySocket,
    wants_write: bool,
    registered: bool,
) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let fd = socket_tcp(socket)?.as_raw_fd();
    let interest = if wants_write {
        Interest::READABLE.add(Interest::WRITABLE)
    } else {
        Interest::READABLE
    };
    let mut source = SourceFd(&fd);
    if registered {
        poll.registry().reregister(&mut source, SOCKET, interest)
    } else {
        poll.registry().register(&mut source, SOCKET, interest)
    }
}

fn socket_tcp(socket: &mut RelaySocket) -> io::Result<&mut TcpStream> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => Ok(stream),
        MaybeTlsStream::Rustls(stream) => Ok(stream.get_mut()),
        #[allow(unreachable_patterns)]
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported relay socket stream variant",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::RelayMessage;
    use std::io::Read;
    use std::net::TcpListener;
    use tungstenite::protocol::{Role, WebSocketConfig};

    const LARGE_FRAME_BYTES: usize = 8 * 1024 * 1024;
    const TEST_EVENT_QUEUE_CAPACITY: usize = 8;

    fn real_buffered_socket() -> (RelaySocket, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (peer, _) = listener.accept().unwrap();
        client.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        let config = WebSocketConfig::default().write_buffer_size(LARGE_FRAME_BYTES * 2);
        let socket = tungstenite::WebSocket::from_raw_socket(
            MaybeTlsStream::Plain(client),
            Role::Client,
            Some(config),
        );
        (socket, peer)
    }

    fn real_websocket_pair() -> (
        RelaySocket,
        tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (peer, _) = listener.accept().unwrap();
        client.set_nonblocking(true).unwrap();
        let client = tungstenite::WebSocket::from_raw_socket(
            MaybeTlsStream::Plain(client),
            Role::Client,
            None,
        );
        let server = tungstenite::WebSocket::from_raw_socket(
            MaybeTlsStream::Plain(peer),
            Role::Server,
            None,
        );
        (client, server)
    }

    fn begin_real_unconfirmed_write(
        socket: &mut RelaySocket,
        correlation: AttemptCorrelation,
        event_tx: &SyncSender<WorkerEvent>,
        write_accepted: &mut Vec<AttemptCorrelation>,
    ) {
        let mut pending = VecDeque::new();
        let mut durable = VecDeque::from([(correlation, "x".repeat(LARGE_FRAME_BYTES))]);
        let mut ephemeral = VecDeque::new();
        let mut ephemeral_write_accepted = Vec::new();
        assert!(matches!(
            flush_writes(
                &mut pending,
                &mut durable,
                write_accepted,
                &mut ephemeral,
                &mut ephemeral_write_accepted,
                socket,
                event_tx,
                1,
                1,
            ),
            FlushResult::Blocked
        ));
        assert!(durable.is_empty(), "the frame's write() was accepted");
        assert_eq!(write_accepted, &[correlation]);
    }

    fn drain_peer(peer: &mut TcpStream) {
        let mut bytes = [0u8; 64 * 1024];
        loop {
            match peer.read(&mut bytes) {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) => panic!("peer read failed: {error}"),
            }
        }
    }

    fn handoff_results(rx: &Receiver<WorkerEvent>) -> Vec<(AttemptCorrelation, HandoffResult)> {
        rx.try_iter()
            .filter_map(|event| match event.kind {
                WorkerEventKind::EventHandoff {
                    correlation,
                    result,
                } => Some((correlation, result)),
                _ => None,
            })
            .collect()
    }

    fn ephemeral_completion() -> (EphemeralCompletion, Receiver<EphemeralSendOutcome>) {
        let (tx, rx) = mpsc::channel();
        (
            EphemeralCompletion::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
            rx,
        )
    }

    #[test]
    fn pack_generation_is_ordered_by_worker_id_then_attempt() {
        assert!(pack_generation(1, 0) < pack_generation(1, 1));
        assert!(pack_generation(1, u32::MAX) < pack_generation(2, 0));
        assert_eq!(pack_generation(0, 0), 0);
    }

    #[test]
    fn worker_id_of_round_trips_through_pack_generation() {
        assert_eq!(worker_id_of(pack_generation(7, 42)), 7);
        assert_eq!(worker_id_of(pack_generation(0, u32::MAX)), 0);
        assert_ne!(
            worker_id_of(pack_generation(1, 0)),
            worker_id_of(pack_generation(2, 0))
        );
    }

    #[test]
    fn initial_read_orders_buffered_auth_before_completion_and_completes_empty_once() {
        let relay = super::super::committed_observations::RelayScope::new(
            &RelayUrl::parse("wss://relay.example").unwrap(),
        );
        let committed_observations =
            super::super::committed_observations::CommittedObservationCache::new(0);
        let (mut socket, mut peer) = real_websocket_pair();
        peer.send(Message::Text(
            "[\"AUTH\",\"worker-ordered\"]".to_string().into(),
        ))
        .unwrap();
        peer.flush().unwrap();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let mut keepalive = KeepaliveState::new(
            Instant::now(),
            Duration::from_secs(60),
            Duration::from_secs(10),
        );
        let waker_slot = Arc::new(Mutex::new(None));
        let mut poller = RelayPoller::new(&mut socket, &waker_slot).unwrap();
        assert!(poller.wait(Duration::from_secs(1)).unwrap());
        drop(poller);

        assert!(complete_initial_read(
            3,
            9,
            &event_tx,
            &mut socket,
            &mut keepalive,
            relay,
            &committed_observations,
        )
        .is_none());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0].kind,
            WorkerEventKind::Frame(RelayFrame::Message(message))
                if matches!(message.as_ref(), RelayMessage::Auth { challenge } if challenge == "worker-ordered")
        ));
        assert!(matches!(
            events[1].kind,
            WorkerEventKind::InitialReadCompleted
        ));

        let (mut empty_socket, _empty_peer) = real_websocket_pair();
        let (empty_tx, empty_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let mut empty_keepalive = KeepaliveState::new(
            Instant::now(),
            Duration::from_secs(60),
            Duration::from_secs(10),
        );
        assert!(complete_initial_read(
            4,
            10,
            &empty_tx,
            &mut empty_socket,
            &mut empty_keepalive,
            relay,
            &committed_observations,
        )
        .is_none());
        let empty_events = empty_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(empty_events.len(), 1);
        assert!(matches!(
            empty_events[0].kind,
            WorkerEventKind::InitialReadCompleted
        ));

        assert_eq!(
            read_error_disposition(&tungstenite::Error::Io(io::Error::from(
                io::ErrorKind::Interrupted,
            ))),
            ReadErrorDisposition::Retry,
            "Interrupted must retry the read instead of completing the drain"
        );
        assert_eq!(
            read_error_disposition(&tungstenite::Error::Io(io::Error::from(
                io::ErrorKind::WouldBlock,
            ))),
            ReadErrorDisposition::Drained
        );

        let (mut closing_socket, mut closing_peer) = real_websocket_pair();
        closing_peer.close(None).unwrap();
        closing_peer.flush().unwrap();
        let (closing_tx, closing_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let closing_waker = Arc::new(Mutex::new(None));
        let mut closing_poller = RelayPoller::new(&mut closing_socket, &closing_waker).unwrap();
        assert!(
            closing_poller.wait(Duration::from_secs(1)).unwrap(),
            "close frame must be socket-readable before exercising initial drain"
        );
        let mut closing_keepalive = KeepaliveState::new(
            Instant::now(),
            Duration::from_secs(60),
            Duration::from_secs(10),
        );
        assert!(matches!(
            complete_initial_read(
                5,
                11,
                &closing_tx,
                &mut closing_socket,
                &mut closing_keepalive,
                relay,
                &committed_observations,
            ),
            Some(ConnectedOutcome::Reconnect { .. })
        ));
        assert!(
            closing_rx
                .try_iter()
                .all(|event| !matches!(event.kind, WorkerEventKind::InitialReadCompleted)),
            "a websocket close is fatal and cannot emit completion"
        );
    }

    #[test]
    fn public_connected_loop_flushes_queued_wire_without_initial_marker() {
        let (mut socket, mut peer) = real_websocket_pair();
        let (command_tx, command_rx) = mpsc::channel();
        command_tx
            .send(WorkerCommand::Send("public-immediate".to_string()))
            .unwrap();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let waker = Arc::new(Mutex::new(None));
        let worker_waker = Arc::clone(&waker);
        let worker = std::thread::spawn(move || {
            let mut pending = VecDeque::new();
            let mut keepalive = KeepaliveState::new(
                Instant::now(),
                Duration::from_secs(60),
                Duration::from_secs(10),
            );
            let mut suspend_gap = SuspendGapDetector::new(SystemTime::now(), SUSPEND_GAP_THRESHOLD);
            let mut preamble = Vec::new();
            let mut durable = VecDeque::new();
            let mut write_accepted = Vec::new();
            let mut ephemeral = VecDeque::new();
            let mut ephemeral_write_accepted = Vec::new();
            let mut outbound_released = true;
            let shutdown = AtomicBool::new(false);
            let relay = super::super::committed_observations::RelayScope::new(
                &RelayUrl::parse("wss://relay.example").unwrap(),
            );
            let committed_observations =
                super::super::committed_observations::CommittedObservationCache::new(0);
            run_connected_inner(
                3,
                12,
                &event_tx,
                &command_rx,
                &worker_waker,
                &shutdown,
                &mut pending,
                &mut socket,
                &mut keepalive,
                &mut suspend_gap,
                &mut preamble,
                &mut durable,
                &mut write_accepted,
                &mut ephemeral,
                &mut ephemeral_write_accepted,
                &mut outbound_released,
                false,
                relay,
                &committed_observations,
            )
        });

        assert!(matches!(
            peer.read().unwrap(),
            Message::Text(text) if text == "public-immediate"
        ));
        assert!(
            event_rx
                .try_iter()
                .all(|event| !matches!(event.kind, WorkerEventKind::InitialReadCompleted)),
            "a public session must never enter the protected marker handshake"
        );
        command_tx.send(WorkerCommand::Shutdown).unwrap();
        if let Some(waker) = waker.lock().unwrap().as_ref() {
            waker.wake().unwrap();
        }
        assert!(matches!(worker.join().unwrap(), ConnectedOutcome::Shutdown));
    }

    /// The resume-gap heuristic (issue #4), exercised end to end against a
    /// real socket: a `SuspendGapDetector` seeded with a deliberately stale
    /// baseline observes a huge gap on its very first real-wall-clock
    /// `observe()` call inside the loop -- simulating "the process was just
    /// resumed after a long suspension" without an actual sleep. With a
    /// long (60s) keepalive idle threshold, no ordinary ping would fire for
    /// a full minute; the heuristic must instead emit one immediately.
    #[test]
    fn resume_gap_triggers_immediate_ping_bypassing_the_idle_threshold() {
        let (mut socket, mut peer) = real_websocket_pair();
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let waker = Arc::new(Mutex::new(None));
        let worker_waker = Arc::clone(&waker);
        let worker = std::thread::spawn(move || {
            let mut pending = VecDeque::new();
            let mut keepalive = KeepaliveState::new(
                Instant::now(),
                Duration::from_secs(60),
                Duration::from_secs(10),
            );
            let stale_baseline = SystemTime::now() - Duration::from_secs(120);
            let mut suspend_gap = SuspendGapDetector::new(stale_baseline, SUSPEND_GAP_THRESHOLD);
            let mut preamble = Vec::new();
            let mut durable = VecDeque::new();
            let mut write_accepted = Vec::new();
            let mut ephemeral = VecDeque::new();
            let mut ephemeral_write_accepted = Vec::new();
            let mut outbound_released = true;
            let shutdown = AtomicBool::new(false);
            let relay = super::super::committed_observations::RelayScope::new(
                &RelayUrl::parse("wss://relay.example").unwrap(),
            );
            let committed_observations =
                super::super::committed_observations::CommittedObservationCache::new(0);
            run_connected_inner(
                3,
                12,
                &event_tx,
                &command_rx,
                &worker_waker,
                &shutdown,
                &mut pending,
                &mut socket,
                &mut keepalive,
                &mut suspend_gap,
                &mut preamble,
                &mut durable,
                &mut write_accepted,
                &mut ephemeral,
                &mut ephemeral_write_accepted,
                &mut outbound_released,
                false,
                relay,
                &committed_observations,
            )
        });

        assert!(
            matches!(peer.read().unwrap(), Message::Ping(_)),
            "a detected resume gap must emit an immediate ping instead of waiting \
             out the 60s idle threshold"
        );

        command_tx.send(WorkerCommand::Shutdown).unwrap();
        if let Some(waker) = waker.lock().unwrap().as_ref() {
            waker.wake().unwrap();
        }
        assert!(matches!(worker.join().unwrap(), ConnectedOutcome::Shutdown));
    }

    /// Review regression guard: a gap-triggered ping that hits a `Blocked`
    /// write on its first attempt -- the exact scenario a suspension-queued
    /// write earlier in the same resume iteration produces against a still-
    /// stalled socket -- must not silently drop the accelerated probe.
    /// `SuspendGapDetector::observe` is one-shot (it already consumed the
    /// gap for this iteration by the time the ping write is attempted), so
    /// only the sticky `pending_gap` flag can make the loop retry `EmitPing`
    /// on a LATER iteration, once the socket actually has room.
    #[test]
    fn resume_gap_survives_a_blocked_first_ping_attempt() {
        // `real_buffered_socket`'s peer stream is nonblocking (unlike
        // `real_websocket_pair`'s), so a plain `peer.read()` can spuriously
        // observe `WouldBlock` before the worker thread has written
        // anything yet; retry past that instead of treating it as failure.
        fn read_blocking(peer: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>) -> Message {
            loop {
                match peer.read() {
                    Ok(message) => return message,
                    Err(tungstenite::Error::Io(error))
                        if error.kind() == io::ErrorKind::WouldBlock =>
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("peer read failed: {error}"),
                }
            }
        }

        let (mut socket, peer_stream) = real_buffered_socket();
        let mut peer = tungstenite::WebSocket::from_raw_socket(
            MaybeTlsStream::Plain(peer_stream),
            Role::Server,
            None,
        );

        // Saturate the socket so the worker's first gap-triggered ping
        // attempt genuinely blocks, before the loop ever gets to run.
        let _ = socket.write(Message::Text("x".repeat(LARGE_FRAME_BYTES).into()));
        assert!(
            matches!(socket.flush(), Err(ref error) if is_nonblocking_io(error)),
            "setup: the giant frame must leave the socket genuinely blocked"
        );

        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, _event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let waker = Arc::new(Mutex::new(None));
        let worker_waker = Arc::clone(&waker);
        let worker = std::thread::spawn(move || {
            let mut pending = VecDeque::new();
            let mut keepalive = KeepaliveState::new(
                Instant::now(),
                Duration::from_secs(60),
                Duration::from_secs(10),
            );
            let stale_baseline = SystemTime::now() - Duration::from_secs(120);
            let mut suspend_gap = SuspendGapDetector::new(stale_baseline, SUSPEND_GAP_THRESHOLD);
            let mut preamble = Vec::new();
            let mut durable = VecDeque::new();
            let mut write_accepted = Vec::new();
            let mut ephemeral = VecDeque::new();
            let mut ephemeral_write_accepted = Vec::new();
            let mut outbound_released = true;
            let shutdown = AtomicBool::new(false);
            let relay = super::super::committed_observations::RelayScope::new(
                &RelayUrl::parse("wss://relay.example").unwrap(),
            );
            let committed_observations =
                super::super::committed_observations::CommittedObservationCache::new(0);
            run_connected_inner(
                3,
                12,
                &event_tx,
                &command_rx,
                &worker_waker,
                &shutdown,
                &mut pending,
                &mut socket,
                &mut keepalive,
                &mut suspend_gap,
                &mut preamble,
                &mut durable,
                &mut write_accepted,
                &mut ephemeral,
                &mut ephemeral_write_accepted,
                &mut outbound_released,
                false,
                relay,
                &committed_observations,
            )
        });

        // Draining the giant frame is what finally gives the worker's
        // blocked ping room to flush on a subsequent loop iteration.
        assert!(matches!(
            read_blocking(&mut peer),
            Message::Text(text) if text.len() == LARGE_FRAME_BYTES
        ));
        assert!(
            matches!(read_blocking(&mut peer), Message::Ping(_)),
            "the sticky pending-gap flag must retry the ping once the blocked \
             write clears, not only on the one iteration that first detected \
             the gap"
        );

        command_tx.send(WorkerCommand::Shutdown).unwrap();
        if let Some(waker) = waker.lock().unwrap().as_ref() {
            waker.wake().unwrap();
        }
        assert!(matches!(worker.join().unwrap(), ConnectedOutcome::Shutdown));
    }

    #[test]
    fn ordinary_outbound_is_held_until_exact_generation_release() {
        let (mut socket, mut peer) = real_websocket_pair();
        let (event_tx, _event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let mut pending = VecDeque::from(["held".to_string()]);
        let mut durable = VecDeque::new();
        let mut write_accepted = Vec::new();
        let mut ephemeral = VecDeque::new();
        let mut ephemeral_write_accepted = Vec::new();

        assert!(matches!(
            flush_generation_writes(
                false,
                &mut pending,
                &mut durable,
                &mut write_accepted,
                &mut ephemeral,
                &mut ephemeral_write_accepted,
                &mut socket,
                &event_tx,
                1,
                7,
            ),
            FlushResult::Flushed
        ));
        assert_eq!(pending, ["held"], "closed gate cannot consume queued wire");

        assert!(matches!(
            flush_generation_writes(
                true,
                &mut pending,
                &mut durable,
                &mut write_accepted,
                &mut ephemeral,
                &mut ephemeral_write_accepted,
                &mut socket,
                &event_tx,
                1,
                7,
            ),
            FlushResult::Flushed
        ));
        assert!(pending.is_empty());
        assert!(matches!(
            peer.read().unwrap(),
            Message::Text(text) if text == "held"
        ));

        let (command_tx, command_rx) = mpsc::channel();
        command_tx
            .send(WorkerCommand::ReleaseInitialRead { generation: 6 })
            .unwrap();
        let mut preamble = Vec::new();
        let mut released = false;
        assert!(matches!(
            drain_commands(
                &command_rx,
                &mut pending,
                &mut preamble,
                &mut durable,
                &mut ephemeral,
                &mut released,
                &event_tx,
                1,
                7,
            ),
            Drain::Continue
        ));
        assert!(!released, "stale generation release must be inert");
    }

    #[test]
    fn generation_end_classifies_queued_and_write_accepted_exactly() {
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let queued = AttemptCorrelation(10);
        let accepted = AttemptCorrelation(11);
        let mut durable = VecDeque::from([(queued, "queued".to_string())]);
        let mut write_accepted = vec![accepted];
        let mut ephemeral = VecDeque::new();
        let mut ephemeral_write_accepted = Vec::new();

        resolve_generation_end(
            &event_tx,
            3,
            7,
            &mut durable,
            &mut write_accepted,
            &mut ephemeral,
            &mut ephemeral_write_accepted,
        );

        assert_eq!(
            handoff_results(&event_rx),
            vec![
                (queued, HandoffResult::NotHandedOff),
                (accepted, HandoffResult::Ambiguous),
            ]
        );
        assert!(durable.is_empty());
        assert!(write_accepted.is_empty());
    }

    #[test]
    fn real_socket_write_ok_unconfirmed_flush_then_generation_end_is_ambiguous() {
        let (mut socket, peer) = real_buffered_socket();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let correlation = AttemptCorrelation(31);
        let mut write_accepted = Vec::new();
        begin_real_unconfirmed_write(&mut socket, correlation, &event_tx, &mut write_accepted);

        drop(peer);
        let mut durable = VecDeque::new();
        let mut ephemeral = VecDeque::new();
        let mut ephemeral_write_accepted = Vec::new();
        resolve_generation_end(
            &event_tx,
            1,
            1,
            &mut durable,
            &mut write_accepted,
            &mut ephemeral,
            &mut ephemeral_write_accepted,
        );

        assert_eq!(
            handoff_results(&event_rx),
            vec![(correlation, HandoffResult::Ambiguous)]
        );
    }

    #[test]
    fn successful_control_flush_settles_prior_durable_write_as_written() {
        let (mut socket, mut peer) = real_buffered_socket();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let correlation = AttemptCorrelation(32);
        let mut write_accepted = Vec::new();
        let mut ephemeral_write_accepted = Vec::new();
        begin_real_unconfirmed_write(&mut socket, correlation, &event_tx, &mut write_accepted);

        let mut flushed = false;
        for _ in 0..512 {
            drain_peer(&mut peer);
            match flush_message(
                &mut socket,
                Message::Ping(Vec::new().into()),
                &mut write_accepted,
                &mut ephemeral_write_accepted,
                &event_tx,
                1,
                1,
            ) {
                FlushResult::Flushed => {
                    flushed = true;
                    break;
                }
                FlushResult::Blocked => std::thread::yield_now(),
                FlushResult::Broken(message) => panic!("control flush broke: {message}"),
            }
        }
        assert!(
            flushed,
            "peer draining must eventually allow a control flush"
        );
        assert!(write_accepted.is_empty());
        assert_eq!(
            handoff_results(&event_rx),
            vec![(correlation, HandoffResult::Written)]
        );

        let mut durable = VecDeque::new();
        let mut ephemeral = VecDeque::new();
        resolve_generation_end(
            &event_tx,
            1,
            1,
            &mut durable,
            &mut write_accepted,
            &mut ephemeral,
            &mut ephemeral_write_accepted,
        );
        assert!(
            handoff_results(&event_rx).is_empty(),
            "generation end cannot resolve the already-Written correlation twice"
        );
    }

    #[test]
    fn permanent_disconnect_drains_every_accepted_durable_command_once() {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let first = AttemptCorrelation(21);
        let second = AttemptCorrelation(22);
        let shutdown = Arc::new(AtomicBool::new(false));
        let drain = std::thread::spawn(move || {
            drain_permanently_disconnected(&command_rx, &event_tx, &shutdown, 1, 9);
        });
        command_tx
            .send(WorkerCommand::SendDurable {
                generation: 9,
                correlation: first,
                frame: "first".to_string(),
            })
            .unwrap();
        command_tx.send(WorkerCommand::Send("req".into())).unwrap();
        command_tx
            .send(WorkerCommand::SendDurable {
                generation: 9,
                correlation: second,
                frame: "second".to_string(),
            })
            .unwrap();
        command_tx.send(WorkerCommand::Shutdown).unwrap();
        drain.join().unwrap();

        assert_eq!(
            handoff_results(&event_rx),
            vec![
                (first, HandoffResult::NotHandedOff),
                (second, HandoffResult::NotHandedOff),
            ]
        );
    }

    /// The #506 Fix 2 durable-resolution regression guard: retiring a worker
    /// (out-of-band shutdown flag) with `SendDurable` commands STILL in the
    /// bounded channel must resolve every one `NotHandedOff` — never silently
    /// drop a correlation when `command_rx` drops on exit (issue #93's
    /// resolve-exactly-once). This drives the WORST case: the flag is already
    /// set, so `drain_permanently_disconnected` observes it on its very first
    /// check having drained zero commands, and must still settle the whole
    /// queued burst before returning. Before the fix, that path abandoned
    /// every queued durable.
    #[test]
    fn shutdown_flag_exit_resolves_every_queued_durable_not_handed_off() {
        let (command_tx, command_rx) = mpsc::sync_channel::<WorkerCommand>(8);
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);

        let first = AttemptCorrelation(41);
        let second = AttemptCorrelation(42);
        let third = AttemptCorrelation(43);
        // A non-empty queue, durables interleaved with non-durable traffic.
        command_tx
            .send(WorkerCommand::SendDurable {
                generation: 5,
                correlation: first,
                frame: "a".to_string(),
            })
            .unwrap();
        command_tx.send(WorkerCommand::Send("req".into())).unwrap();
        command_tx
            .send(WorkerCommand::SendDurable {
                generation: 5,
                correlation: second,
                frame: "b".to_string(),
            })
            .unwrap();
        command_tx
            .send(WorkerCommand::SetReconnectPreamble(vec![]))
            .unwrap();
        command_tx
            .send(WorkerCommand::SendDurable {
                generation: 5,
                correlation: third,
                frame: "c".to_string(),
            })
            .unwrap();

        // Flag ALREADY set: the first loop check fires before any recv, so the
        // exit path itself must drain + resolve the whole queue.
        let shutdown = Arc::new(AtomicBool::new(true));
        drain_permanently_disconnected(&command_rx, &event_tx, &shutdown, 1, 5);

        let mut results = handoff_results(&event_rx);
        results.sort_by_key(|(correlation, _)| correlation.0);
        assert_eq!(
            results,
            vec![
                (first, HandoffResult::NotHandedOff),
                (second, HandoffResult::NotHandedOff),
                (third, HandoffResult::NotHandedOff),
            ],
            "every queued durable must resolve exactly once on retirement, none dropped"
        );
    }

    fn test_worker_handle(
        command_tx: SyncSender<WorkerCommand>,
    ) -> (WorkerHandle, Arc<Mutex<Option<Waker>>>, Arc<AtomicBool>) {
        let waker_slot: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = WorkerHandle {
            command_tx,
            shutdown: Arc::clone(&shutdown),
            waker: Arc::clone(&waker_slot),
            // No real worker thread backs this handle in these tests --
            // `retire`/`push` never touch `join` (`retire` only takes it out
            // and hands it back), so a trivially-finished thread is a
            // faithful enough stand-in.
            join: Some(thread::spawn(|| {})),
        };
        (handle, waker_slot, shutdown)
    }

    /// The HIGH falsifier (issue #506): a stalled-but-connected relay must
    /// no longer be able to grow its outbound queue without bound.
    /// `WorkerHandle::push` now uses `try_send` against the bounded channel
    /// (`PoolConfig::command_queue_capacity`), so a saturated queue reports
    /// `false` -- the EXACT signal `Pool::send`/`send_durable` already turn
    /// into "not handed off" backpressure -- instead of silently succeeding
    /// forever.
    #[test]
    fn push_reports_backpressure_once_the_bounded_queue_is_full() {
        let (command_tx, command_rx) = mpsc::sync_channel::<WorkerCommand>(2);
        let (handle, _waker_slot, _shutdown) = test_worker_handle(command_tx);

        assert!(handle.push(WorkerCommand::Send("a".into())));
        assert!(handle.push(WorkerCommand::Send("b".into())));
        assert!(
            !handle.push(WorkerCommand::Send("c".into())),
            "a full bounded queue must report backpressure (false), \
             never grow past its configured capacity"
        );

        // Draining one slot must free exactly one more `push`.
        assert!(matches!(command_rx.recv(), Ok(WorkerCommand::Send(text)) if text == "a"));
        assert!(handle.push(WorkerCommand::Send("d".into())));
        assert!(
            !handle.push(WorkerCommand::Send("e".into())),
            "capacity is bounded, not one-shot -- it stays saturated at N \
             in-flight commands"
        );

        drop(command_rx);
        handle.join.expect("join handle retained").join().unwrap();
    }

    /// The deadlock falsifier (issue #506 Fix 2): `retire` must be
    /// non-blocking even when the bounded command queue is FULL and NOBODY is
    /// draining it. That "full + undrained" state is exactly the worker's
    /// situation in the whole-pool deadlock -- it is transitively blocked on a
    /// full `event_tx` (waiting on the translator, which needs the pool lock
    /// the retiring thread holds), so it cannot drain its command queue. The
    /// earlier (rejected) version routed `Shutdown` through a BLOCKING `send`
    /// on this same queue: under this precondition that send parks forever,
    /// the lock is never released, and the whole pool wedges. This test would
    /// hang on that version (caught by the timeout below) and passes on the
    /// atomic-flag design, which never touches the data queue to signal
    /// shutdown.
    #[test]
    fn retire_is_non_blocking_when_the_command_queue_is_full_and_undrained() {
        let (command_tx, command_rx) = mpsc::sync_channel::<WorkerCommand>(1);
        command_tx
            .send(WorkerCommand::Send("only-slot".into()))
            .unwrap();
        assert!(
            command_tx
                .try_send(WorkerCommand::Send("overflow".into()))
                .is_err(),
            "the command queue must be observably full for this falsifier to mean anything"
        );

        let (handle, _waker_slot, shutdown) = test_worker_handle(command_tx);

        // Drive retire on its own thread and REQUIRE prompt completion. There
        // is deliberately NO drainer: the only way this finishes is if retire
        // never blocks on the full queue. A blocking `send` would park this
        // thread forever and the timeout below would fire.
        let (done_tx, done_rx) = mpsc::channel();
        let retired = std::thread::spawn(move || {
            let join = handle.retire();
            let _ = done_tx.send(());
            join
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("retire must not block on a full, undrained command queue (#506)");

        // Shutdown is signalled out-of-band, without consuming a queue slot.
        assert!(
            shutdown.load(Ordering::SeqCst),
            "retire must set the terminal atomic as the authoritative signal"
        );
        // The pre-existing command is untouched: retire never needed to drain
        // it (and could not have -- the queue was full). The best-effort
        // `Shutdown` nudge was simply dropped, which is safe.
        assert!(
            matches!(command_rx.recv(), Ok(WorkerCommand::Send(text)) if text == "only-slot"),
            "the queued data command must survive retirement intact"
        );

        let join = retired.join().expect("retire thread must not panic");
        join.join().expect("stand-in worker join");
        drop(command_rx);
    }

    /// Companion to the deadlock falsifier: when the command queue has room,
    /// the best-effort `Shutdown` nudge DOES land on the channel (so a worker
    /// parked in a `recv`-based wait -- backoff / permanent-drain, where the
    /// mio waker is inactive -- is unparked immediately, not only via the
    /// atomic on the next timeout). Proves the nudge is wired, complementing
    /// the "dropped-if-full is safe" case above.
    #[test]
    fn retire_nudges_the_channel_when_the_queue_has_room() {
        let (command_tx, command_rx) = mpsc::sync_channel::<WorkerCommand>(1);
        let (handle, _waker_slot, shutdown) = test_worker_handle(command_tx);

        let join = handle.retire();

        assert!(shutdown.load(Ordering::SeqCst));
        assert!(
            matches!(command_rx.recv(), Ok(WorkerCommand::Shutdown)),
            "with room in the queue, retire's nudge must reach a recv-parked worker"
        );
        join.join().expect("stand-in worker join");
    }

    #[test]
    fn stale_ephemeral_command_is_rejected_before_any_send_queue() {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let (completion, completion_rx) = ephemeral_completion();
        command_tx
            .send(WorkerCommand::SendEphemeral {
                generation: 7,
                frame: "auth".to_string(),
                completion,
            })
            .unwrap();

        let mut pending = VecDeque::new();
        let mut preamble = Vec::new();
        let mut durable = VecDeque::new();
        let mut ephemeral = VecDeque::new();
        let mut outbound_released = false;
        assert!(matches!(
            drain_commands(
                &command_rx,
                &mut pending,
                &mut preamble,
                &mut durable,
                &mut ephemeral,
                &mut outbound_released,
                &event_tx,
                4,
                8,
            ),
            Drain::Continue
        ));

        assert_eq!(
            completion_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            EphemeralSendOutcome::Unavailable
        );
        assert!(pending.is_empty());
        assert!(preamble.is_empty());
        assert!(durable.is_empty());
        assert!(ephemeral.is_empty());
        assert!(event_rx.try_recv().is_err(), "no write correlation emitted");
    }

    #[test]
    fn exact_ephemeral_command_stays_separate_and_dies_with_generation() {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let (completion, completion_rx) = ephemeral_completion();
        command_tx
            .send(WorkerCommand::SendEphemeral {
                generation: 9,
                frame: "auth".to_string(),
                completion,
            })
            .unwrap();

        let mut pending = VecDeque::new();
        let mut preamble = vec!["req-preamble".to_string()];
        let mut durable = VecDeque::new();
        let mut ephemeral = VecDeque::new();
        let mut outbound_released = false;
        assert!(matches!(
            drain_commands(
                &command_rx,
                &mut pending,
                &mut preamble,
                &mut durable,
                &mut ephemeral,
                &mut outbound_released,
                &event_tx,
                2,
                9,
            ),
            Drain::Continue
        ));
        assert!(pending.is_empty(), "AUTH never enters ordinary pending");
        assert_eq!(preamble, ["req-preamble"]);
        assert!(durable.is_empty(), "AUTH never enters durable EVENT state");
        assert_eq!(ephemeral.len(), 1);
        assert!(completion_rx.try_recv().is_err());

        let mut write_accepted = Vec::new();
        let mut ephemeral_write_accepted = Vec::new();
        resolve_generation_end(
            &event_tx,
            2,
            9,
            &mut durable,
            &mut write_accepted,
            &mut ephemeral,
            &mut ephemeral_write_accepted,
        );
        assert_eq!(
            completion_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            EphemeralSendOutcome::Unavailable
        );
        assert!(event_rx.try_recv().is_err(), "no write correlation emitted");
    }

    #[test]
    fn reconnect_wait_rejects_ephemeral_instead_of_carrying_it() {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let (completion, completion_rx) = ephemeral_completion();
        command_tx
            .send(WorkerCommand::SendEphemeral {
                generation: 10,
                frame: "auth".to_string(),
                completion,
            })
            .unwrap();
        command_tx.send(WorkerCommand::Shutdown).unwrap();

        let shutdown = AtomicBool::new(false);
        let mut pending = VecDeque::new();
        let mut preamble = vec!["req-preamble".to_string()];
        assert!(!wait_before_reconnect(
            &command_rx,
            &mut pending,
            &mut preamble,
            Duration::from_secs(1),
            &event_tx,
            &shutdown,
            6,
            11,
        ));
        assert_eq!(
            completion_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            EphemeralSendOutcome::Unavailable
        );
        assert!(pending.is_empty());
        assert_eq!(preamble, ["req-preamble"]);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn successful_ephemeral_flush_accepts_once_without_write_correlation() {
        let (mut socket, _peer) = real_buffered_socket();
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);
        let (completion, completion_rx) = ephemeral_completion();
        let mut pending = VecDeque::new();
        let mut durable = VecDeque::new();
        let mut write_accepted = Vec::new();
        let mut ephemeral = VecDeque::from([EphemeralFrame {
            frame: "auth".to_string(),
            completion,
        }]);
        let mut ephemeral_write_accepted = Vec::new();

        assert!(matches!(
            flush_writes(
                &mut pending,
                &mut durable,
                &mut write_accepted,
                &mut ephemeral,
                &mut ephemeral_write_accepted,
                &mut socket,
                &event_tx,
                1,
                22,
            ),
            FlushResult::Flushed
        ));
        assert_eq!(
            completion_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            EphemeralSendOutcome::Accepted
        );
        assert!(completion_rx.try_recv().is_err(), "completion is one-shot");
        assert!(event_rx.try_recv().is_err(), "no write correlation emitted");
        assert!(pending.is_empty());
        assert!(durable.is_empty());
        assert!(ephemeral.is_empty());
        assert!(ephemeral_write_accepted.is_empty());
    }

    #[test]
    fn dropped_worker_command_resolves_ephemeral_shutdown_baseline() {
        let (completion, completion_rx) = ephemeral_completion();
        drop(WorkerCommand::SendEphemeral {
            generation: 1,
            frame: "auth".to_string(),
            completion,
        });
        assert_eq!(
            completion_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            EphemeralSendOutcome::Unavailable
        );
    }

    /// Master-only path (the #506 Fix 2 flag-observed exit): retiring a
    /// worker with a `SendEphemeral` still in the bounded channel must
    /// resolve its completion `Unavailable` explicitly, exactly where queued
    /// durables are resolved `NotHandedOff` — never rely on the `Drop`
    /// backstop for a channel-resident command at retirement.
    #[test]
    fn shutdown_flag_exit_resolves_queued_ephemeral_unavailable() {
        let (command_tx, command_rx) = mpsc::sync_channel::<WorkerCommand>(8);
        let (event_tx, event_rx) = mpsc::sync_channel(TEST_EVENT_QUEUE_CAPACITY);

        let queued = AttemptCorrelation(51);
        let (completion, completion_rx) = ephemeral_completion();
        command_tx
            .send(WorkerCommand::SendDurable {
                generation: 5,
                correlation: queued,
                frame: "a".to_string(),
            })
            .unwrap();
        command_tx
            .send(WorkerCommand::SendEphemeral {
                generation: 5,
                frame: "auth".to_string(),
                completion,
            })
            .unwrap();

        // Flag ALREADY set: the exit path itself settles the whole queue.
        let shutdown = Arc::new(AtomicBool::new(true));
        drain_permanently_disconnected(&command_rx, &event_tx, &shutdown, 1, 5);

        assert_eq!(
            handoff_results(&event_rx),
            vec![(queued, HandoffResult::NotHandedOff)]
        );
        assert_eq!(
            completion_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            EphemeralSendOutcome::Unavailable
        );
    }
}
