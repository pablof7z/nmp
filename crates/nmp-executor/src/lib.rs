//! A finite, zero-queue owner for NMP's blocking native bridge tasks.
//!
//! These tasks drain one blocking receiver for their whole lifetime. Putting
//! them behind a conventional fixed worker pool would accept later streams
//! into a queue that cannot run while earlier drains remain live. This owner
//! therefore reserves one of a finite number of immediately-startable slots
//! before a caller accepts the corresponding stream or operation. Saturation
//! is synchronous and typed; accepted tasks are never queued or truncated.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

/// Mobile/hosted default: enough for several simultaneous views plus a NIP-46
/// session and receipts, while keeping the executor's contribution below the
/// default relay envelope's worst-case live+retiring worker count. Hosts with
/// a measured need can raise it explicitly; it is never inferred from CPUs.
pub const DEFAULT_MAX_TASKS: usize = 12;

static NEXT_EXECUTOR_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT_EXECUTOR: Cell<u64> = const { Cell::new(0) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saturated {
    pub component: String,
    pub capacity: usize,
}

impl fmt::Display for Saturated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} refused: native task executor is at its finite capacity {}",
            self.component, self.capacity
        )
    }
}

impl std::error::Error for Saturated {}

#[derive(Debug)]
pub enum BuildError {
    ThreadUnavailable(std::io::Error),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadUnavailable(error) => write!(f, "executor reaper unavailable: {error}"),
        }
    }
}

impl std::error::Error for BuildError {}

#[derive(Debug)]
pub enum SpawnError {
    ThreadUnavailable {
        component: String,
        error: std::io::Error,
    },
    ExecutorShutDown {
        component: String,
    },
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadUnavailable { component, error } => {
                write!(f, "{component} thread unavailable: {error}")
            }
            Self::ExecutorShutDown { component } => {
                write!(f, "{component} refused: native task executor is shut down")
            }
        }
    }
}

impl std::error::Error for SpawnError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Census {
    pub capacity: usize,
    pub admitted: usize,
    pub running: usize,
    pub accepting: bool,
}

struct State {
    accepting: bool,
    admitted: usize,
    reservations: HashSet<u64>,
    running: HashMap<u64, RunningTask>,
    reaper_done: bool,
}

struct RunningTask {
    join: JoinHandle<()>,
    cancel: Arc<dyn Fn() + Send + Sync>,
}

struct Shared {
    capacity: usize,
    state: Mutex<State>,
    changed: Condvar,
}

enum ReaperMsg {
    Completed(u64),
    SlotReleased,
    Shutdown,
}

struct Core {
    id: u64,
    shared: Arc<Shared>,
    reaper_tx: Sender<ReaperMsg>,
}

/// A cloneable handle to one finite task owner. The executor never queues:
/// [`reserve`](Executor::reserve) either claims an immediately-startable slot
/// or returns [`Saturated`].
#[derive(Clone)]
pub struct Executor {
    core: Arc<Core>,
}

impl Executor {
    pub fn new(capacity: usize) -> Result<Self, BuildError> {
        Self::new_with_reaper_spawn(capacity, |builder, task| builder.spawn(task))
    }

    fn new_with_reaper_spawn(
        capacity: usize,
        spawn: impl FnOnce(
            thread::Builder,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<JoinHandle<()>>,
    ) -> Result<Self, BuildError> {
        let capacity = if capacity == 0 {
            DEFAULT_MAX_TASKS
        } else {
            capacity
        };
        let id = NEXT_EXECUTOR_ID.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared {
            capacity,
            state: Mutex::new(State {
                accepting: true,
                admitted: 0,
                reservations: HashSet::new(),
                running: HashMap::new(),
                reaper_done: false,
            }),
            changed: Condvar::new(),
        });
        let (reaper_tx, reaper_rx) = mpsc::channel();
        let reaper_shared = Arc::clone(&shared);
        spawn(
            thread::Builder::new().name("nmp-native-task-reaper".to_string()),
            Box::new(move || reaper_loop(reaper_shared, reaper_rx)),
        )
        .map_err(BuildError::ThreadUnavailable)?;
        Ok(Self {
            core: Arc::new(Core {
                id,
                shared,
                reaper_tx,
            }),
        })
    }

    pub fn reserve(&self, component: impl Into<String>) -> Result<Reservation, Saturated> {
        let component = component.into();
        let mut state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state.accepting || state.admitted >= self.core.shared.capacity {
            return Err(Saturated {
                component,
                capacity: self.core.shared.capacity,
            });
        }
        let reservation_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
        state.admitted += 1;
        state.reservations.insert(reservation_id);
        drop(state);
        Ok(Reservation {
            core: Arc::clone(&self.core),
            component,
            reservation_id,
            live: true,
        })
    }

    pub fn spawn_with_cancel(
        &self,
        component: impl Into<String>,
        cancel: impl Fn() + Send + Sync + 'static,
        task: impl FnOnce() + Send + 'static,
    ) -> Result<(), ExecutorError> {
        self.reserve(component)
            .map_err(ExecutorError::Saturated)?
            .spawn_with_cancel(cancel, task)
            .map_err(ExecutorError::Spawn)
    }

    pub fn census(&self) -> Census {
        let state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Census {
            capacity: self.core.shared.capacity,
            admitted: state.admitted,
            running: state.running.len(),
            accepting: state.accepting,
        }
    }

    /// Event-driven lifecycle barrier used by teardown/census proofs. It
    /// returns only after the reaper has joined every task admitted so far.
    pub fn wait_for_idle(&self) {
        let mut state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while state.admitted != 0 {
            state = self
                .core
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    /// Refuse new work and wait until every admitted task is joined. When a
    /// callback invokes shutdown from one of this executor's own tasks, it
    /// only requests shutdown; waiting there would deadlock that callback.
    /// The reaper still joins the task and returns the census to baseline.
    pub fn shutdown(&self) {
        let cancellations = {
            let mut state = self
                .core
                .shared
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if state.accepting {
                state.accepting = false;
            }
            let abandoned = state.reservations.len();
            state.reservations.clear();
            state.admitted -= abandoned;
            self.core.shared.changed.notify_all();
            state
                .running
                .values()
                .map(|task| Arc::clone(&task.cancel))
                .collect::<Vec<_>>()
        };
        // Cancellation runs outside the state lock: component teardown may
        // synchronously cause a task to complete and notify the reaper.
        for cancel in cancellations {
            cancel();
        }
        let _ = self.core.reaper_tx.send(ReaperMsg::Shutdown);
        let on_own_task = CURRENT_EXECUTOR.with(|current| current.get() == self.core.id);
        if on_own_task {
            return;
        }
        let mut state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while !state.reaper_done {
            state = self
                .core
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let cancellations = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.accepting = false;
            let abandoned = state.reservations.len();
            state.reservations.clear();
            state.admitted -= abandoned;
            self.shared.changed.notify_all();
            state
                .running
                .values()
                .map(|task| Arc::clone(&task.cancel))
                .collect::<Vec<_>>()
        };
        for cancel in cancellations {
            cancel();
        }
        let _ = self.reaper_tx.send(ReaperMsg::Shutdown);
    }
}

#[derive(Debug)]
pub enum ExecutorError {
    Saturated(Saturated),
    Spawn(SpawnError),
}

pub struct Reservation {
    core: Arc<Core>,
    component: String,
    reservation_id: u64,
    live: bool,
}

impl Reservation {
    pub fn spawn_with_cancel(
        self,
        cancel: impl Fn() + Send + Sync + 'static,
        task: impl FnOnce() + Send + 'static,
    ) -> Result<(), SpawnError> {
        self.start_with_cancel(cancel)
            .map(|starter| starter.run(task))
    }

    /// Start and register the OS thread before transferring a stream or
    /// operation into it. The returned starter is a one-shot handoff; dropping
    /// it releases the already-started task without running user work.
    pub fn start_with_cancel(
        self,
        cancel: impl Fn() + Send + Sync + 'static,
    ) -> Result<StartedTask, SpawnError> {
        self.start_with_cancel_and_spawn(cancel, |builder, task| builder.spawn(task))
    }

    fn start_with_cancel_and_spawn(
        mut self,
        cancel: impl Fn() + Send + Sync + 'static,
        spawn: impl FnOnce(
            thread::Builder,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<JoinHandle<()>>,
    ) -> Result<StartedTask, SpawnError> {
        let task_id = self.reservation_id;
        let component = self.component.clone();
        let thread_name = format!("nmp-native-{}", sanitize_name(&component));
        let executor_id = self.core.id;
        let completed = self.core.reaper_tx.clone();
        let (start_tx, start_rx) = mpsc::sync_channel::<Box<dyn FnOnce() + Send + 'static>>(0);
        // Hold the state lock across OS spawn and the reservation->running
        // transition. Shutdown can therefore either invalidate the untouched
        // reservation or observe the registered cancellable task, never a
        // spawned-but-unowned gap between the two.
        let mut state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state.reservations.contains(&task_id) {
            self.live = false;
            return Err(SpawnError::ExecutorShutDown { component });
        }
        let handle = match spawn(
            thread::Builder::new().name(thread_name),
            Box::new(move || {
                CURRENT_EXECUTOR.with(|current| current.set(executor_id));
                let outcome = start_rx
                    .recv()
                    .map(|task| std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)));
                CURRENT_EXECUTOR.with(|current| current.set(0));
                let _ = completed.send(ReaperMsg::Completed(task_id));
                if let Ok(Err(payload)) = outcome {
                    std::panic::resume_unwind(payload);
                }
            }),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                state.reservations.remove(&task_id);
                state.admitted -= 1;
                self.core.shared.changed.notify_all();
                self.live = false;
                return Err(SpawnError::ThreadUnavailable { component, error });
            }
        };
        state.reservations.remove(&task_id);
        state.running.insert(
            task_id,
            RunningTask {
                join: handle,
                cancel: Arc::new(cancel),
            },
        );
        drop(state);
        self.live = false;
        Ok(StartedTask { start: start_tx })
    }

    fn release(&mut self) {
        if !self.live {
            return;
        }
        self.live = false;
        let mut state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.reservations.remove(&self.reservation_id) {
            state.admitted -= 1;
        }
        drop(state);
        let _ = self.core.reaper_tx.send(ReaperMsg::SlotReleased);
    }
}

pub struct StartedTask {
    start: mpsc::SyncSender<Box<dyn FnOnce() + Send + 'static>>,
}

impl StartedTask {
    pub fn run(self, task: impl FnOnce() + Send + 'static) {
        let _ = self.start.send(Box::new(task));
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.release();
    }
}

fn sanitize_name(component: &str) -> String {
    component
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .take(40)
        .collect()
}

fn reaper_loop(shared: Arc<Shared>, rx: mpsc::Receiver<ReaperMsg>) {
    let mut shutdown = false;
    while let Ok(message) = rx.recv() {
        match message {
            ReaperMsg::Completed(task_id) => {
                let handle = {
                    let mut state = shared
                        .state
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    state.running.remove(&task_id).map(|task| task.join)
                };
                if let Some(handle) = handle {
                    let _ = handle.join();
                    let mut state = shared
                        .state
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    state.admitted -= 1;
                    shared.changed.notify_all();
                }
            }
            ReaperMsg::SlotReleased => {}
            ReaperMsg::Shutdown => shutdown = true,
        }
        let done = {
            let state = shared
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shutdown && state.admitted == 0
        };
        if done {
            break;
        }
    }
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    state.reaper_done = true;
    shared.changed.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const REPRESENTATIVE_COMPOSED_PEAK: [&str; 11] = [
        "row observer",
        "demand observer",
        "follow projection",
        "follow FFI bridge",
        "receipt observer",
        "NIP-46 connection",
        "NIP-46 session",
        "NIP-46 event forwarder",
        "NIP-46 switch-relays",
        "NIP-46 result mapper",
        "engine signer waiter",
    ];

    fn hold_task(executor: &Executor, component: &str) {
        let (release_tx, release_rx) = mpsc::channel();
        executor
            .spawn_with_cancel(
                component,
                move || {
                    let _ = release_tx.send(());
                },
                move || {
                    let _ = release_rx.recv();
                },
            )
            .unwrap();
    }

    #[test]
    fn default_fits_representative_transient_peak_with_one_slot_headroom() {
        assert_eq!(DEFAULT_MAX_TASKS, 12);
        let executor = Executor::new(0).unwrap();
        for component in REPRESENTATIVE_COMPOSED_PEAK {
            hold_task(&executor, component);
        }
        assert_eq!(executor.census().admitted, 11);

        hold_task(&executor, "headroom");
        assert_eq!(executor.census().admitted, DEFAULT_MAX_TASKS);
        assert!(matches!(
            executor.reserve("beyond default"),
            Err(Saturated { capacity: 12, .. })
        ));
        executor.shutdown();
        assert_eq!(executor.census().admitted, 0);
        assert_eq!(executor.census().running, 0);
    }

    #[test]
    fn eight_slots_cannot_hold_the_representative_signing_peak() {
        let executor = Executor::new(8).unwrap();
        for component in REPRESENTATIVE_COMPOSED_PEAK.iter().take(8) {
            hold_task(&executor, component);
        }
        let refusal = match executor.reserve(REPRESENTATIVE_COMPOSED_PEAK[8]) {
            Ok(_) => panic!("eight slots must refuse the ninth representative task"),
            Err(error) => error,
        };
        assert_eq!(refusal.capacity, 8);
        assert_eq!(refusal.component, "NIP-46 switch-relays");
        executor.shutdown();
    }

    #[test]
    fn exact_cap_refuses_without_queueing_and_reuses_joined_slot() {
        let executor = Executor::new(1).unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let cancel = release_tx.clone();
        executor
            .spawn_with_cancel(
                "held",
                move || {
                    let _ = cancel.send(());
                },
                move || release_rx.recv().unwrap(),
            )
            .unwrap();
        assert_eq!(executor.census().admitted, 1);
        let refusal = match executor.reserve("refused") {
            Ok(_) => panic!("the cap-sized executor must refuse another task"),
            Err(error) => error,
        };
        assert_eq!(
            refusal,
            Saturated {
                component: "refused".to_string(),
                capacity: 1,
            }
        );
        release_tx.send(()).unwrap();
        executor.wait_for_idle();
        let reservation = executor.reserve("next").unwrap();
        reservation.spawn_with_cancel(|| {}, || {}).unwrap();
        executor.shutdown();
        assert_eq!(executor.census().admitted, 0);
        assert_eq!(executor.census().running, 0);
    }

    #[test]
    fn dropping_a_reservation_returns_exact_baseline() {
        let executor = Executor::new(1).unwrap();
        drop(executor.reserve("reserved").unwrap());
        assert_eq!(executor.census().admitted, 0);
        executor.shutdown();
    }

    #[test]
    fn injected_reaper_refusal_is_typed_before_an_executor_escapes() {
        let error = match Executor::new_with_reaper_spawn(1, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "injected reaper pressure",
            ))
        }) {
            Ok(_) => panic!("injected reaper refusal must fail construction"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "executor reaper unavailable: injected reaper pressure"
        );
    }

    #[test]
    fn injected_task_spawn_refusal_releases_its_reserved_slot_exactly() {
        let executor = Executor::new(1).unwrap();
        let reservation = executor.reserve("refused-task").unwrap();
        let error = match reservation.start_with_cancel_and_spawn(
            || {},
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "injected task pressure",
                ))
            },
        ) {
            Ok(_) => panic!("injected task refusal must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("injected task pressure"));
        assert_eq!(executor.census().admitted, 0);
        assert_eq!(executor.census().running, 0);
        executor.shutdown();
    }

    #[test]
    fn shutdown_invalidates_a_forgotten_reservation_without_hanging() {
        let executor = Executor::new(1).unwrap();
        let reservation = executor.reserve("forgotten").unwrap();
        executor.shutdown();
        assert_eq!(executor.census().admitted, 0);
        let error = match reservation.start_with_cancel(|| {}) {
            Ok(_) => panic!("a shutdown-invalidated reservation must not start"),
            Err(error) => error,
        };
        assert!(matches!(error, SpawnError::ExecutorShutDown { .. }));
    }

    #[test]
    fn shutdown_wakes_and_joins_an_idle_blocking_task_without_a_timeout() {
        let executor = Executor::new(1).unwrap();
        let (wake_tx, wake_rx) = mpsc::channel();
        executor
            .spawn_with_cancel(
                "idle",
                move || {
                    let _ = wake_tx.send(());
                },
                move || {
                    let _ = wake_rx.recv();
                },
            )
            .unwrap();
        executor.shutdown();
        assert_eq!(
            executor.census(),
            Census {
                capacity: 1,
                admitted: 0,
                running: 0,
                accepting: false,
            }
        );
    }

    #[test]
    fn callback_initiated_shutdown_is_two_phase_and_reaches_exact_zero() {
        let executor = Executor::new(1).unwrap();
        let callback_executor = executor.clone();
        let (returned_tx, returned_rx) = mpsc::channel();
        executor
            .spawn_with_cancel(
                "callback",
                || {},
                move || {
                    callback_executor.shutdown();
                    returned_tx.send(()).unwrap();
                },
            )
            .unwrap();
        returned_rx.recv().unwrap();
        executor.wait_for_idle();
        executor.shutdown();
        assert_eq!(executor.census().admitted, 0);
        assert_eq!(executor.census().running, 0);
        assert!(!executor.census().accepting);
    }

    #[test]
    fn panicking_task_still_releases_its_slot_and_joins() {
        let executor = Executor::new(1).unwrap();
        executor
            .spawn_with_cancel("panicking", || {}, || panic!("injected task panic"))
            .unwrap();
        executor.wait_for_idle();
        assert_eq!(executor.census().admitted, 0);
        assert_eq!(executor.census().running, 0);
        executor.shutdown();
    }
}
