//! Internal state of [`super::Pool`]: per-slot bookkeeping plus the
//! worker→pool translator thread that turns a [`super::worker::WorkerEvent`]
//! into a [`super::PoolEvent`], enforcing generation safety on the way.
//!
//! HARVEST source: the old repo's `crates/nmp-network/src/pool/inner.rs` —
//! the slot table (`Vec<Option<SlotState>>` + `url -> slot` index so a
//! closed slot's id is reusable), the single dedicated translator thread,
//! and "stale event -> silently drop" are all carried over. What's new here
//! (M3 plan §3.2 + tests 6/7): the generation check is a single `u64`
//! compare against [`super::worker::pack_generation`]'s packed
//! `(worker_id, attempt)` value rather than a plain incrementing counter —
//! see that module's doc comment for why. `Pool::close`/`Pool::shutdown`
//! also push their `Disconnected` event synchronously from the calling
//! thread (under this module's lock) instead of round-tripping through the
//! worker — the pool already knows the outcome the instant it decides to
//! tear a slot down, so there is nothing to learn from an async ack.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use nostr::secp256k1::schnorr::Signature;
#[cfg(test)]
use nostr::RelayUrl;
use nostr::{Event, EventId};

use crate::handle::RelayHandle;
use crate::health::{ConnState, RelayHealth};

use super::spawn::ThreadSpawner;
use super::verify::{self, VerificationOutcome, VerifierPool};
use super::worker::{
    pack_generation, worker_id_of, WorkerCommand, WorkerEvent, WorkerEventKind, WorkerHandle,
};
use super::{
    DisconnectReason, PoolBuildError, PoolConfig, PoolEvent, PoolEventSink, RelayOpenError,
    RelaySessionKey, ThreadRole, ThreadSpawnError,
};

struct RetireRequest {
    slot: u32,
    generation: u64,
    worker_id: u32,
    join: JoinHandle<()>,
}

pub(super) struct ShutdownHandles {
    reaper: Option<JoinHandle<()>>,
    translator: Option<JoinHandle<()>>,
    orphaned_workers: Vec<RetireRequest>,
    worker_event_tx: Option<SyncSender<WorkerEvent>>,
}

impl ShutdownHandles {
    pub(super) fn join(self) {
        if let Some(handle) = self.reaper {
            let _ = handle.join();
        }
        for request in self.orphaned_workers {
            let _ = request.join.join();
        }
        drop(self.worker_event_tx);
        if let Some(handle) = self.translator {
            let _ = handle.join();
        }
    }
}

struct SlotState {
    session: RelaySessionKey,
    /// `None` once explicitly closed (via `Pool::close`) or after
    /// `Pool::shutdown` — a slot in this state accepts no further worker
    /// events (see [`apply_worker_event`]) and is only revivable by a fresh
    /// `ensure_open` (reopen).
    worker: Option<WorkerHandle>,
    generation: u64,
    health: RelayHealth,
}

pub(super) struct PoolInner {
    /// Indexed by dense `RelayHandle.slot`. `worker: None` marks a closed
    /// slot; the entry itself stays so the slot id is only ever reused by a
    /// reopen of the SAME session (matching `session_to_slot`).
    slots: Vec<SlotState>,
    session_to_slot: HashMap<RelaySessionKey, u32>,
    /// Bumped on every fresh worker spawn (new session or reopen-after-close).
    /// Globally unique across the pool's whole lifetime — see
    /// `worker::pack_generation`.
    next_worker_id: u32,
    sink: Arc<dyn PoolEventSink>,
    /// `None` once [`Self::shutdown`] has run. The pool itself is the one
    /// long-lived owner of a `Sender<WorkerEvent>` clone beyond the worker
    /// threads (see [`Self::spawn_worker`]); if it were never dropped the
    /// mpsc channel could never disconnect even after every worker thread
    /// has exited, so the translator thread's blocking `recv()` in
    /// [`spawn_translator`] would never observe end-of-channel and
    /// `Pool::shutdown`'s `JoinHandle::join` would hang forever. Dropping
    /// this field in `shutdown()` is what lets the channel actually close
    /// once the last worker thread's own clone is also dropped.
    worker_event_tx: Option<SyncSender<WorkerEvent>>,
    retire_tx: Option<SyncSender<RetireRequest>>,
    reaper: Option<JoinHandle<()>>,
    retiring_worker_ids: HashSet<u32>,
    orphaned_workers: Vec<RetireRequest>,
    max_relay_threads: usize,
    spawner: Arc<dyn ThreadSpawner>,
    config: PoolConfig,
    translator: Option<JoinHandle<()>>,
    shutdown: bool,
    /// Count of [`Self::ensure_open`] calls refused because opening the relay
    /// would have taken the pool past `config.max_relays` LIVE workers (issue
    /// #121, the worker-exhaustion half). Monotonic; read (never reset) by
    /// [`super::Pool::admission_rejections`] so the engine can fold it into
    /// its diagnostics rejection counter. Zero is normalized to the finite
    /// default during construction.
    relays_rejected_over_cap: u64,
}

impl PoolInner {
    pub(super) fn try_new(
        config: PoolConfig,
        sink: Arc<dyn PoolEventSink>,
        spawner: Arc<dyn ThreadSpawner>,
    ) -> Result<Arc<Mutex<Self>>, PoolBuildError> {
        let mut config = config;
        if config.max_relays == 0 {
            config.max_relays = super::DEFAULT_MAX_RELAYS;
        }
        let max_relay_threads =
            config
                .max_relays
                .checked_mul(2)
                .ok_or(PoolBuildError::RelayBudgetOverflow {
                    max_relays: config.max_relays,
                })?;
        let (worker_event_tx, worker_event_rx) =
            mpsc::sync_channel::<WorkerEvent>(config.ingest_queue_capacity.max(1));
        let (retire_tx, retire_rx) = mpsc::sync_channel::<RetireRequest>(config.max_relays.max(1));
        let reaper = spawn_reaper(retire_rx, worker_event_tx.clone(), spawner.as_ref())
            .map_err(PoolBuildError::ThreadUnavailable)?;
        let verifier = match VerifierPool::new(
            configured_verifier_workers(config.verifier_workers),
            config.verifier_queue_capacity,
            Arc::clone(&spawner),
        ) {
            Ok(verifier) => verifier,
            Err(error) => {
                drop(retire_tx);
                let _ = reaper.join();
                return Err(PoolBuildError::ThreadUnavailable(error));
            }
        };
        let translator_config = config.clone();
        let inner = Arc::new(Mutex::new(Self {
            slots: Vec::new(),
            session_to_slot: HashMap::new(),
            next_worker_id: 0,
            sink,
            worker_event_tx: Some(worker_event_tx),
            retire_tx: Some(retire_tx),
            reaper: Some(reaper),
            retiring_worker_ids: HashSet::new(),
            orphaned_workers: Vec::new(),
            max_relay_threads,
            spawner: Arc::clone(&spawner),
            config,
            translator: None,
            shutdown: false,
            relays_rejected_over_cap: 0,
        }));
        let translator = match spawn_translator(
            Arc::clone(&inner),
            worker_event_rx,
            translator_config,
            verifier,
            spawner.as_ref(),
        ) {
            Ok(translator) => translator,
            Err(error) => {
                let reaper = inner.lock().ok().and_then(|mut guard| {
                    guard.worker_event_tx = None;
                    guard.retire_tx = None;
                    guard.reaper.take()
                });
                if let Some(reaper) = reaper {
                    let _ = reaper.join();
                }
                return Err(PoolBuildError::ThreadUnavailable(error));
            }
        };
        if let Ok(mut guard) = inner.lock() {
            guard.translator = Some(translator);
        }
        Ok(inner)
    }

    #[cfg(test)]
    pub(super) fn new(config: PoolConfig, sink: Arc<dyn PoolEventSink>) -> Arc<Mutex<Self>> {
        Self::try_new(config, sink, super::spawn::system_spawner())
            .expect("test pool construction must succeed")
    }

    #[cfg(test)]
    pub(super) fn ensure_open(&mut self, url: &RelayUrl) -> RelayHandle {
        self.try_ensure_session(&RelaySessionKey::public(url.clone()))
            .expect("test relay worker spawn/admission must succeed")
    }

    #[cfg(test)]
    pub(super) fn try_ensure_open(
        &mut self,
        url: &RelayUrl,
    ) -> Result<RelayHandle, RelayOpenError> {
        self.try_ensure_session(&RelaySessionKey::public(url.clone()))
    }

    pub(super) fn try_ensure_session(
        &mut self,
        session: &RelaySessionKey,
    ) -> Result<RelayHandle, RelayOpenError> {
        self.reap_orphaned_workers();
        if self.shutdown {
            return Err(RelayOpenError::ShuttingDown);
        }
        if let Some(&slot_id) = self.session_to_slot.get(session) {
            let state = &self.slots[slot_id as usize];
            if state.worker.is_some() {
                // Idempotent: a live slot for this session already exists — never
                // counted against the cap (it is already one of the live
                // relays the cap bounds).
                return Ok(RelayHandle {
                    slot: slot_id,
                    generation: state.generation,
                });
            }
            // Reopening a previously-closed slot makes a worker LIVE again,
            // so it is subject to the same live-relay ceiling as a brand-new
            // relay.
            if self.live_worker_count() >= self.config.max_relays
                || self.total_relay_thread_count() >= self.max_relay_threads
            {
                self.relays_rejected_over_cap += 1;
                return Err(RelayOpenError::AtCapacity {
                    max_relays: self.config.max_relays,
                });
            }
            return self.reopen(slot_id, session.clone());
        }
        if self.live_worker_count() >= self.config.max_relays
            || self.total_relay_thread_count() >= self.max_relay_threads
        {
            self.relays_rejected_over_cap += 1;
            return Err(RelayOpenError::AtCapacity {
                max_relays: self.config.max_relays,
            });
        }
        self.open_new(session.clone())
    }

    pub(super) fn live_session_handle(&self, session: &RelaySessionKey) -> Option<RelayHandle> {
        let slot = *self.session_to_slot.get(session)?;
        let state = self.slots.get(slot as usize)?;
        state.worker.as_ref()?;
        Some(RelayHandle {
            slot,
            generation: state.generation,
        })
    }

    /// Distinct relays currently backed by a live worker (a slot whose
    /// `worker` has not been taken by `close`/`shutdown`).
    fn live_worker_count(&self) -> usize {
        self.slots.iter().filter(|s| s.worker.is_some()).count()
    }

    fn total_relay_thread_count(&self) -> usize {
        self.live_worker_count()
            .checked_add(self.retiring_worker_ids.len())
            .expect("active + retiring cannot exceed checked construction envelope")
    }

    fn reap_orphaned_workers(&mut self) {
        let mut pending = Vec::new();
        for request in self.orphaned_workers.drain(..) {
            if request.join.is_finished() {
                let _ = request.join.join();
                self.retiring_worker_ids.remove(&request.worker_id);
            } else {
                pending.push(request);
            }
        }
        self.orphaned_workers = pending;
    }

    fn retire_worker(&mut self, slot: u32, generation: u64, worker: WorkerHandle) {
        let worker_id = worker_id_of(generation);
        let request = RetireRequest {
            slot,
            generation,
            worker_id,
            join: worker.retire(),
        };
        self.retiring_worker_ids.insert(worker_id);
        let Some(retire_tx) = self.retire_tx.as_ref() else {
            self.orphaned_workers.push(request);
            return;
        };
        if let Err(error) = retire_tx.try_send(request) {
            let request = match error {
                mpsc::TrySendError::Full(request) | mpsc::TrySendError::Disconnected(request) => {
                    request
                }
            };
            self.orphaned_workers.push(request);
        }
    }

    /// Read the monotonic count of relay-cap rejections (issue #121). See
    /// [`Self::relays_rejected_over_cap`].
    pub(super) fn relays_rejected_over_cap(&self) -> u64 {
        self.relays_rejected_over_cap
    }

    fn open_new(&mut self, session: RelaySessionKey) -> Result<RelayHandle, RelayOpenError> {
        let slot_id = u32::try_from(self.slots.len()).map_err(|_| RelayOpenError::Unavailable)?;
        let worker_id = self.next_worker_id;
        self.next_worker_id = self
            .next_worker_id
            .checked_add(1)
            .ok_or(RelayOpenError::Unavailable)?;
        let generation = pack_generation(worker_id, 0);
        let worker = self.spawn_worker(slot_id, worker_id, &session)?;
        self.slots.push(SlotState {
            session: session.clone(),
            worker: Some(worker),
            generation,
            health: RelayHealth {
                state: ConnState::Connecting,
                ..RelayHealth::default()
            },
        });
        self.session_to_slot.insert(session, slot_id);
        Ok(RelayHandle {
            slot: slot_id,
            generation,
        })
    }

    fn reopen(
        &mut self,
        slot_id: u32,
        session: RelaySessionKey,
    ) -> Result<RelayHandle, RelayOpenError> {
        let worker_id = self.next_worker_id;
        self.next_worker_id = self
            .next_worker_id
            .checked_add(1)
            .ok_or(RelayOpenError::Unavailable)?;
        let generation = pack_generation(worker_id, 0);
        let worker = self.spawn_worker(slot_id, worker_id, &session)?;
        self.slots[slot_id as usize] = SlotState {
            session,
            worker: Some(worker),
            generation,
            health: RelayHealth {
                state: ConnState::Connecting,
                ..RelayHealth::default()
            },
        };
        Ok(RelayHandle {
            slot: slot_id,
            generation,
        })
    }

    fn spawn_worker(
        &self,
        slot_id: u32,
        worker_id: u32,
        session: &RelaySessionKey,
    ) -> Result<WorkerHandle, RelayOpenError> {
        let idle = self
            .config
            .keepalive_idle
            .unwrap_or(crate::keepalive::KEEPALIVE_IDLE_THRESHOLD);
        let pong_timeout = self
            .config
            .keepalive_pong_timeout
            .unwrap_or(crate::keepalive::KEEPALIVE_PONG_TIMEOUT);
        let reconnect_delay_initial = self
            .config
            .reconnect_delay_initial
            .unwrap_or(crate::backoff::RECONNECT_DELAY_INITIAL);
        let reconnect_jitter_max = self
            .config
            .reconnect_jitter_max
            .unwrap_or(crate::backoff::RECONNECT_JITTER_MAX);
        let command_queue_capacity = self.config.command_queue_capacity.max(1);
        super::worker::spawn(
            slot_id,
            worker_id,
            session.relay.as_str().to_string(),
            session.access != nmp_grammar::AccessContext::Public,
            self.worker_event_tx
                .as_ref()
                .expect("spawn_worker never called after shutdown (ensure_open guards it)")
                .clone(),
            idle,
            pong_timeout,
            reconnect_delay_initial,
            reconnect_jitter_max,
            command_queue_capacity,
            Arc::clone(&self.config.allowed_local_hosts),
            self.spawner.as_ref(),
        )
        .map_err(RelayOpenError::ThreadUnavailable)
    }

    pub(super) fn command_tx_for(&self, h: RelayHandle) -> Option<&WorkerHandle> {
        let state = self.slots.get(h.slot as usize)?;
        if state.generation != h.generation || state.health.state == ConnState::Disconnected {
            return None;
        }
        state.worker.as_ref()
    }

    /// Exact connected-session command door for nonpersistent protocol
    /// handoffs. Validation and command enqueue both happen while the one
    /// `PoolInner` lock is held, so the translator cannot publish a newer
    /// slot generation between them. The worker repeats the generation check
    /// when draining to close the remaining worker-side reconnect race.
    pub(super) fn connected_command_tx_for(
        &self,
        session: &RelaySessionKey,
        h: RelayHandle,
    ) -> Option<&WorkerHandle> {
        let state = self.slots.get(h.slot as usize)?;
        if state.session != *session
            || state.generation != h.generation
            || state.health.state != ConnState::Connected
        {
            return None;
        }
        state.worker.as_ref()
    }

    pub(super) fn set_reconnect_preamble_for(&self, h: RelayHandle, frames: Vec<String>) -> bool {
        match self.command_tx_for(h) {
            Some(worker) => worker.push(WorkerCommand::SetReconnectPreamble(frames)),
            None => false,
        }
    }

    pub(super) fn release_initial_read_for(&self, h: RelayHandle) -> bool {
        match self.command_tx_for(h) {
            Some(worker) => worker.push(WorkerCommand::ReleaseInitialRead {
                generation: h.generation,
            }),
            None => false,
        }
    }

    pub(super) fn health_for(&self, h: RelayHandle) -> Option<RelayHealth> {
        let state = self.slots.get(h.slot as usize)?;
        if state.generation != h.generation {
            return None;
        }
        Some(state.health.clone())
    }

    /// Close the slot for `h` and return its synchronous disconnect fact.
    /// Sink delivery is intentionally the caller's responsibility so no
    /// blocking bounded send can occur while `PoolInner` is locked.
    pub(super) fn close(&mut self, h: RelayHandle) -> Option<PoolEvent> {
        let state = self.slots.get_mut(h.slot as usize)?;
        if state.generation != h.generation {
            return None;
        }
        let worker = state.worker.take()?;
        let generation = state.generation;
        let session = state.session.clone();
        state.health.state = ConnState::Disconnected;
        self.retire_worker(h.slot, generation, worker);
        Some(PoolEvent::Disconnected {
            handle: h,
            session,
            reason: DisconnectReason::Closed,
        })
    }

    /// Release every live slot not present in the caller-owned exact demand
    /// set. Handles are snapshotted first so [`Self::close`] remains the one
    /// generation-safe mutation door and produces the ordinary synchronous
    /// disconnect fact for every released worker.
    pub(super) fn close_unrequired_sessions(
        &mut self,
        required: &BTreeSet<RelaySessionKey>,
    ) -> Vec<PoolEvent> {
        let obsolete: Vec<RelayHandle> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, state)| state.worker.is_some() && !required.contains(&state.session))
            .map(|(slot, state)| RelayHandle {
                slot: u32::try_from(slot).expect("pool slot id already fit u32 at allocation"),
                generation: state.generation,
            })
            .collect();
        obsolete
            .into_iter()
            .filter_map(|handle| self.close(handle))
            .collect()
    }

    /// Tear down every open worker, hand back the translator's `JoinHandle`
    /// so the caller can join it *outside* this lock (the translator locks
    /// `PoolInner` per event; joining while holding the lock deadlocks).
    pub(super) fn shutdown(&mut self) -> ShutdownHandles {
        self.shutdown = true;
        let active: Vec<_> = self
            .slots
            .iter_mut()
            .enumerate()
            .filter_map(|(slot, state)| {
                let worker = state.worker.take()?;
                state.health.state = ConnState::Disconnected;
                Some((slot as u32, state.generation, worker))
            })
            .collect();
        for (slot, generation, worker) in active {
            self.retire_worker(slot, generation, worker);
        }
        // Drop the pool's own long-lived `Sender<WorkerEvent>` clone. Every
        // worker thread also holds a clone but each exits promptly after
        // processing the `Shutdown` command pushed above, dropping its own
        // clone in turn; once every clone (this one plus every worker's) is
        // gone the channel disconnects and the translator's blocking `recv()`
        // below finally returns `Err`, letting `translator_loop` exit instead
        // of blocking forever. Without this drop the channel could never
        // disconnect even after all worker threads exit, and `Pool::shutdown`
        // joining the translator handle would hang indefinitely.
        self.retire_tx = None;
        ShutdownHandles {
            reaper: self.reaper.take(),
            translator: self.translator.take(),
            orphaned_workers: std::mem::take(&mut self.orphaned_workers),
            worker_event_tx: self.worker_event_tx.take(),
        }
    }
}

fn spawn_translator(
    inner: Arc<Mutex<PoolInner>>,
    worker_event_rx: std::sync::mpsc::Receiver<WorkerEvent>,
    config: PoolConfig,
    verifier: VerifierPool,
    spawner: &dyn ThreadSpawner,
) -> Result<JoinHandle<()>, ThreadSpawnError> {
    spawner
        .spawn(
            thread::Builder::new().name("nmp-transport-pool-translator".to_string()),
            Box::new(move || translator_loop(&inner, &worker_event_rx, &config, verifier)),
        )
        .map_err(|error| ThreadSpawnError {
            role: ThreadRole::PoolTranslator,
            reason: error.to_string(),
        })
}

fn spawn_reaper(
    retire_rx: std::sync::mpsc::Receiver<RetireRequest>,
    worker_event_tx: SyncSender<WorkerEvent>,
    spawner: &dyn ThreadSpawner,
) -> Result<JoinHandle<()>, ThreadSpawnError> {
    spawner
        .spawn(
            thread::Builder::new().name("nmp-transport-relay-reaper".to_string()),
            Box::new(move || {
                while let Ok(request) = retire_rx.recv() {
                    let _ = request.join.join();
                    if worker_event_tx
                        .send(WorkerEvent {
                            slot: request.slot,
                            generation: request.generation,
                            kind: WorkerEventKind::Retired {
                                worker_id: request.worker_id,
                            },
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }),
        )
        .map_err(|error| ThreadSpawnError {
            role: ThreadRole::RetirementReaper,
            reason: error.to_string(),
        })
}

fn translator_loop(
    inner: &Arc<Mutex<PoolInner>>,
    worker_event_rx: &std::sync::mpsc::Receiver<WorkerEvent>,
    config: &PoolConfig,
    mut verifier: VerifierPool,
) {
    let mut verified = VerifiedEventCache::new(config.verified_cache_capacity);
    let max_batch = config.max_verify_batch.max(1);
    while let Ok(event) = worker_event_rx.recv() {
        let mut events = vec![event];
        events.extend(worker_event_rx.try_iter().take(max_batch - 1));
        let Ok(guard) = inner.lock() else { break };
        // Project generation changes in per-worker FIFO order without
        // mutating the real slots (the retirement reaper is a separate
        // producer). A reconnect worker emits Connected before its first
        // Frame and InitialReadCompleted; planning all three in one batch
        // must therefore see the latter two as current.
        let current = planned_currentness(&guard, &events);
        drop(guard);

        // Build the finite crypto plan without holding PoolInner. Known ids
        // are signature comparisons. Unknown identical (id, signature) pairs
        // share one crypto check within the burst, but every accepted frame is
        // still forwarded so provenance is never deduplicated away.
        let mut candidates: Vec<Arc<Event>> = Vec::new();
        let mut candidate_by_pair = HashMap::new();
        let plans: Vec<_> = events
            .iter()
            .zip(current)
            .map(|(event, current)| {
                if !current {
                    return VerificationPlan::Stale;
                }
                let WorkerEventKind::Frame(frame) = &event.kind else {
                    return VerificationPlan::Pass;
                };
                let Some(event) = frame.event() else {
                    return VerificationPlan::Pass;
                };
                if let Some(plan) = cached_frame_plan(&verified, frame) {
                    return plan;
                }
                let pair = (event.id, event.sig);
                let candidate = *candidate_by_pair.entry(pair).or_insert_with(|| {
                    let index = candidates.len();
                    candidates.push(Arc::clone(event));
                    index
                });
                VerificationPlan::Candidate(candidate)
            })
            .collect();
        let candidate_results = verifier.verify_batch(&candidates);

        let Ok(mut guard) = inner.lock() else { break };
        let mut pool_events = Vec::with_capacity(events.len());
        for (event, plan) in events.into_iter().zip(plans) {
            // A slot can close/reopen while crypto is running. Recheck now;
            // stale work is neither cached nor treated as relay misbehavior.
            let verdict = if frame_is_current(&guard, &event) {
                match (&event.kind, plan) {
                    (WorkerEventKind::Frame(frame), VerificationPlan::Known(known)) => {
                        Some(if event_signature(frame) == Some(known) {
                            FrameVerdict::Accept
                        } else {
                            FrameVerdict::RejectMisbehavior
                        })
                    }
                    (WorkerEventKind::Frame(frame), VerificationPlan::Candidate(candidate)) => {
                        Some(resolve_candidate_verdict(
                            &mut verified,
                            frame,
                            candidate_results[candidate],
                        ))
                    }
                    (WorkerEventKind::Frame(_), VerificationPlan::InvalidId) => {
                        Some(FrameVerdict::RejectMisbehavior)
                    }
                    (WorkerEventKind::Frame(_), VerificationPlan::Pass) => {
                        Some(FrameVerdict::Accept)
                    }
                    (_, VerificationPlan::Pass) => None,
                    (_, VerificationPlan::Stale) => None,
                    _ => unreachable!("verification plan must match its worker event"),
                }
            } else {
                None
            };
            if let Some(pool_event) = apply_worker_event_with_verdict(&mut guard, event, verdict) {
                pool_events.push(pool_event);
            }
        }
        // Clone the sink handle (Arc bump) and drop the lock before
        // delivering, so a slow/blocking sink can never stall a concurrent
        // `Pool::send`/`ensure_open` (mirrors the harvested source's
        // off-lock delivery discipline).
        let sink = Arc::clone(&guard.sink);
        drop(guard);
        // Release verifier references before sink delivery so the engine can
        // unwrap each frame's Arc<Event> without cloning content or tags.
        drop(candidates);
        for pool_event in pool_events {
            sink.on_event(pool_event);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn configured_verifier_workers(configured: usize) -> usize {
    if configured == 0 {
        super::DEFAULT_VERIFIER_WORKERS
    } else {
        configured.min(super::DEFAULT_VERIFIER_WORKERS)
    }
}

#[cfg(target_arch = "wasm32")]
pub(super) fn configured_verifier_workers(_configured: usize) -> usize {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationPlan {
    Stale,
    Pass,
    InvalidId,
    Known(Signature),
    Candidate(usize),
}

fn cached_frame_plan(
    verified: &VerifiedEventCache,
    frame: &super::RelayFrame,
) -> Option<VerificationPlan> {
    let event = frame.event()?;
    if !event.verify_id() {
        return Some(VerificationPlan::InvalidId);
    }
    verified.get(&event.id).map(VerificationPlan::Known)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameVerdict {
    Accept,
    RejectMisbehavior,
    RejectUnavailable,
}

struct VerifiedEventCache {
    capacity: usize,
    signatures: HashMap<EventId, Signature>,
    insertion_order: VecDeque<EventId>,
}

impl VerifiedEventCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            signatures: HashMap::with_capacity(capacity),
            insertion_order: VecDeque::with_capacity(capacity),
        }
    }

    fn get(&self, id: &EventId) -> Option<Signature> {
        self.signatures.get(id).copied()
    }

    fn insert(&mut self, id: EventId, signature: Signature) {
        if self.capacity == 0 || self.signatures.contains_key(&id) {
            return;
        }
        if self.signatures.len() == self.capacity {
            let evicted = self
                .insertion_order
                .pop_front()
                .expect("full verification cache has an eviction candidate");
            self.signatures.remove(&evicted);
        }
        self.signatures.insert(id, signature);
        self.insertion_order.push_back(id);
    }
}

fn event_signature(frame: &super::RelayFrame) -> Option<Signature> {
    frame.event().map(|event| event.sig)
}

fn resolve_candidate_verdict(
    verified: &mut VerifiedEventCache,
    frame: &super::RelayFrame,
    cryptographically_valid: VerificationOutcome,
) -> FrameVerdict {
    let Some(event) = frame.event() else {
        unreachable!("only EVENT frames receive candidate plans")
    };
    match (verified.get(&event.id), cryptographically_valid) {
        (Some(known), VerificationOutcome::Valid) if known == event.sig => FrameVerdict::Accept,
        (Some(_), VerificationOutcome::Valid | VerificationOutcome::Invalid) => {
            FrameVerdict::RejectMisbehavior
        }
        (Some(_), VerificationOutcome::Unavailable) => FrameVerdict::RejectUnavailable,
        (None, VerificationOutcome::Valid) => {
            verified.insert(event.id, event.sig);
            FrameVerdict::Accept
        }
        (None, VerificationOutcome::Invalid) => FrameVerdict::RejectMisbehavior,
        (None, VerificationOutcome::Unavailable) => FrameVerdict::RejectUnavailable,
    }
}

fn frame_is_current(inner: &PoolInner, event: &WorkerEvent) -> bool {
    if !matches!(event.kind, WorkerEventKind::Frame(_)) {
        return true;
    }
    let Some(state) = inner.slots.get(event.slot as usize) else {
        return false;
    };
    state.worker.is_some()
        && worker_id_of(event.generation) == worker_id_of(state.generation)
        && event.generation == state.generation
}

fn planned_currentness(inner: &PoolInner, events: &[WorkerEvent]) -> Vec<bool> {
    let mut planned_generations: HashMap<u32, u64> = inner
        .slots
        .iter()
        .enumerate()
        .filter_map(|(slot, state)| {
            state
                .worker
                .as_ref()
                .map(|_| (slot as u32, state.generation))
        })
        .collect();
    events
        .iter()
        .map(|event| {
            let baseline = planned_generations.get(&event.slot).copied();
            match &event.kind {
                WorkerEventKind::Connected
                    if baseline.is_some_and(|generation| {
                        worker_id_of(generation) == worker_id_of(event.generation)
                            && event.generation >= generation
                    }) =>
                {
                    planned_generations.insert(event.slot, event.generation);
                    true
                }
                WorkerEventKind::Frame(_) | WorkerEventKind::InitialReadCompleted => {
                    baseline == Some(event.generation)
                }
                _ => true,
            }
        })
        .collect()
}

/// Apply one [`WorkerEvent`] to its slot and build the outbound
/// [`PoolEvent`], or `None` if the event is stale / the slot is closed.
///
/// The generation-safety rule (plan §3.2, tests 6/7):
/// - A slot with `worker: None` (explicitly closed, not yet reopened)
///   accepts nothing — a closed slot's leftover in-flight events are inert.
/// - `Connected` is accepted iff it comes from the CURRENTLY active worker
///   instance (`worker_id_of(event.generation) == worker_id_of(state.generation)`)
///   and is not older than what's already recorded (`>=`) — the `>=` (not
///   `>`) matters for the very first `Connected` of a freshly spawned
///   worker, whose `attempt == 0` equals the baseline generation the pool
///   already assigned synchronously in `ensure_open`.
/// - `Failed`/`Frame` are accepted iff they come from the currently active
///   worker instance. Because one worker thread emits its own events in
///   strict program order onto a single channel, an event can never be
///   "for an older session than the last `Connected` this worker itself
///   already reported" — the only real staleness is cross-instance (a
///   worker from before an explicit close+reopen), which the worker-id
///   check alone fully covers.
#[cfg(test)]
fn apply_worker_event(inner: &mut PoolInner, event: WorkerEvent) -> Option<PoolEvent> {
    let verdict = match &event.kind {
        WorkerEventKind::Frame(frame) => Some(match frame.event() {
            Some(event) if event.verify().is_err() => FrameVerdict::RejectMisbehavior,
            _ => FrameVerdict::Accept,
        }),
        _ => None,
    };
    apply_worker_event_with_verdict(inner, event, verdict)
}

fn apply_worker_event_with_verdict(
    inner: &mut PoolInner,
    event: WorkerEvent,
    preverified: Option<FrameVerdict>,
) -> Option<PoolEvent> {
    if let WorkerEventKind::Retired { worker_id } = event.kind {
        return inner
            .retiring_worker_ids
            .remove(&worker_id)
            .then_some(PoolEvent::WorkerRetired);
    }

    // `EventHandoff` (issue #93) is the one exception to every generation/
    // slot-state gate below: it is the sole, ever, resolution of a durable
    // EVENT's `AttemptCorrelation`, decided once by the worker itself. It
    // must reach the sink regardless of whether the pool has since closed
    // this slot, reopened it, or moved on to a newer generation — gating it
    // like `Frame`/`Connected` would risk silently stranding a correlation
    // with no answer at all, which is precisely the hidden-queue failure
    // mode this seam exists to remove.
    if let WorkerEventKind::EventHandoff {
        correlation,
        result,
    } = event.kind
    {
        return Some(PoolEvent::EventHandoff {
            correlation,
            result,
        });
    }

    let state = inner.slots.get_mut(event.slot as usize)?;
    state.worker.as_ref()?;
    let same_worker = worker_id_of(event.generation) == worker_id_of(state.generation);

    match event.kind {
        WorkerEventKind::Connected => {
            // A different worker instance is always stale (a since-closed
            // slot's leftover worker) — the pool set `state.generation` to
            // the new worker's baseline synchronously at spawn time, before
            // this event could possibly arrive. The `>=` guard against the
            // SAME worker is defense-in-depth against out-of-order delivery;
            // FIFO per-sender ordering already makes it unreachable.
            if !same_worker || event.generation < state.generation {
                return None;
            }
            state.generation = event.generation;
            state.health.state = ConnState::Connected;
            state.health.last_error = None;
            state.health.backoff = std::time::Duration::ZERO;
            Some(PoolEvent::Connected {
                handle: RelayHandle {
                    slot: event.slot,
                    generation: event.generation,
                },
                session: state.session.clone(),
            })
        }
        WorkerEventKind::Failed {
            message,
            permanent,
            retry_in,
        } => {
            if !same_worker {
                return None;
            }
            let was_connected = state.health.state == ConnState::Connected;
            state.health.last_error = Some(message);
            state.health.backoff = retry_in.unwrap_or_default();
            state.health.state = if permanent {
                ConnState::Disconnected
            } else {
                ConnState::Connecting
            };
            let handle = RelayHandle {
                slot: event.slot,
                generation: event.generation,
            };
            if permanent {
                // The load-bearing fix (issue #506's CRITICAL finding): a
                // permanent failure (401/403 -- `backoff::is_permanent_error`)
                // means the WORKER ITSELF has already given up for good (see
                // `worker::drain_permanently_disconnected`) -- it will never
                // redial on its own. Leaving `state.worker` populated here
                // would wedge this slot forever: `try_ensure_open`/
                // `live_handle` judge liveness by `worker.is_some()`, so they
                // would keep idempotently handing back this dead handle, and
                // the parked worker thread plus its `max_relays` cap slot
                // would never be reclaimed. Taking the worker and retiring it
                // -- exactly the same door `close`/`shutdown` use -- frees
                // both the OS thread and the cap slot immediately, and
                // leaves `state.worker == None` so a subsequent
                // `ensure_open` reopens a FRESH generation instead of
                // handing back a stale one. This is reported on BOTH
                // branches below (was-connected and never-connected) --
                // unlike an ordinary transient failure, a permanent one is
                // never merely a `Health` update, because there is no
                // worker left behind for the caller to keep observing.
                let taken = state.worker.take();
                let generation = state.generation;
                let session = state.session.clone();
                // `state`'s mutable borrow of `inner.slots` ends here (its
                // last use); `retire_worker` below takes `&mut inner` for
                // the whole `PoolInner`, which NLL only allows once `state`
                // is no longer live.
                if let Some(worker) = taken {
                    inner.retire_worker(event.slot, generation, worker);
                }
                return Some(PoolEvent::Disconnected {
                    handle,
                    session,
                    reason: DisconnectReason::PermanentlyFailed,
                });
            }
            if was_connected {
                Some(PoolEvent::Disconnected {
                    handle,
                    session: state.session.clone(),
                    reason: DisconnectReason::Error,
                })
            } else {
                Some(PoolEvent::Health {
                    handle,
                    session: state.session.clone(),
                    health: state.health.clone(),
                })
            }
        }
        WorkerEventKind::Frame(frame) => {
            if !same_worker || event.generation != state.generation {
                return None;
            }
            // Ingest verification gate (network-boundary, kind-blind --
            // see `pool::verify`'s module doc): a frame that fails here is
            // dropped BEFORE it ever becomes a `PoolEvent::Frame` -- never
            // forwarded to the engine/store/routing. `verified_events` is
            // pool-global (not per-slot) so a redelivery of the same event
            // id by a DIFFERENT relay still hits the cache-compare fast
            // path instead of re-running schnorr.
            match preverified.expect("translator must classify every current frame") {
                FrameVerdict::Accept => Some(PoolEvent::Frame {
                    handle: RelayHandle {
                        slot: event.slot,
                        generation: event.generation,
                    },
                    session: state.session.clone(),
                    frame,
                }),
                FrameVerdict::RejectMisbehavior => {
                    verify::record_misbehavior(&mut state.health);
                    Some(PoolEvent::Health {
                        handle: RelayHandle {
                            slot: event.slot,
                            generation: event.generation,
                        },
                        session: state.session.clone(),
                        health: state.health.clone(),
                    })
                }
                FrameVerdict::RejectUnavailable => {
                    verify::record_unavailable(&mut state.health);
                    Some(PoolEvent::Health {
                        handle: RelayHandle {
                            slot: event.slot,
                            generation: event.generation,
                        },
                        session: state.session.clone(),
                        health: state.health.clone(),
                    })
                }
            }
        }
        WorkerEventKind::InitialReadCompleted => {
            if !same_worker || event.generation != state.generation {
                return None;
            }
            Some(PoolEvent::InitialReadCompleted {
                handle: RelayHandle {
                    slot: event.slot,
                    generation: event.generation,
                },
                session: state.session.clone(),
            })
        }
        WorkerEventKind::EventHandoff { .. } => {
            unreachable!("EventHandoff already returned above, before any slot lookup")
        }
        WorkerEventKind::Retired { .. } => {
            unreachable!("Retired already returned above, before any slot lookup")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{Pool, PoolConfig, PoolEvent, WireFrame};
    use std::sync::mpsc::Sender as StdSender;

    struct Collector(StdSender<PoolEvent>);
    impl PoolEventSink for Collector {
        fn on_event(&self, event: PoolEvent) {
            let _ = self.0.send(event);
        }
    }

    fn test_pool() -> (Arc<Mutex<PoolInner>>, std::sync::mpsc::Receiver<PoolEvent>) {
        let (tx, rx) = mpsc::channel();
        let inner = PoolInner::new(PoolConfig::default(), Arc::new(Collector(tx)));
        (inner, rx)
    }

    /// The core generation-safety falsifier, exercised with NO network at
    /// all: directly drive `apply_worker_event` as if a stale worker
    /// (superseded by an explicit close+reopen) delivered a late frame.
    #[test]
    fn frame_from_a_closed_and_reopened_slot_is_dropped() {
        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        let url = RelayUrl::parse("wss://relay.example").unwrap();

        let h1 = guard.ensure_open(&url);
        // Simulate the first worker's own Connected event.
        let connected = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h1.slot,
                generation: h1.generation,
                kind: WorkerEventKind::Connected,
            },
        );
        assert!(matches!(connected, Some(PoolEvent::Connected { .. })));

        // Close, then reopen — a new worker instance, new generation.
        assert!(guard.close(h1).is_some());
        let h2 = guard.ensure_open(&url);
        assert_ne!(
            h1.generation, h2.generation,
            "reopen must mint a fresh generation"
        );

        // A straggler Frame from the OLD (h1) worker must be dropped.
        let stale = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h1.slot,
                generation: h1.generation,
                kind: WorkerEventKind::Frame(crate::pool::RelayFrame::from_message(
                    nostr::RelayMessage::notice("late"),
                )),
            },
        );
        assert!(stale.is_none(), "stale-worker frame must be dropped");

        let stale_completion = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h1.slot,
                generation: h1.generation,
                kind: WorkerEventKind::InitialReadCompleted,
            },
        );
        assert!(
            stale_completion.is_none(),
            "stale-worker initial-read completion must be dropped"
        );

        // A frame from the NEW (h2) worker is accepted.
        let fresh = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h2.slot,
                generation: h2.generation,
                kind: WorkerEventKind::Frame(crate::pool::RelayFrame::from_message(
                    nostr::RelayMessage::notice("fresh"),
                )),
            },
        );
        assert!(matches!(fresh, Some(PoolEvent::Frame { .. })));
    }

    #[test]
    fn ensure_open_is_idempotent_for_a_live_slot() {
        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        let url = RelayUrl::parse("wss://relay.example").unwrap();
        let h1 = guard.ensure_open(&url);
        let h2 = guard.ensure_open(&url);
        assert_eq!(h1, h2, "a live slot's ensure_open must be idempotent");
    }

    /// The relay-count admission cap (issue #121, worker-exhaustion half):
    /// with `max_relays: 2`, the pool opens two distinct relays but REFUSES
    /// the third with a typed capacity error and bumps the observable rejection
    /// counter, while an already-open relay
    /// stays idempotently openable. A hostile (validly-signed) kind:10002
    /// listing thousands of relays can never spawn thousands of workers.
    #[test]
    fn relay_count_cap_refuses_relays_beyond_max_and_counts_the_rejection() {
        let (tx, _rx) = mpsc::channel();
        let inner = PoolInner::new(
            PoolConfig {
                max_relays: 2,
                ..PoolConfig::default()
            },
            Arc::new(Collector(tx)),
        );
        let mut guard = inner.lock().unwrap();

        let a = RelayUrl::parse("wss://relay-a.example").unwrap();
        let b = RelayUrl::parse("wss://relay-b.example").unwrap();
        let c = RelayUrl::parse("wss://relay-c.example").unwrap();

        let ha = guard.try_ensure_open(&a).expect("first relay must open");
        let hb = guard.try_ensure_open(&b).expect("second relay must open");
        assert_eq!(guard.relays_rejected_over_cap(), 0, "no rejection yet");

        // The third DISTINCT relay is over the cap: refused explicitly, no
        // slot created, counter bumped.
        assert_eq!(
            guard.try_ensure_open(&c),
            Err(RelayOpenError::AtCapacity { max_relays: 2 })
        );
        assert_eq!(guard.relays_rejected_over_cap(), 1);

        // Re-opening an ALREADY-live relay is idempotent, never a rejection.
        let ha_again = guard.ensure_open(&a);
        assert_eq!(
            ha, ha_again,
            "an already-open relay stays idempotently open"
        );
        assert_eq!(
            guard.relays_rejected_over_cap(),
            1,
            "idempotent reopen is not a rejection"
        );

        // Closing one relay removes it from the active set, so one replacement
        // may start. The retiring thread still consumes the separate bounded
        // retirement allowance until its exact OS-thread exit is observed.
        assert!(guard.close(hb).is_some());
        guard
            .try_ensure_open(&c)
            .expect("the freed active slot admits one replacement");
        assert_eq!(guard.relays_rejected_over_cap(), 1);
    }

    /// Zero is a legacy/default spelling, not an uncapped bypass. It is
    /// normalized to the finite safe default at construction.
    #[test]
    fn zero_max_relays_normalizes_to_the_safe_default() {
        let (tx, _rx) = mpsc::channel();
        let inner = PoolInner::new(
            PoolConfig {
                max_relays: 0,
                ..PoolConfig::default()
            },
            Arc::new(Collector(tx)),
        );
        let mut guard = inner.lock().unwrap();
        for i in 0..crate::DEFAULT_MAX_RELAYS {
            let url = RelayUrl::parse(&format!("wss://relay{i}.example")).unwrap();
            guard
                .try_ensure_open(&url)
                .expect("the finite default must admit its budget");
        }
        let over = RelayUrl::parse("wss://over-default.example").unwrap();
        assert_eq!(
            guard.try_ensure_open(&over),
            Err(RelayOpenError::AtCapacity {
                max_relays: crate::DEFAULT_MAX_RELAYS,
            })
        );
        assert_eq!(guard.relays_rejected_over_cap(), 1);
    }

    #[test]
    fn stale_handle_is_rejected_by_command_tx_for() {
        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        let url = RelayUrl::parse("wss://relay.example").unwrap();
        let h1 = guard.ensure_open(&url);
        guard.close(h1);
        let h2 = guard.ensure_open(&url);
        assert!(
            guard.command_tx_for(h1).is_none(),
            "old handle must be rejected"
        );
        assert!(
            guard.command_tx_for(h2).is_some(),
            "new handle must be valid"
        );
    }

    /// The CRITICAL falsifier (issue #506): a permanent failure (401/403 --
    /// `backoff::is_permanent_error`) on a relay that never even reached
    /// `Connected` must still retire the worker and free its `max_relays`
    /// cap slot -- not merely surface a `Health` update while the zombie
    /// worker keeps squatting the slot forever. Before the fix, this exact
    /// scenario left `state.worker: Some(..)` with `health.state ==
    /// Disconnected`: `try_ensure_open` would then treat the slot as still
    /// "live" (`state.worker.is_some()`) and idempotently hand back the same
    /// dead handle, so `max_relays: 1` would wedge on this one relay
    /// forever.
    #[test]
    fn permanent_failure_before_ever_connecting_retires_the_worker_and_frees_the_cap_slot() {
        let (tx, _rx) = mpsc::channel();
        let inner = PoolInner::new(
            PoolConfig {
                max_relays: 1,
                ..PoolConfig::default()
            },
            Arc::new(Collector(tx)),
        );
        let mut guard = inner.lock().unwrap();
        let url = RelayUrl::parse("wss://relay.example").unwrap();
        let h1 = guard.ensure_open(&url);

        let disconnected = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h1.slot,
                generation: h1.generation,
                kind: WorkerEventKind::Failed {
                    message: "401 Unauthorized".to_string(),
                    permanent: true,
                    retry_in: None,
                },
            },
        );
        assert!(
            matches!(
                disconnected,
                Some(PoolEvent::Disconnected {
                    reason: DisconnectReason::PermanentlyFailed,
                    ..
                })
            ),
            "a permanent failure must surface Disconnected{{PermanentlyFailed}}, \
             even when the relay never reached Connected -- never a silent Health update"
        );

        assert!(
            guard.command_tx_for(h1).is_none(),
            "the retired worker must reject any further send"
        );
        assert!(
            guard
                .live_session_handle(&RelaySessionKey::public(url.clone()))
                .is_none(),
            "the pool must never auto-redial a permanently-failed relay itself"
        );

        // The freed cap slot, not just the health flag: with max_relays: 1,
        // a SECOND distinct relay could not previously open while the
        // zombie worker squatted the pool's one live slot.
        let other = RelayUrl::parse("wss://relay-two.example").unwrap();
        guard
            .try_ensure_open(&other)
            .expect("the permanently-failed relay's cap slot must be freed");
    }

    /// The was-connected sibling of the falsifier above: a permanent
    /// failure arriving AFTER a live session must still report
    /// `PermanentlyFailed` (never the ordinary transient `Error` reason) and
    /// still retire the worker. Losing this distinction is exactly what
    /// would make the engine's `on_relay_disconnected` re-issue
    /// `Effect::EnsureRelay` into a 401 busy-loop.
    #[test]
    fn permanent_failure_after_a_connected_session_reports_permanent_and_retires() {
        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        let url = RelayUrl::parse("wss://relay.example").unwrap();
        let h1 = guard.ensure_open(&url);

        let connected = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h1.slot,
                generation: h1.generation,
                kind: WorkerEventKind::Connected,
            },
        );
        assert!(matches!(connected, Some(PoolEvent::Connected { .. })));

        let disconnected = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h1.slot,
                generation: h1.generation,
                kind: WorkerEventKind::Failed {
                    message: "403 Forbidden".to_string(),
                    permanent: true,
                    retry_in: None,
                },
            },
        );
        assert!(
            matches!(
                disconnected,
                Some(PoolEvent::Disconnected {
                    reason: DisconnectReason::PermanentlyFailed,
                    ..
                })
            ),
            "a permanent failure after a live session must report \
             PermanentlyFailed, never the ordinary transient Error reason"
        );
        assert!(
            guard.command_tx_for(h1).is_none(),
            "the retired worker must reject any further send"
        );
    }

    /// The ingest verification gate wired into `apply_worker_event`
    /// (`WorkerEventKind::Frame`'s arm): a bad-signature `EVENT` frame from
    /// a relay is dropped -- it never becomes a `PoolEvent::Frame` -- and
    /// instead surfaces as a `PoolEvent::Health` with a nonzero
    /// `invalid_signature_count`, the relay-misbehavior signal a caller can
    /// observe. A genuine signed event from the SAME relay/slot afterward
    /// still passes -- the gate rejects forgery, not the relay itself.
    #[test]
    fn tampered_event_frame_is_dropped_and_flags_relay_misbehavior() {
        use nostr::{EventBuilder, Keys, Kind};

        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        let url = RelayUrl::parse("wss://relay.example").unwrap();
        let h = guard.ensure_open(&url);
        let _ = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h.slot,
                generation: h.generation,
                kind: WorkerEventKind::Connected,
            },
        );

        let keys = Keys::generate();
        let mut event = EventBuilder::new(Kind::TextNote, "genuine")
            .sign_with_keys(&keys)
            .expect("test fixture must sign cleanly");
        event.content = "forged in transit".to_string();
        let forged_frame = crate::pool::RelayFrame::from_message(nostr::RelayMessage::event(
            nostr::SubscriptionId::new("s"),
            event,
        ));

        let outcome = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h.slot,
                generation: h.generation,
                kind: WorkerEventKind::Frame(forged_frame),
            },
        );
        match outcome {
            Some(PoolEvent::Health { health, .. }) => {
                assert_eq!(
                    health.invalid_signature_count, 1,
                    "the forged frame must bump the misbehavior counter"
                );
            }
            other => panic!("expected PoolEvent::Health for a rejected frame, got {other:?}"),
        }
        assert_eq!(
            guard.health_for(h).map(|h| h.invalid_signature_count),
            Some(1),
            "the misbehavior count must be visible via Pool::health"
        );

        // A genuine event from the same relay still passes through as a
        // normal Frame -- the gate rejects forgery, not the relay slot.
        let genuine = nmp_resolver_test_event(&keys, "real content");
        let genuine_frame = crate::pool::RelayFrame::from_message(nostr::RelayMessage::event(
            nostr::SubscriptionId::new("s"),
            genuine,
        ));
        let outcome = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h.slot,
                generation: h.generation,
                kind: WorkerEventKind::Frame(genuine_frame),
            },
        );
        assert!(
            matches!(outcome, Some(PoolEvent::Frame { .. })),
            "a genuine event must still be forwarded as a Frame"
        );
    }

    #[test]
    fn ordered_cache_policy_rejects_signature_mismatch_and_does_not_poison_on_invalid() {
        use nostr::{EventBuilder, Keys, Kind};

        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "cache-policy")
            .sign_with_keys(&keys)
            .unwrap();
        let frame = crate::pool::RelayFrame::from_message(nostr::RelayMessage::event(
            nostr::SubscriptionId::new("s"),
            event.clone(),
        ));
        let mut cache = VerifiedEventCache::new(2);
        assert_eq!(
            resolve_candidate_verdict(&mut cache, &frame, VerificationOutcome::Valid),
            FrameVerdict::Accept
        );
        assert_eq!(cache.get(&event.id), Some(event.sig));
        assert_eq!(
            resolve_candidate_verdict(&mut cache, &frame, VerificationOutcome::Valid),
            FrameVerdict::Accept,
            "an exact redelivery remains a cheap cache hit"
        );

        let mut mismatched = event.clone();
        mismatched.sig = EventBuilder::new(Kind::TextNote, "other-signature")
            .sign_with_keys(&keys)
            .unwrap()
            .sig;
        let mismatched = crate::pool::RelayFrame::from_message(nostr::RelayMessage::event(
            nostr::SubscriptionId::new("s"),
            mismatched,
        ));
        assert_eq!(
            resolve_candidate_verdict(&mut cache, &mismatched, VerificationOutcome::Valid),
            FrameVerdict::RejectMisbehavior,
            "a verified id pins its exact signature"
        );

        let later = EventBuilder::new(Kind::TextNote, "invalid-then-valid")
            .sign_with_keys(&keys)
            .unwrap();
        let later_frame = crate::pool::RelayFrame::from_message(nostr::RelayMessage::event(
            nostr::SubscriptionId::new("s"),
            later.clone(),
        ));
        assert_eq!(
            resolve_candidate_verdict(&mut cache, &later_frame, VerificationOutcome::Invalid),
            FrameVerdict::RejectMisbehavior
        );
        assert!(
            cache.get(&later.id).is_none(),
            "invalid work cannot poison cache"
        );
        assert_eq!(
            resolve_candidate_verdict(&mut cache, &later_frame, VerificationOutcome::Valid),
            FrameVerdict::Accept,
            "a later valid sighting must still be admissible"
        );
    }

    #[test]
    fn cached_id_signature_cannot_admit_mutated_event_payload() {
        use nostr::{EventBuilder, Keys, Kind};

        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "canonical")
            .sign_with_keys(&keys)
            .unwrap();
        let mut cache = VerifiedEventCache::new(4);
        cache.insert(event.id, event.sig);

        let valid = crate::pool::RelayFrame::from(nostr::RelayMessage::event(
            nostr::SubscriptionId::new("s"),
            event.clone(),
        ));
        assert_eq!(
            cached_frame_plan(&cache, &valid),
            Some(VerificationPlan::Known(event.sig))
        );

        let mut mutated = event;
        mutated.content.push_str("-forged");
        let mutated = crate::pool::RelayFrame::from(nostr::RelayMessage::event(
            nostr::SubscriptionId::new("s"),
            mutated,
        ));
        assert_eq!(
            cached_frame_plan(&cache, &mutated),
            Some(VerificationPlan::InvalidId)
        );

        let mut valid_first = VerifiedEventCache::new(4);
        assert_eq!(
            resolve_candidate_verdict(&mut valid_first, &valid, VerificationOutcome::Valid),
            FrameVerdict::Accept
        );
        assert_eq!(
            resolve_candidate_verdict(&mut valid_first, &mutated, VerificationOutcome::Invalid),
            FrameVerdict::RejectMisbehavior,
            "a prior valid cache insert cannot override this payload's failed proof"
        );

        let mut invalid_first = VerifiedEventCache::new(4);
        assert_eq!(
            resolve_candidate_verdict(&mut invalid_first, &mutated, VerificationOutcome::Invalid),
            FrameVerdict::RejectMisbehavior
        );
        assert_eq!(
            resolve_candidate_verdict(&mut invalid_first, &valid, VerificationOutcome::Valid),
            FrameVerdict::Accept,
            "an invalid sibling cannot poison the later valid payload"
        );
    }

    #[test]
    fn same_batch_connected_transition_makes_following_frame_and_completion_current() {
        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        let url = RelayUrl::parse("wss://relay.example").unwrap();
        let handle = guard.ensure_open(&url);
        let connected_generation = pack_generation(worker_id_of(handle.generation), 1);
        let events = vec![
            WorkerEvent {
                slot: handle.slot,
                generation: connected_generation,
                kind: WorkerEventKind::Connected,
            },
            WorkerEvent {
                slot: handle.slot,
                generation: connected_generation,
                kind: WorkerEventKind::Frame(crate::pool::RelayFrame::from(
                    nostr::RelayMessage::notice("after reconnect"),
                )),
            },
            WorkerEvent {
                slot: handle.slot,
                generation: connected_generation,
                kind: WorkerEventKind::InitialReadCompleted,
            },
        ];

        assert_eq!(planned_currentness(&guard, &events), vec![true, true, true]);
        guard.shutdown();
    }

    #[test]
    fn verification_cache_is_strictly_bounded_and_eviction_only_forgets() {
        use nostr::{EventBuilder, Keys, Kind};

        let keys = Keys::generate();
        let first = EventBuilder::new(Kind::TextNote, "first")
            .sign_with_keys(&keys)
            .unwrap();
        let second = EventBuilder::new(Kind::TextNote, "second")
            .sign_with_keys(&keys)
            .unwrap();
        let mut cache = VerifiedEventCache::new(1);
        cache.insert(first.id, first.sig);
        cache.insert(second.id, second.sig);
        assert_eq!(cache.signatures.len(), 1);
        assert!(cache.get(&first.id).is_none());
        assert_eq!(cache.get(&second.id), Some(second.sig));
    }

    /// Test-only helper: a properly signed kind:1 event (mirrors
    /// `nmp_resolver::testkit::kind1`, duplicated here rather than pulled in
    /// as a dependency -- `nmp-transport` depends on no other NMP crate).
    fn nmp_resolver_test_event(keys: &nostr::Keys, content: &str) -> nostr::Event {
        nostr::EventBuilder::new(nostr::Kind::TextNote, content)
            .sign_with_keys(keys)
            .expect("test fixture must sign cleanly")
    }

    // ---- issue #93: durable EVENT handoff ---------------------------

    /// The one exception to every other generation/slot gate in
    /// `apply_worker_event`: an `EventHandoff` for a slot that was closed
    /// and reopened under a BRAND NEW generation must still reach the sink.
    /// Gating it like `Frame` would silently strand the correlation with no
    /// answer at all -- exactly the failure mode #93 removes.
    #[test]
    fn event_handoff_from_a_closed_and_reopened_slot_still_delivers() {
        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        let url = RelayUrl::parse("wss://relay.example").unwrap();

        let h1 = guard.ensure_open(&url);
        assert!(guard.close(h1).is_some());
        let _h2 = guard.ensure_open(&url);

        let delivered = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h1.slot,
                generation: h1.generation,
                kind: WorkerEventKind::EventHandoff {
                    correlation: crate::pool::AttemptCorrelation(1),
                    result: crate::pool::HandoffResult::NotHandedOff,
                },
            },
        );
        assert!(
            matches!(
                delivered,
                Some(PoolEvent::EventHandoff {
                    result: crate::pool::HandoffResult::NotHandedOff,
                    ..
                })
            ),
            "a durable EVENT's resolution must reach the sink even from a slot that has since \
             closed and reopened under a new generation, got {delivered:?}"
        );
    }

    /// `EventHandoff` delivery does not even need a valid slot index --
    /// unlike every other `WorkerEvent` variant, it carries no slot-state
    /// dependency at all (a correlation is engine-minted, not pool-slot-
    /// scoped), so an "unknown" slot number still delivers. This is the
    /// same invariant as the closed-and-reopened-slot case above, taken to
    /// its logical extreme: NOTHING about pool/slot bookkeeping may ever
    /// swallow a durable EVENT's one-and-only resolution.
    #[test]
    fn event_handoff_delivers_even_for_an_out_of_range_slot() {
        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        let outcome = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: 999,
                generation: 0,
                kind: WorkerEventKind::EventHandoff {
                    correlation: crate::pool::AttemptCorrelation(7),
                    result: crate::pool::HandoffResult::Ambiguous,
                },
            },
        );
        assert!(matches!(
            outcome,
            Some(PoolEvent::EventHandoff {
                result: crate::pool::HandoffResult::Ambiguous,
                ..
            })
        ));
    }

    /// `Pool::send_durable` against a stale (superseded) handle must
    /// resolve `NotHandedOff` synchronously -- the correlation never even
    /// reaches a live worker's command channel, so there is nothing
    /// asynchronous left to wait for.
    #[test]
    fn send_durable_on_a_stale_handle_resolves_not_handed_off_synchronously() {
        use crate::pool::{AttemptCorrelation, HandoffResult, Pool, WireFrame};

        let (tx, rx) = mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), tx).expect("test pool construction");
        let url = RelayUrl::parse("wss://relay.example").unwrap();
        let h1 = pool.ensure_open(&url).expect("relay admitted");
        assert!(pool.close(h1).is_some());

        let correlation = AttemptCorrelation(42);
        let outcome = pool.send_durable(h1, correlation, WireFrame::Text("[]".into()));
        assert_eq!(
            outcome,
            crate::pool::DurableSendOutcome::Resolved(HandoffResult::NotHandedOff)
        );
        assert!(rx.try_recv().is_err(), "immediate resolution stays local");
        pool.shutdown();
    }

    #[test]
    fn public_open_boundary_returns_typed_capacity_and_shutdown_refusals() {
        let (tx, _rx) = mpsc::channel();
        let pool = Pool::new(
            PoolConfig {
                max_relays: 1,
                ..PoolConfig::default()
            },
            tx,
        )
        .expect("test pool construction");
        let admitted = RelayUrl::parse("wss://admitted.example").unwrap();
        let refused = RelayUrl::parse("wss://refused.example").unwrap();

        assert!(pool.ensure_open(&admitted).is_ok());
        assert_eq!(
            pool.ensure_open(&refused),
            Err(crate::pool::RelayOpenError::AtCapacity { max_relays: 1 })
        );

        pool.shutdown();
        assert_eq!(
            pool.ensure_open(&admitted),
            Err(crate::pool::RelayOpenError::ShuttingDown)
        );
    }

    #[test]
    fn poisoned_pool_lock_still_resolves_durable_handoff_synchronously() {
        let (tx, rx) = mpsc::channel();
        let inner = PoolInner::new(PoolConfig::default(), Arc::new(Collector(tx)));
        let pool = Pool {
            inner: Arc::clone(&inner),
        };
        let poison = Arc::clone(&inner);
        let _ = std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("intentional poison");
        })
        .join();

        let correlation = crate::pool::AttemptCorrelation(99);
        assert_eq!(
            pool.send_durable(
                RelayHandle {
                    slot: u32::MAX,
                    generation: 0,
                },
                correlation,
                WireFrame::Text("[]".into()),
            ),
            crate::pool::DurableSendOutcome::Resolved(crate::pool::HandoffResult::NotHandedOff)
        );
        assert!(rx.try_recv().is_err());

        inner.clear_poison();
        pool.shutdown();
    }
}
