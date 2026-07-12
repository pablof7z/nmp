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

use std::collections::VecDeque;
use std::io;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

use crate::backoff;
use crate::keepalive::{KeepaliveAction, KeepaliveState};

use super::connect::{open_relay_socket, RelaySocket};
use super::frame::classify_message;
use super::{AttemptCorrelation, HandoffResult, RelayFrame};

const SOCKET: Token = Token(0);
const CONTROL: Token = Token(1);

/// Command the pool pushes to one relay worker.
pub(super) enum WorkerCommand {
    Send(String),
    Shutdown,
    /// Frames replayed at the front of the outbound queue on every
    /// (re)connect, before any newly-enqueued `Send`. Registered by the
    /// engine after observing `Connected` so the current live subscriptions
    /// survive a reconnect without the engine racing the socket.
    SetReconnectPreamble(Vec<String>),
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
}

/// What happened, tagged with the worker's packed `(worker_id, attempt)`
/// generation at the time it happened.
pub(super) enum WorkerEventKind {
    Connected,
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
    command_tx: Sender<WorkerCommand>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl WorkerHandle {
    /// Enqueue `command` and wake the worker if it is currently parked in
    /// `mio::Poll::poll`. Returns `false` only if the worker thread is
    /// already gone (channel disconnected).
    pub(super) fn push(&self, command: WorkerCommand) -> bool {
        if self.command_tx.send(command).is_err() {
            return false;
        }
        if let Ok(guard) = self.waker.lock() {
            if let Some(waker) = guard.as_ref() {
                let _ = waker.wake();
            }
        }
        true
    }
}

/// Spawn the worker thread for one relay slot.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn(
    slot: u32,
    worker_id: u32,
    url: String,
    event_tx: Sender<WorkerEvent>,
    keepalive_idle: Duration,
    keepalive_pong_timeout: Duration,
    reconnect_delay_initial: Duration,
) -> WorkerHandle {
    let (command_tx, command_rx) = mpsc::channel::<WorkerCommand>();
    let waker_slot: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
    let waker_for_thread = Arc::clone(&waker_slot);
    thread::Builder::new()
        .name(format!("nmp-transport-relay-{slot}"))
        .spawn(move || {
            run_worker(
                slot,
                worker_id,
                url,
                event_tx,
                command_rx,
                waker_for_thread,
                keepalive_idle,
                keepalive_pong_timeout,
                reconnect_delay_initial,
            );
        })
        .expect("relay worker thread spawn must succeed");
    WorkerHandle {
        command_tx,
        waker: waker_slot,
    }
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
    url: String,
    event_tx: Sender<WorkerEvent>,
    command_rx: Receiver<WorkerCommand>,
    waker_slot: Arc<Mutex<Option<Waker>>>,
    keepalive_idle: Duration,
    keepalive_pong_timeout: Duration,
    reconnect_delay_initial: Duration,
) {
    let mut pending: VecDeque<String> = VecDeque::new();
    let mut preamble: Vec<String> = Vec::new();
    // Durable EVENT tracking (issue #93): entirely separate from `pending`
    // above, and NEVER carried across a reconnect — each `run_connected`
    // call starts these two empty and `resolve_generation_end` drains both
    // (firing `NotHandedOff`/`Ambiguous`) the instant that call returns, no
    // matter which internal path produced the outcome.
    let mut durable: VecDeque<(AttemptCorrelation, String)> = VecDeque::new();
    let mut write_accepted: Vec<AttemptCorrelation> = Vec::new();
    let mut attempt: u32 = 0;
    let mut backoff_delay = reconnect_delay_initial;

    loop {
        let generation = pack_generation(worker_id, attempt);
        match open_relay_socket(&url) {
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
                let outcome = run_connected(
                    slot,
                    generation,
                    &event_tx,
                    &command_rx,
                    &waker_slot,
                    &mut pending,
                    &mut socket,
                    &mut keepalive,
                    &mut preamble,
                    &mut durable,
                    &mut write_accepted,
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
                                slot,
                                generation,
                            );
                            return;
                        }
                        let base = retry_in.expect("retry_in set above for non-permanent");
                        let delay = backoff::jittered(base, &url);
                        attempt = attempt.wrapping_add(1);
                        if !wait_before_reconnect(
                            &command_rx,
                            &mut pending,
                            &mut preamble,
                            delay,
                            &event_tx,
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
                    drain_permanently_disconnected(&command_rx, &event_tx, slot, generation);
                    return;
                }
                let base = retry_in.expect("retry_in set above for non-permanent");
                let delay = backoff::jittered(base, &url);
                attempt = attempt.wrapping_add(1);
                if !wait_before_reconnect(
                    &command_rx,
                    &mut pending,
                    &mut preamble,
                    delay,
                    &event_tx,
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
/// failure until the pool explicitly closes the slot. This closes the race
/// between `Pool::send_durable` successfully enqueueing a command and the
/// worker returning after its final dial/session failure: every command the
/// sender accepted before the pool observed the permanent failure is
/// drained and resolved `NotHandedOff`, while commands submitted after the
/// health transition are rejected synchronously by `PoolInner`.
fn drain_permanently_disconnected(
    command_rx: &Receiver<WorkerCommand>,
    event_tx: &Sender<WorkerEvent>,
    slot: u32,
    generation: u64,
) {
    loop {
        match command_rx.recv() {
            Ok(WorkerCommand::SendDurable { correlation, .. }) => resolve_correlation(
                event_tx,
                slot,
                generation,
                correlation,
                HandoffResult::NotHandedOff,
            ),
            Ok(WorkerCommand::Send(_) | WorkerCommand::SetReconnectPreamble(_)) => {}
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
    event_tx: &Sender<WorkerEvent>,
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

/// Resolve every durable `EVENT` still tracked for this generation the
/// instant it ends (issue #93's core invariant — nothing is ever silently
/// carried into the next connection):
/// - `durable` (still queued, never reached `socket.write()`) resolves
///   `NotHandedOff` — provably safe to resubmit under a fresh generation.
/// - `write_accepted` (its own `write()` succeeded, but the shared flush
///   that would confirm it never completed before this generation ended)
///   resolves `Ambiguous` — the bytes MAY have reached the relay, so
///   nothing may treat it as a fresh, never-attempted send.
fn resolve_generation_end(
    event_tx: &Sender<WorkerEvent>,
    slot: u32,
    generation: u64,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
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
    event_tx: &Sender<WorkerEvent>,
    slot: u32,
    generation: u64,
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        match command_rx.recv_timeout(remaining) {
            Ok(WorkerCommand::Send(text)) => pending.push_back(text),
            Ok(WorkerCommand::SetReconnectPreamble(frames)) => *preamble = frames,
            Ok(WorkerCommand::SendDurable { correlation, .. }) => {
                resolve_correlation(
                    event_tx,
                    slot,
                    generation,
                    correlation,
                    HandoffResult::NotHandedOff,
                );
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
    event_tx: &Sender<WorkerEvent>,
    command_rx: &Receiver<WorkerCommand>,
    waker_slot: &Arc<Mutex<Option<Waker>>>,
    pending: &mut VecDeque<String>,
    socket: &mut RelaySocket,
    keepalive: &mut KeepaliveState,
    preamble: &mut Vec<String>,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
) -> ConnectedOutcome {
    let outcome = run_connected_inner(
        slot,
        generation,
        event_tx,
        command_rx,
        waker_slot,
        pending,
        socket,
        keepalive,
        preamble,
        durable,
        write_accepted,
    );
    resolve_generation_end(event_tx, slot, generation, durable, write_accepted);
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_connected_inner(
    slot: u32,
    generation: u64,
    event_tx: &Sender<WorkerEvent>,
    command_rx: &Receiver<WorkerCommand>,
    waker_slot: &Arc<Mutex<Option<Waker>>>,
    pending: &mut VecDeque<String>,
    socket: &mut RelaySocket,
    keepalive: &mut KeepaliveState,
    preamble: &mut Vec<String>,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
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

    loop {
        match drain_commands(
            command_rx, pending, preamble, durable, event_tx, slot, generation,
        ) {
            Drain::Continue => {}
            Drain::Shutdown | Drain::Disconnected => {
                let _ = socket.close(None);
                return ConnectedOutcome::Shutdown;
            }
        }

        let mut wants_write = match flush_writes(
            pending,
            durable,
            write_accepted,
            socket,
            event_tx,
            slot,
            generation,
        ) {
            FlushResult::Flushed => false,
            FlushResult::Blocked => true,
            FlushResult::Broken(message) => {
                return ConnectedOutcome::Reconnect {
                    message,
                    permanent: false,
                }
            }
        };

        match keepalive.step(Instant::now()) {
            KeepaliveAction::Idle => {}
            KeepaliveAction::EmitPing => {
                match flush_message(socket, Message::Ping(Vec::new().into())) {
                    FlushResult::Flushed => keepalive.on_ping_flushed(Instant::now()),
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
        if let Some(outcome) = drain_reads(slot, generation, event_tx, socket, keepalive) {
            return outcome;
        }
    }
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
    event_tx: &Sender<WorkerEvent>,
    slot: u32,
    generation: u64,
) -> Drain {
    loop {
        match command_rx.try_recv() {
            Ok(WorkerCommand::Send(text)) => pending.push_back(text),
            Ok(WorkerCommand::Shutdown) => return Drain::Shutdown,
            Ok(WorkerCommand::SetReconnectPreamble(frames)) => *preamble = frames,
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

/// Write every pending REQ frame, then every queued durable EVENT frame,
/// then flush the socket ONCE for the whole batch — durable frames whose
/// OWN `write()` succeeds move to `write_accepted` (awaiting THIS shared
/// flush to confirm them); only once `flush_socket` itself reports
/// `Flushed` do they resolve `Written` (fired here, the only place that
/// ever fires `Written`). A `Blocked`/`Broken` flush leaves them in
/// `write_accepted` for the caller to resolve later (a subsequent flush
/// attempt, or — on `Broken` — [`resolve_generation_end`] once the
/// connection actually ends): never resolved twice, never resolved early.
#[allow(clippy::too_many_arguments)]
fn flush_writes(
    pending: &mut VecDeque<String>,
    durable: &mut VecDeque<(AttemptCorrelation, String)>,
    write_accepted: &mut Vec<AttemptCorrelation>,
    socket: &mut RelaySocket,
    event_tx: &Sender<WorkerEvent>,
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
    }
    result
}

fn flush_message(socket: &mut RelaySocket, message: Message) -> FlushResult {
    match socket.write(message) {
        Ok(()) => flush_socket(socket),
        Err(error) if is_nonblocking_io(&error) => FlushResult::Blocked,
        Err(error) => FlushResult::Broken(error.to_string()),
    }
}

fn flush_socket(socket: &mut RelaySocket) -> FlushResult {
    match socket.flush() {
        Ok(()) => FlushResult::Flushed,
        Err(error) if is_nonblocking_io(&error) => FlushResult::Blocked,
        Err(error) => FlushResult::Broken(error.to_string()),
    }
}

fn drain_reads(
    slot: u32,
    generation: u64,
    event_tx: &Sender<WorkerEvent>,
    socket: &mut RelaySocket,
    keepalive: &mut KeepaliveState,
) -> Option<ConnectedOutcome> {
    loop {
        match socket.read() {
            Ok(message) => {
                keepalive.on_inbound(Instant::now());
                if let Some(frame) = classify_message(&message) {
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
            Err(error) if is_nonblocking_io(&error) => return None,
            Err(error) => {
                let message = error.to_string();
                let permanent = backoff::is_permanent_error(&message);
                return Some(ConnectedOutcome::Reconnect { message, permanent });
            }
        }
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
    fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        self.poll.poll(&mut self.events, Some(timeout))?;
        Ok(())
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
    fn generation_end_classifies_queued_and_write_accepted_exactly() {
        let (event_tx, event_rx) = mpsc::channel();
        let queued = AttemptCorrelation(10);
        let accepted = AttemptCorrelation(11);
        let mut durable = VecDeque::from([(queued, "queued".to_string())]);
        let mut write_accepted = vec![accepted];

        resolve_generation_end(&event_tx, 3, 7, &mut durable, &mut write_accepted);

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
    fn permanent_disconnect_drains_every_accepted_durable_command_once() {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let first = AttemptCorrelation(21);
        let second = AttemptCorrelation(22);
        let drain = std::thread::spawn(move || {
            drain_permanently_disconnected(&command_rx, &event_tx, 1, 9);
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
}
