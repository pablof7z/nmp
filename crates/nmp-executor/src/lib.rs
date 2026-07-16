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
/// session and receipts, plus one concurrent NIP-11 acquisition, while
/// keeping the executor's contribution below the default relay envelope's
/// worst-case live+retiring worker count. Additional NIP-11 flights are
/// synchronously and safely refused at the same zero-queue boundary. Hosts
/// with a measured need can raise it explicitly; it is never inferred from
/// CPUs.
pub const DEFAULT_MAX_TASKS: usize = 12;

static NEXT_EXECUTOR_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT_TASK: Cell<Option<(u64, TaskId)>> = const { Cell::new(None) };
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

/// Closed, destructor-free identity delivered after one exact native task
/// has been joined and its finite admission slot released. Callers retain
/// any rich release payload in their own registry keyed by this value; the
/// executor reaper can never own or drop caller-defined data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReleaseId(u64);

impl ReleaseId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Opaque identity of one exact admitted task while that task is executing.
///
/// Unlike [`ReleaseId`], this value carries no release edge and has no
/// lifecycle behavior. It is a destructor-free correlation token for an
/// owner that must distinguish reentry from its own task from work running
/// elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

struct ReleaseEdge {
    sender: Sender<ReleaseId>,
    id: ReleaseId,
}

impl ReleaseEdge {
    fn signal(self) {
        let _ = self.sender.send(self.id);
    }
}

enum ReaperMsg {
    Completed(u64, Option<ReleaseEdge>),
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
            reservation_id: Some(reservation_id),
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

    /// Return the exact task currently executing on this executor, if this
    /// call itself is running inside one of its admitted tasks.
    #[doc(hidden)]
    pub fn current_task_id(&self) -> Option<TaskId> {
        CURRENT_TASK.with(|current| {
            current
                .get()
                .and_then(|(executor_id, task_id)| (executor_id == self.core.id).then_some(task_id))
        })
    }

    /// Event-driven lifecycle barrier used by teardown/census proofs. It
    /// returns only after the reaper has joined every task admitted so far.
    pub fn wait_for_idle(&self) {
        self.wait_for_idle_with(|| {});
    }

    fn wait_for_idle_with(&self, mut before_wait: impl FnMut()) {
        let mut state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while state.admitted != 0 {
            before_wait();
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
        let on_own_task = self.current_task_id().is_some();
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
    /// `Some` while the reservation is live and unclaimed; taken (leaving
    /// `None`) the moment it is either started or released, so a
    /// double-release or a release-after-start is unrepresentable instead of
    /// relying on a separate liveness flag.
    reservation_id: Option<u64>,
}

impl Reservation {
    /// Exact destructor-free identity that will be installed in task-local
    /// state if this reservation is started. Only a live (not yet started or
    /// released) reservation has one — matching every call site, which reads
    /// it before `start_with_cancel` consumes the reservation.
    #[doc(hidden)]
    pub fn task_id(&self) -> TaskId {
        TaskId(
            self.reservation_id
                .expect("Reservation::task_id read after the reservation was consumed"),
        )
    }

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
        // Taking the id here marks the reservation consumed for the rest of
        // this call, on every path (early-return, spawn failure, or success)
        // — mirroring the old `self.live = false` that was set on all three
        // paths, but now impossible to forget on a future new path.
        let task_id = self
            .reservation_id
            .take()
            .expect("Reservation::start_with_cancel_and_spawn called twice");
        let component = self.component.clone();
        let thread_name = format!("nmp-native-{}", sanitize_name(&component));
        let executor_id = self.core.id;
        let completed = self.core.reaper_tx.clone();
        let (start_tx, start_rx) = mpsc::sync_channel::<StartedWork>(0);
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
            return Err(SpawnError::ExecutorShutDown { component });
        }
        let handle = match spawn(
            thread::Builder::new().name(thread_name),
            Box::new(move || {
                CURRENT_TASK.with(|current| current.set(Some((executor_id, TaskId(task_id)))));
                let outcome = match start_rx.recv() {
                    Ok(work) => {
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(work.task));
                        let _ = completed.send(ReaperMsg::Completed(task_id, work.release));
                        Some(outcome)
                    }
                    Err(_) => {
                        let _ = completed.send(ReaperMsg::Completed(task_id, None));
                        None
                    }
                };
                CURRENT_TASK.with(|current| current.set(None));
                if let Some(Err(payload)) = outcome {
                    std::panic::resume_unwind(payload);
                }
            }),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                state.reservations.remove(&task_id);
                state.admitted -= 1;
                self.core.shared.changed.notify_all();
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
        Ok(StartedTask { start: start_tx })
    }

    fn release(&mut self) {
        // `take()` both reads the id (if still live) and disarms the
        // reservation in one step, so a second call — e.g. from `Drop` after
        // an explicit `release()` — observes `None` and is a no-op.
        let Some(reservation_id) = self.reservation_id.take() else {
            return;
        };
        let mut state = self
            .core
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.reservations.remove(&reservation_id) {
            state.admitted -= 1;
            // A waiter may already have observed this reservation in the
            // admitted count. SlotReleased wakes the reaper, not this
            // condition variable, so signal the lifecycle barrier directly.
            self.core.shared.changed.notify_all();
        }
        drop(state);
        let _ = self.core.reaper_tx.send(ReaperMsg::SlotReleased);
    }
}

pub struct StartedTask {
    start: mpsc::SyncSender<StartedWork>,
}

struct StartedWork {
    task: Box<dyn FnOnce() + Send + 'static>,
    release: Option<ReleaseEdge>,
}

impl StartedTask {
    pub fn run(self, task: impl FnOnce() + Send + 'static) {
        let _ = self.start.send(StartedWork {
            task: Box::new(task),
            release: None,
        });
    }

    /// Run `task`, then deliver its closed release id from the executor
    /// reaper only after the task is joined and its slot is released. Rich
    /// caller state must remain outside the executor, keyed by `release_id`.
    pub fn run_with_release_signal(
        self,
        task: impl FnOnce() + Send + 'static,
        release_sender: Sender<ReleaseId>,
        release_id: ReleaseId,
    ) {
        let _ = self.start.send(StartedWork {
            task: Box::new(task),
            release: Some(ReleaseEdge {
                sender: release_sender,
                id: release_id,
            }),
        });
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
            ReaperMsg::Completed(task_id, release) => {
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
                if let Some(release) = release {
                    release.signal();
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
    fn dropping_started_task_before_run_releases_admission_and_shutdown() {
        let executor = Executor::new(1).unwrap();
        let started = executor
            .reserve("drop-before-run")
            .unwrap()
            .start_with_cancel(|| {})
            .unwrap();
        drop(started);
        executor.wait_for_idle();
        assert_eq!(executor.census().admitted, 0);
        executor.shutdown();
    }

    #[test]
    fn dropped_release_receiver_cannot_kill_reaper() {
        let executor = Executor::new(1).unwrap();
        let (release_tx, release_rx) = mpsc::channel::<ReleaseId>();
        drop(release_rx);
        executor
            .reserve("panic-release")
            .unwrap()
            .start_with_cancel(|| {})
            .unwrap()
            .run_with_release_signal(|| {}, release_tx, ReleaseId::new(7));
        executor.wait_for_idle();
        executor
            .reserve("after-release-panic")
            .expect("capacity released after callback panic")
            .spawn_with_cancel(|| {}, || {})
            .expect("reaper remains alive after callback panic");
        executor.wait_for_idle();
        executor.shutdown();
        assert_eq!(executor.census().admitted, 0);
        assert_eq!(executor.census().running, 0);
    }

    #[test]
    fn closed_release_id_arrives_after_exact_slot_release() {
        let executor = Executor::new(1).unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        executor
            .reserve("typed-release")
            .unwrap()
            .start_with_cancel(|| {})
            .unwrap()
            .run_with_release_signal(|| {}, release_tx, ReleaseId::new(41));
        assert_eq!(release_rx.recv().unwrap(), ReleaseId::new(41));
        assert_eq!(executor.census().admitted, 0);
        executor.shutdown();
    }

    #[test]
    fn release_id_is_copy_and_cannot_own_a_destructor() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<ReleaseId>();
        assert!(!std::mem::needs_drop::<ReleaseId>());
        assert_eq!(std::mem::size_of::<ReleaseId>(), std::mem::size_of::<u64>());
    }

    #[test]
    fn default_fits_representative_transient_peak_plus_one_nip11_flight() {
        assert_eq!(DEFAULT_MAX_TASKS, 12);
        let executor = Executor::new(0).unwrap();
        for component in REPRESENTATIVE_COMPOSED_PEAK {
            hold_task(&executor, component);
        }
        assert_eq!(executor.census().admitted, 11);

        hold_task(&executor, "NIP-11 acquisition");
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
    fn dropping_a_reservation_wakes_an_already_waiting_idle_barrier() {
        let executor = Executor::new(1).unwrap();
        let reservation = executor.reserve("reserved").unwrap();
        let waiter = executor.clone();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let join = thread::spawn(move || {
            let mut waiting_tx = Some(waiting_tx);
            waiter.wait_for_idle_with(|| {
                if let Some(waiting_tx) = waiting_tx.take() {
                    waiting_tx.send(()).unwrap();
                }
            });
            done_tx.send(()).unwrap();
        });

        waiting_rx.recv().unwrap();
        drop(reservation);
        let woke_on_release = done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok();

        // Also makes the falsifier self-cleaning on the broken implementation:
        // shutdown's terminal notification wakes the waiter before we assert.
        executor.shutdown();
        join.join().unwrap();
        assert!(
            woke_on_release,
            "dropping the final reservation must wake an idle-barrier waiter"
        );
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
    fn current_task_identity_is_exact_and_scoped_to_its_executor() {
        let executor = Executor::new(1).unwrap();
        let other = Executor::new(1).unwrap();
        let reservation = executor.reserve("identity").unwrap();
        let expected = reservation.task_id();
        assert_eq!(executor.current_task_id(), None);
        assert_eq!(other.current_task_id(), None);

        let task_executor = executor.clone();
        let task_other = other.clone();
        let (observed_tx, observed_rx) = mpsc::channel();
        reservation
            .spawn_with_cancel(
                || {},
                move || {
                    observed_tx
                        .send((
                            task_executor.current_task_id(),
                            task_other.current_task_id(),
                        ))
                        .unwrap();
                },
            )
            .unwrap();

        assert_eq!(observed_rx.recv().unwrap(), (Some(expected), None));
        executor.wait_for_idle();
        executor.shutdown();
        other.shutdown();
        assert_eq!(executor.current_task_id(), None);
        assert_eq!(executor.census().admitted, 0);
        assert_eq!(executor.census().running, 0);
        assert_eq!(other.census().admitted, 0);
        assert_eq!(other.census().running, 0);
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
