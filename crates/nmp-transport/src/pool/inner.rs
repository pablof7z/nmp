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

use nostr::secp256k1::schnorr::Signature;
use nostr::{EventId, RelayUrl};

use crate::handle::RelayHandle;
use crate::health::{ConnState, RelayHealth};

use super::verify::{self, GateVerdict};
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
    /// Pool-global ingest verification cache (`pool::verify::gate`): every
    /// distinct event id that has passed `Event::verify()` once, mapped to
    /// the signature that verified. Shared across every slot -- deliberately
    /// NOT per-relay -- because a redelivery of the same event by a second
    /// relay must reuse this cache, not re-run the schnorr check. See
    /// `verify::gate`'s doc for the accept/reject/pass-through rules.
    verified_events: HashMap<EventId, Signature>,
    /// Count of [`Self::ensure_open`] calls refused because opening the relay
    /// would have taken the pool past `config.max_relays` LIVE workers (issue
    /// #121, the worker-exhaustion half). Monotonic; read (never reset) by
    /// [`super::Pool::admission_rejections`] so the engine can fold it into
    /// its diagnostics rejection counter. `config.max_relays == 0` disables
    /// the cap entirely, so this only ever moves when an operator configured
    /// a real ceiling.
    relays_rejected_over_cap: u64,
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
            verified_events: HashMap::new(),
            relays_rejected_over_cap: 0,
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
                // Idempotent: a live slot for this URL already exists — never
                // counted against the cap (it is already one of the live
                // relays the cap bounds).
                return RelayHandle {
                    slot: slot_id,
                    generation: state.generation,
                };
            }
            // Reopening a previously-closed slot makes a worker LIVE again,
            // so it is subject to the same live-relay ceiling as a brand-new
            // relay.
            if self.would_exceed_relay_cap() {
                self.relays_rejected_over_cap += 1;
                return RelayHandle {
                    slot: u32::MAX,
                    generation: 0,
                };
            }
            return self.reopen(slot_id, url.clone());
        }
        if self.would_exceed_relay_cap() {
            self.relays_rejected_over_cap += 1;
            return RelayHandle {
                slot: u32::MAX,
                generation: 0,
            };
        }
        self.open_new(url.clone())
    }

    /// The relay-count admission cap (issue #121): with a configured
    /// `max_relays > 0`, refuse to bring a NEW live worker up once
    /// `max_relays` workers are already live. `max_relays == 0` (the
    /// [`PoolConfig::default`] value every existing call site uses) disables
    /// the cap, so this is a pure no-op unless an operator sets a real
    /// ceiling — the worker-exhaustion backstop stays dormant by default.
    fn would_exceed_relay_cap(&self) -> bool {
        let cap = self.config.max_relays;
        cap != 0 && self.live_worker_count() >= cap
    }

    /// Distinct relays currently backed by a live worker (a slot whose
    /// `worker` has not been taken by `close`/`shutdown`).
    fn live_worker_count(&self) -> usize {
        self.slots.iter().filter(|s| s.worker.is_some()).count()
    }

    /// Read the monotonic count of relay-cap rejections (issue #121). See
    /// [`Self::relays_rejected_over_cap`].
    pub(super) fn relays_rejected_over_cap(&self) -> u64 {
        self.relays_rejected_over_cap
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
        let reconnect_jitter_max = self
            .config
            .reconnect_jitter_max
            .unwrap_or(crate::backoff::RECONNECT_JITTER_MAX);
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
            reconnect_jitter_max,
        )
    }

    pub(super) fn command_tx_for(&self, h: RelayHandle) -> Option<&WorkerHandle> {
        let state = self.slots.get(h.slot as usize)?;
        if state.generation != h.generation || state.health.state == ConnState::Disconnected {
            return None;
        }
        state.worker.as_ref()
    }

    /// Clone the sink handle. Used by [`super::Pool::send_durable`] to
    /// resolve an [`super::AttemptCorrelation`] synchronously as
    /// `NotHandedOff` when the frame never even reaches a live worker's
    /// command channel — the sink itself outlives every slot (dropped only
    /// by [`Self::shutdown`], by which point no caller can still be racing
    /// a `send_durable`, since `Pool::shutdown` joins the translator and the
    /// pool is `Arc`-shared, so any in-flight `send_durable` call already
    /// holds its own clone of this `Arc` before the lock is ever released).
    pub(super) fn sink(&self) -> Arc<dyn PoolEventSink> {
        Arc::clone(&self.sink)
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
        let mut events = vec![event];
        events.extend(worker_event_rx.try_iter().take(127));
        let Ok(mut guard) = inner.lock() else { break };
        let frame_positions: Vec<_> = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                let WorkerEventKind::Frame(frame) = &event.kind else {
                    return None;
                };
                let state = guard.slots.get(event.slot as usize)?;
                let current = state.worker.is_some()
                    && worker_id_of(event.generation) == worker_id_of(state.generation)
                    && event.generation == state.generation;
                current.then_some((index, frame))
            })
            .collect();
        let frame_refs: Vec<_> = frame_positions.iter().map(|(_, frame)| *frame).collect();
        let frame_verdicts = verify::gate_batch(&mut guard.verified_events, &frame_refs);
        let mut verdict_by_event = vec![None; events.len()];
        for ((position, _frame), verdict) in frame_positions.into_iter().zip(frame_verdicts) {
            verdict_by_event[position] = Some(verdict);
        }
        let pool_events: Vec<_> = events
            .into_iter()
            .zip(verdict_by_event)
            .filter_map(|(event, verdict)| {
                apply_worker_event_with_verdict(&mut guard, event, verdict)
            })
            .collect();
        // Clone the sink handle (Arc bump) and drop the lock before
        // delivering, so a slow/blocking sink can never stall a concurrent
        // `Pool::send`/`ensure_open` (mirrors the harvested source's
        // off-lock delivery discipline).
        let sink = Arc::clone(&guard.sink);
        drop(guard);
        for pool_event in pool_events {
            sink.on_event(pool_event);
        }
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
#[cfg(test)]
fn apply_worker_event(inner: &mut PoolInner, event: WorkerEvent) -> Option<PoolEvent> {
    apply_worker_event_with_verdict(inner, event, None)
}

fn apply_worker_event_with_verdict(
    inner: &mut PoolInner,
    event: WorkerEvent,
    preverified: Option<GateVerdict>,
) -> Option<PoolEvent> {
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
            // Ingest verification gate (network-boundary, kind-blind --
            // see `pool::verify`'s module doc): a frame that fails here is
            // dropped BEFORE it ever becomes a `PoolEvent::Frame` -- never
            // forwarded to the engine/store/routing. `verified_events` is
            // pool-global (not per-slot) so a redelivery of the same event
            // id by a DIFFERENT relay still hits the cache-compare fast
            // path instead of re-running schnorr.
            match preverified.unwrap_or_else(|| verify::gate(&mut inner.verified_events, &frame)) {
                GateVerdict::PassThrough | GateVerdict::Accept => Some(PoolEvent::Frame {
                    handle: RelayHandle {
                        slot: event.slot,
                        generation: event.generation,
                    },
                    frame,
                }),
                GateVerdict::Reject => {
                    verify::record_misbehavior(&mut state.health);
                    Some(PoolEvent::Health {
                        slot: event.slot,
                        health: state.health.clone(),
                    })
                }
            }
        }
        WorkerEventKind::EventHandoff { .. } => {
            unreachable!("EventHandoff already returned above, before any slot lookup")
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

    /// The relay-count admission cap (issue #121, worker-exhaustion half):
    /// with `max_relays: 2`, the pool opens two distinct relays but REFUSES
    /// the third — returning the stale/dead sentinel (never a live slot) and
    /// bumping the observable rejection counter — while an already-open relay
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

        let ha = guard.ensure_open(&a);
        let hb = guard.ensure_open(&b);
        assert_ne!(ha.slot, u32::MAX, "first relay must open");
        assert_ne!(hb.slot, u32::MAX, "second relay must open");
        assert_eq!(guard.relays_rejected_over_cap(), 0, "no rejection yet");

        // The third DISTINCT relay is over the cap: refused with the dead
        // sentinel, no slot created, counter bumped.
        let hc = guard.ensure_open(&c);
        assert_eq!(
            hc.slot,
            u32::MAX,
            "third relay must be refused past the cap"
        );
        assert_eq!(guard.relays_rejected_over_cap(), 1);
        assert!(
            guard.command_tx_for(hc).is_none(),
            "a cap-refused handle must be a structural no-op, exactly like a stale handle"
        );

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

        // Closing one relay frees a slot in the live budget; the next new
        // relay is admitted again.
        assert!(guard.close(hb));
        let hc2 = guard.ensure_open(&c);
        assert_ne!(hc2.slot, u32::MAX, "a freed slot lets a new relay in again");
        assert_eq!(
            guard.relays_rejected_over_cap(),
            1,
            "the successful open is not a rejection"
        );
    }

    /// The default (`max_relays: 0`) must impose NO ceiling — every existing
    /// call site constructs the pool with `PoolConfig::default()`, so the
    /// cap must stay dormant until an operator opts in.
    #[test]
    fn zero_max_relays_imposes_no_cap() {
        let (inner, _rx) = test_pool();
        let mut guard = inner.lock().unwrap();
        for i in 0..8 {
            let url = RelayUrl::parse(&format!("wss://relay{i}.example")).unwrap();
            assert_ne!(
                guard.ensure_open(&url).slot,
                u32::MAX,
                "max_relays: 0 must never refuse a relay"
            );
        }
        assert_eq!(guard.relays_rejected_over_cap(), 0);
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

    /// The ingest verification gate wired into `apply_worker_event`
    /// (`WorkerEventKind::Frame`'s arm): a bad-signature `EVENT` frame from
    /// a relay is dropped -- it never becomes a `PoolEvent::Frame` -- and
    /// instead surfaces as a `PoolEvent::Health` with a nonzero
    /// `invalid_signature_count`, the relay-misbehavior signal a caller can
    /// observe. A genuine signed event from the SAME relay/slot afterward
    /// still passes -- the gate rejects forgery, not the relay itself.
    #[test]
    fn tampered_event_frame_is_dropped_and_flags_relay_misbehavior() {
        use nostr::{EventBuilder, JsonUtil, Keys, Kind};

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
        let forged_text =
            nostr::RelayMessage::event(nostr::SubscriptionId::new("s"), event).as_json();

        let outcome = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h.slot,
                generation: h.generation,
                kind: WorkerEventKind::Frame(crate::pool::RelayFrame::Text(forged_text)),
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
        let genuine_text =
            nostr::RelayMessage::event(nostr::SubscriptionId::new("s"), genuine).as_json();
        let outcome = apply_worker_event(
            &mut guard,
            WorkerEvent {
                slot: h.slot,
                generation: h.generation,
                kind: WorkerEventKind::Frame(crate::pool::RelayFrame::Text(genuine_text)),
            },
        );
        assert!(
            matches!(outcome, Some(PoolEvent::Frame { .. })),
            "a genuine event must still be forwarded as a Frame"
        );
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
        assert!(guard.close(h1));
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
        let pool = Pool::new(PoolConfig::default(), tx);
        let url = RelayUrl::parse("wss://relay.example").unwrap();
        let h1 = pool.ensure_open(&url);
        pool.close(h1);

        let correlation = AttemptCorrelation(42);
        let handed_off = pool.send_durable(h1, correlation, WireFrame::Text("[]".into()));
        assert!(!handed_off);

        // Drain events until the handoff resolution shows up -- `close`
        // also emits a synchronous `Disconnected` first.
        let mut found = None;
        for event in rx.iter().take(4) {
            if let PoolEvent::EventHandoff {
                correlation: c,
                result,
            } = event
            {
                assert_eq!(c, correlation);
                found = Some(result);
                break;
            }
        }
        assert!(matches!(found, Some(HandoffResult::NotHandedOff)));
        pool.shutdown();
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
        assert!(!pool.send_durable(
            RelayHandle {
                slot: u32::MAX,
                generation: 0,
            },
            correlation,
            WireFrame::Text("[]".into()),
        ));
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(PoolEvent::EventHandoff {
                correlation: found,
                result: crate::pool::HandoffResult::NotHandedOff,
            }) if found == correlation
        ));

        inner.clear_poison();
        pool.shutdown();
    }
}
