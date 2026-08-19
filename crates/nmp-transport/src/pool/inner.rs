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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use nostr::Event;

use super::spawn::ThreadSpawner;
use super::verify::{Verdict, Verifier};
use super::{ThreadRole, ThreadSpawnError};
use crate::handle::RelayHandle;
use crate::health::{ConnState, RelayHealth};

use super::worker::{
    pack_generation, worker_id_of, ReconnectPreambleRegistration, WorkerCommand, WorkerEvent,
    WorkerEventKind, WorkerHandle,
};
use super::{
    committed_observations::CommittedObservationCache, DisconnectReason, PoolBuildError,
    PoolConfig, PoolEvent, PoolEventSink, RelayOpenError, RelaySessionKey,
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
    pub(super) committed_observations: Arc<CommittedObservationCache>,
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
        verifier: Verifier,
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
        let translator_config = config.clone();
        let committed_observations = Arc::new(CommittedObservationCache::new(
            config.committed_observation_cache_capacity,
        ));
        let inner = Arc::new(Mutex::new(Self {
            committed_observations,
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
        self.retire_worker_with_frames(slot, generation, worker, Vec::new());
    }

    fn retire_worker_with_frames(
        &mut self,
        slot: u32,
        generation: u64,
        worker: WorkerHandle,
        frames: Vec<String>,
    ) {
        let worker_id = worker_id_of(generation);
        let request = RetireRequest {
            slot,
            generation,
            worker_id,
            join: worker.retire_with_frames(frames),
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
        let idle = crate::keepalive::KEEPALIVE_IDLE_THRESHOLD;
        let pong_timeout = crate::keepalive::KEEPALIVE_PONG_TIMEOUT;
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
            session.relay.clone(),
            session.authenticate_as.is_some(),
            self.worker_event_tx
                .as_ref()
                .expect("spawn_worker never called after shutdown (ensure_open guards it)")
                .clone(),
            idle,
            pong_timeout,
            reconnect_delay_initial,
            reconnect_jitter_max,
            command_queue_capacity,
            Arc::clone(&self.committed_observations),
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

    pub(super) fn reconnect_preamble_registration_for(
        &self,
        h: RelayHandle,
    ) -> Option<ReconnectPreambleRegistration> {
        let state = self.slots.get(h.slot as usize)?;
        if state.generation != h.generation {
            return None;
        }
        state
            .worker
            .as_ref()
            .map(WorkerHandle::reconnect_preamble_registration)
    }

    pub(super) fn replay_reconnect_preamble_for(&self, h: RelayHandle) -> bool {
        let Some(state) = self.slots.get(h.slot as usize) else {
            return false;
        };
        if state.generation != h.generation || state.health.state != ConnState::Connected {
            return false;
        }
        match state.worker.as_ref() {
            Some(worker) => worker.replay_reconnect_preamble(h.generation),
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

    /// Release obsolete sessions, flushing the caller-supplied terminal text
    /// frames on the exact still-connected generation before retirement.
    pub(super) fn close_unrequired_sessions(
        &mut self,
        required: &BTreeSet<RelaySessionKey>,
        mut frames: BTreeMap<RelaySessionKey, Vec<String>>,
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
            .filter_map(|handle| {
                let state = self.slots.get_mut(handle.slot as usize)?;
                if state.generation != handle.generation {
                    return None;
                }
                let worker = state.worker.take()?;
                let generation = state.generation;
                let session = state.session.clone();
                state.health.state = ConnState::Disconnected;
                let terminal = frames.remove(&session).unwrap_or_default();
                self.retire_worker_with_frames(handle.slot, generation, worker, terminal);
                Some(PoolEvent::Disconnected {
                    handle,
                    session,
                    reason: DisconnectReason::Closed,
                })
            })
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
    verifier: Verifier,
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
    mut verifier: Verifier,
) {
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

        // Split the batch without holding PoolInner. Stale frames and
        // non-frame events bypass the trust gate entirely. EVENT frames
        // with a valid id are handed to the verify gate; a mutated payload
        // (failed id recompute) is rejected as misbehavior here, before any
        // schnorr work. the verify gate owns the durable/LRU byte-compare fast
        // paths and the candidate-by-pair dedup, so transport no longer
        // holds a verified cache or a verification plan.
        //
        // `verify_index[i]` records which `verify_events` slot position `i`
        // was handed, so the stitch loop can map results back by explicit
        // index even if some verified frames go stale while crypto runs
        // (a sequential counter would desync across skipped positions).
        let mut verdicts: Vec<Option<Verdict>> = vec![None; events.len()];
        let mut verify_index: Vec<Option<usize>> = vec![None; events.len()];
        let mut verify_events: Vec<Arc<Event>> = Vec::new();
        for (i, (event, current)) in events.iter().zip(current).enumerate() {
            if !current {
                continue;
            }
            let WorkerEventKind::Frame(frame) = &event.kind else {
                continue;
            };
            let Some(event) = frame.event() else {
                // A non-EVENT frame has nothing to verify; forward it.
                verdicts[i] = Some(Verdict::Accept);
                continue;
            };
            if !frame_event_id_is_valid(event) {
                verdicts[i] = Some(Verdict::RejectMisbehavior);
                continue;
            }
            verify_index[i] = Some(verify_events.len());
            verify_events.push(Arc::clone(event));
        }

        let verify_results = verifier.verify_batch(&verify_events);

        let Ok(mut guard) = inner.lock() else { break };
        let mut pool_events = Vec::with_capacity(events.len());
        // Stitch the verify results back onto their batch positions by
        // explicit index, then re-check currentness under the lock: a slot
        // can close/reopen while crypto is running, and a frame that goes
        // stale is dropped (None) — neither cached nor treated as relay
        // misbehavior. Its verify result is simply not consumed by anyone.
        for (i, (event, verdict)) in events.into_iter().zip(verdicts.iter_mut()).enumerate() {
            let is_current_frame =
                matches!(event.kind, WorkerEventKind::Frame(_)) && frame_is_current(&guard, &event);
            if is_current_frame && verdict.is_none() {
                // This position was handed to the verify gate; consume its result
                // by the recorded index (not a counter, so stale-skip does
                // not desync the mapping).
                *verdict = Some(
                    verify_results[verify_index[i].expect("current verified frame has an index")],
                );
            }
            let preverified = if is_current_frame { *verdict } else { None };
            if let Some(pool_event) =
                apply_worker_event_with_verdict(&mut guard, event, preverified)
            {
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
        drop(verify_events);
        for pool_event in pool_events {
            sink.on_event(pool_event);
        }
    }
}

/// Bump the observable relay-misbehavior counter for a rejected event.
fn record_misbehavior(health: &mut RelayHealth) {
    health.invalid_signature_count += 1;
}

/// Surface an internal verifier outage without attributing it to the relay.
fn record_unavailable(health: &mut RelayHealth) {
    health.last_error = Some("signature verification worker unavailable".to_string());
}

/// Recompute and check the NIP-01 event id (transport pre-check). The
/// event-id recompute stays in transport (#1677 non-goal: parse and
/// event-id recompute are not moved); the verify gate only ever sees
/// id-valid, non-stale events. A mutated payload fails here and is
/// rejected as misbehavior before any schnorr work.
fn frame_event_id_is_valid(event: &Event) -> bool {
    let skip_event_id = false;
    let valid_id = skip_event_id || event.verify_id();
    valid_id
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

fn apply_worker_event_with_verdict(
    inner: &mut PoolInner,
    event: WorkerEvent,
    preverified: Option<Verdict>,
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

    // Issue #883: an exact ephemeral terminal is ungated for exactly the same
    // reason, and needs no slot lookup at all — the worker already named the
    // exact `(session, generation)` the operation was submitted against, so
    // nothing here can misattribute it to whatever the slot holds now.
    if let WorkerEventKind::EphemeralHandoff { target, outcome } = event.kind {
        return Some(PoolEvent::EphemeralHandoff {
            operation: target.operation,
            session: target.session,
            handle: RelayHandle {
                slot: event.slot,
                generation: target.generation,
            },
            outcome,
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
                // leaves `state.worker.is_none()` so a subsequent
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
                Verdict::Accept => Some(PoolEvent::Frame {
                    handle: RelayHandle {
                        slot: event.slot,
                        generation: event.generation,
                    },
                    session: state.session.clone(),
                    frame,
                }),
                // A known id redelivered with a different signature. Drop the
                // frame and record NOTHING against the relay: a mismatch is
                // not evidence of misbehavior. Dropping is not free — see
                // `Verdict::Skip` for what a live query can lose
                // on the LRU branch, tracked in #1862.
                Verdict::Skip => None,
                Verdict::RejectMisbehavior => {
                    record_misbehavior(&mut state.health);
                    Some(PoolEvent::Health {
                        handle: RelayHandle {
                            slot: event.slot,
                            generation: event.generation,
                        },
                        session: state.session.clone(),
                        health: state.health.clone(),
                    })
                }
                Verdict::RejectUnavailable => {
                    record_unavailable(&mut state.health);
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
        WorkerEventKind::UndecodableFrame => {
            if !same_worker || event.generation != state.generation {
                return None;
            }
            // Gated exactly like `Frame` above: a report from a retired
            // worker or a superseded generation says nothing about the
            // session occupying this slot now.
            state.health.record_undecodable_frame();
            Some(PoolEvent::Health {
                handle: RelayHandle {
                    slot: event.slot,
                    generation: event.generation,
                },
                session: state.session.clone(),
                health: state.health.clone(),
            })
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
        WorkerEventKind::EphemeralHandoff { .. } => {
            unreachable!("EphemeralHandoff already returned above, before any slot lookup")
        }
        WorkerEventKind::Retired { .. } => {
            unreachable!("Retired already returned above, before any slot lookup")
        }
    }
}

