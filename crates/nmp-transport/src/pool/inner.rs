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

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use nostr::RelayUrl;

use crate::handle::RelayHandle;
use crate::health::{ConnState, RelayHealth};

use super::worker::{
    pack_generation, worker_id_of, WorkerCommand, WorkerEvent, WorkerEventKind, WorkerHandle,
};
use super::{DisconnectReason, PoolConfig, PoolEvent, PoolEventSink};

struct SlotState {
    url: RelayUrl,
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
    /// reopen of the SAME url (matching `url_to_slot`).
    slots: Vec<SlotState>,
    url_to_slot: HashMap<RelayUrl, u32>,
    /// Bumped on every fresh worker spawn (new url or reopen-after-close).
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
    worker_event_tx: Option<Sender<WorkerEvent>>,
    config: PoolConfig,
    translator: Option<JoinHandle<()>>,
    shutdown: bool,
}

impl PoolInner {
    pub(super) fn new(config: PoolConfig, sink: Arc<dyn PoolEventSink>) -> Arc<Mutex<Self>> {
        let (worker_event_tx, worker_event_rx) = mpsc::channel::<WorkerEvent>();
        let inner = Arc::new(Mutex::new(Self {
            slots: Vec::new(),
            url_to_slot: HashMap::new(),
            next_worker_id: 0,
            sink,
            worker_event_tx: Some(worker_event_tx),
            config,
            translator: None,
            shutdown: false,
        }));
        let translator = spawn_translator(Arc::clone(&inner), worker_event_rx);
        if let Ok(mut guard) = inner.lock() {
            guard.translator = Some(translator);
        }
        inner
    }

    pub(super) fn ensure_open(&mut self, url: &RelayUrl) -> RelayHandle {
        if self.shutdown {
            return RelayHandle {
                slot: u32::MAX,
                generation: 0,
            };
        }
        if let Some(&slot_id) = self.url_to_slot.get(url) {
            let state = &self.slots[slot_id as usize];
            if state.worker.is_some() {
                // Idempotent: a live slot for this URL already exists.
                return RelayHandle {
                    slot: slot_id,
                    generation: state.generation,
                };
            }
            return self.reopen(slot_id, url.clone());
        }
        self.open_new(url.clone())
    }

    fn open_new(&mut self, url: RelayUrl) -> RelayHandle {
        let slot_id = u32::try_from(self.slots.len()).expect("pool slot id overflow");
        let worker_id = self.next_worker_id;
        self.next_worker_id += 1;
        let generation = pack_generation(worker_id, 0);
        let worker = self.spawn_worker(slot_id, worker_id, &url);
        self.slots.push(SlotState {
            url: url.clone(),
            worker: Some(worker),
            generation,
            health: RelayHealth {
                state: ConnState::Connecting,
                ..RelayHealth::default()
            },
        });
        self.url_to_slot.insert(url, slot_id);
        RelayHandle {
            slot: slot_id,
            generation,
        }
    }

    fn reopen(&mut self, slot_id: u32, url: RelayUrl) -> RelayHandle {
        let worker_id = self.next_worker_id;
        self.next_worker_id += 1;
        let generation = pack_generation(worker_id, 0);
        let worker = self.spawn_worker(slot_id, worker_id, &url);
        self.slots[slot_id as usize] = SlotState {
            url,
            worker: Some(worker),
            generation,
            health: RelayHealth {
                state: ConnState::Connecting,
                ..RelayHealth::default()
            },
        };
        RelayHandle {
            slot: slot_id,
            generation,
        }
    }

    fn spawn_worker(&self, slot_id: u32, worker_id: u32, url: &RelayUrl) -> WorkerHandle {
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
        super::worker::spawn(
            slot_id,
            worker_id,
            url.as_str().to_string(),
            self.worker_event_tx
                .as_ref()
                .expect("spawn_worker never called after shutdown (ensure_open guards it)")
                .clone(),
            idle,
            pong_timeout,
            reconnect_delay_initial,
        )
    }

    pub(super) fn command_tx_for(&self, h: RelayHandle) -> Option<&WorkerHandle> {
        let state = self.slots.get(h.slot as usize)?;
        if state.generation != h.generation {
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

    pub(super) fn health_for(&self, h: RelayHandle) -> Option<RelayHealth> {
        let state = self.slots.get(h.slot as usize)?;
        if state.generation != h.generation {
            return None;
        }
        Some(state.health.clone())
    }

    /// Close the slot for `h`. Pushes `PoolEvent::Disconnected` synchronously
    /// — the pool already knows the outcome, no need to wait on the worker.
    pub(super) fn close(&mut self, h: RelayHandle) -> bool {
        let Some(state) = self.slots.get_mut(h.slot as usize) else {
            return false;
        };
        if state.generation != h.generation {
            return false;
        }
        let Some(worker) = state.worker.take() else {
            return false;
        };
        worker.push(WorkerCommand::Shutdown);
        state.health.state = ConnState::Disconnected;
        self.sink.on_event(PoolEvent::Disconnected {
            slot: h.slot,
            reason: DisconnectReason::Closed,
        });
        true
    }

    /// Tear down every open worker, hand back the translator's `JoinHandle`
    /// so the caller can join it *outside* this lock (the translator locks
    /// `PoolInner` per event; joining while holding the lock deadlocks).
    pub(super) fn shutdown(&mut self) -> Option<JoinHandle<()>> {
        self.shutdown = true;
        for (slot_id, state) in self.slots.iter_mut().enumerate() {
            let Some(worker) = state.worker.take() else {
                continue;
            };
            worker.push(WorkerCommand::Shutdown);
            state.health.state = ConnState::Disconnected;
            self.sink.on_event(PoolEvent::Disconnected {
                slot: slot_id as u32,
                reason: DisconnectReason::ShuttingDown,
            });
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
        self.worker_event_tx = None;
        self.translator.take()
    }
}

fn spawn_translator(
    inner: Arc<Mutex<PoolInner>>,
    worker_event_rx: std::sync::mpsc::Receiver<WorkerEvent>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("nmp-transport-pool-translator".to_string())
        .spawn(move || translator_loop(&inner, &worker_event_rx))
        .expect("translator thread spawn must succeed")
}

fn translator_loop(
    inner: &Arc<Mutex<PoolInner>>,
    worker_event_rx: &std::sync::mpsc::Receiver<WorkerEvent>,
) {
    while let Ok(event) = worker_event_rx.recv() {
        let Ok(mut guard) = inner.lock() else { break };
        let Some(pool_event) = apply_worker_event(&mut guard, event) else {
            continue;
        };
        // Clone the sink handle (Arc bump) and drop the lock before
        // delivering, so a slow/blocking sink can never stall a concurrent
        // `Pool::send`/`ensure_open` (mirrors the harvested source's
        // off-lock delivery discipline).
        let sink = Arc::clone(&guard.sink);
        drop(guard);
        sink.on_event(pool_event);
    }
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
fn apply_worker_event(inner: &mut PoolInner, event: WorkerEvent) -> Option<PoolEvent> {
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
                url: state.url.clone(),
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
            if was_connected {
                Some(PoolEvent::Disconnected {
                    slot: event.slot,
                    reason: DisconnectReason::Error,
                })
            } else {
                Some(PoolEvent::Health {
                    slot: event.slot,
                    health: state.health.clone(),
                })
            }
        }
        WorkerEventKind::Frame(frame) => {
            if !same_worker || event.generation != state.generation {
                return None;
            }
            Some(PoolEvent::Frame {
                handle: RelayHandle {
                    slot: event.slot,
                    generation: event.generation,
                },
                frame,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{PoolConfig, PoolEvent};
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
        assert!(guard.close(h1));
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
                kind: WorkerEventKind::Frame(crate::pool::RelayFrame::Text("late".into())),
            },
        );
        assert!(stale.is_none(), "stale-worker frame must be dropped");

        // A frame from the NEW (h2) worker is accepted.
        let fresh = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h2.slot,
                generation: h2.generation,
                kind: WorkerEventKind::Frame(crate::pool::RelayFrame::Text("fresh".into())),
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
}
