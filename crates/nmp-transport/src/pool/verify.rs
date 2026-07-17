//! Persistent ingest-time signature verification workers.
//!
//! Parsing and relay-frame policy live at the translator boundary. This module
//! deliberately accepts already-parsed [`Event`] values so the same parse can
//! be reused by routing, caching, and persistence. The translator recomputes
//! each candidate's event id once before dispatch; these workers perform only
//! the schnorr half, avoiding a second content/tag hash. Native targets keep a
//! bounded set of workers alive for the lifetime of the pool; each worker owns
//! one secp256k1 verification context and reuses it for every event. That avoids
//! per-burst thread creation and gives each worker a context-local verification
//! hot path. wasm32 has the same ordered API but verifies deterministically on
//! the calling thread.

use std::sync::Arc;

use nostr::Event;

use super::spawn::ThreadSpawner;
use super::{ThreadRole, ThreadSpawnError};
use crate::health::RelayHealth;

#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{self, Receiver, SyncSender};
#[cfg(not(target_arch = "wasm32"))]
use std::thread::JoinHandle;

#[cfg(target_arch = "wasm32")]
use nostr::secp256k1::{Secp256k1, VerifyOnly};

/// Default number of queued verification tasks per native worker.
///
/// The bounded queues apply backpressure to the translator instead of letting
/// a relay burst allocate an unbounded backlog. A queue belongs to one worker,
/// so no mutex is needed around task receipt or the worker's secp context.
/// Persistent, bounded signature-verification executor.
///
/// Results returned by [`VerifierPool::verify_batch`] always correspond to the
/// input order even though native workers may complete out of order. Dropping
/// the pool drains accepted work, asks every worker to stop, and joins every
/// thread.
pub(super) struct VerifierPool {
    #[cfg(not(target_arch = "wasm32"))]
    workers: Vec<Option<Worker>>,
    #[cfg(not(target_arch = "wasm32"))]
    next_worker: usize,
    #[cfg(not(target_arch = "wasm32"))]
    queue_capacity: usize,
    #[cfg(not(target_arch = "wasm32"))]
    spawner: Arc<dyn ThreadSpawner>,
    #[cfg(target_arch = "wasm32")]
    secp: Secp256k1<VerifyOnly>,
}

/// Fail-closed result for one verification task.
///
/// `Unavailable` is deliberately distinct from a bad signature: an internal
/// worker failure must drop the affected event and become visible as relay
/// health, but must not falsely accuse the relay of cryptographic misbehavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VerificationOutcome {
    Valid,
    Invalid,
    Unavailable,
}

#[cfg(not(target_arch = "wasm32"))]
struct Worker {
    tasks: SyncSender<Task>,
    join: Option<JoinHandle<()>>,
}

#[cfg(not(target_arch = "wasm32"))]
enum Task {
    Verify {
        index: usize,
        event: Arc<Event>,
        results: mpsc::Sender<(usize, bool)>,
    },
    Shutdown,
}

impl VerifierPool {
    /// Build a pool with explicit native worker and per-worker queue bounds.
    ///
    /// Both values are clamped to one. They are retained in the wasm signature
    /// so callers can construct the pool without target-specific application
    /// code; wasm still executes sequentially and does not create queues.
    pub(super) fn new(
        worker_count: usize,
        queue_capacity: usize,
        spawner: Arc<dyn ThreadSpawner>,
    ) -> Result<Self, ThreadSpawnError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let worker_count = worker_count.max(1);
            let queue_capacity = queue_capacity.max(1);
            let mut workers = Vec::with_capacity(worker_count);
            for index in 0..worker_count {
                match Worker::spawn(index, queue_capacity, spawner.as_ref()) {
                    Ok(worker) => workers.push(Some(worker)),
                    Err(error) => {
                        shutdown_workers(&mut workers);
                        return Err(error);
                    }
                }
            }
            Ok(Self {
                workers,
                next_worker: 0,
                queue_capacity,
                spawner,
            })
        }

        #[cfg(target_arch = "wasm32")]
        {
            let _ = (worker_count, queue_capacity, spawner);
            Ok(Self {
                secp: Secp256k1::verification_only(),
            })
        }
    }

    /// Verify a batch and return one validity bit per event, in input order.
    ///
    /// `Arc<Event>` lets the translator hand the exact parsed value to a
    /// worker and later reuse it without cloning its strings or tags.
    pub(super) fn verify_batch(&mut self, events: &[Arc<Event>]) -> Vec<VerificationOutcome> {
        #[cfg(feature = "bench-instrumentation")]
        let started = std::time::Instant::now();
        #[cfg(not(target_arch = "wasm32"))]
        {
            if events.is_empty() {
                return Vec::new();
            }

            let (results_tx, results_rx) = mpsc::channel();
            let first_worker = self.next_worker;
            self.next_worker = self.next_worker.wrapping_add(events.len());
            for (offset, event) in events.iter().enumerate() {
                let worker = first_worker.wrapping_add(offset) % self.workers.len();
                let task = Task::Verify {
                    index: offset,
                    event: Arc::clone(event),
                    results: results_tx.clone(),
                };
                let Some(lane) = self.workers[worker].as_ref() else {
                    drop(task);
                    self.try_replace_worker(worker);
                    continue;
                };
                if let Err(error) = lane.tasks.send(task) {
                    // Retire and replace the failed lane immediately. The
                    // affected task remains fail-closed for this batch, but a
                    // dead worker can never poison every Nth future event.
                    let mut failed = self.workers[worker].take().expect("lane checked above");
                    if let Some(join) = failed.join.take() {
                        let _ = join.join();
                    }
                    drop(error.0);
                    self.try_replace_worker(worker);
                }
            }
            drop(results_tx);

            // Start fail-closed. Successfully completed tasks overwrite their
            // slot; tasks rejected by a dead worker or abandoned by a worker
            // panic remain `Unavailable`. Iteration ends once every task-held
            // result sender has either replied or been dropped.
            let mut ordered = vec![VerificationOutcome::Unavailable; events.len()];
            for (index, valid) in results_rx {
                ordered[index] = if valid {
                    VerificationOutcome::Valid
                } else {
                    VerificationOutcome::Invalid
                };
            }
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::verify(started.elapsed(), events.len());
            ordered
        }

        #[cfg(target_arch = "wasm32")]
        {
            let outcomes = events
                .iter()
                .map(|event| {
                    if event.verify_signature_with_ctx(&self.secp) {
                        VerificationOutcome::Valid
                    } else {
                        VerificationOutcome::Invalid
                    }
                })
                .collect();
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::verify(started.elapsed(), events.len());
            outcomes
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn try_replace_worker(&mut self, index: usize) {
        if self.workers[index].is_some() {
            return;
        }
        if let Ok(worker) = Worker::spawn(index, self.queue_capacity, self.spawner.as_ref()) {
            self.workers[index] = Some(worker);
        }
    }

    #[cfg(all(test, not(target_arch = "wasm32")))]
    fn worker_count(&self) -> usize {
        self.workers.len()
    }

    #[cfg(all(test, not(target_arch = "wasm32")))]
    fn stop_worker(&mut self, index: usize) {
        let worker = self.workers[index].as_mut().expect("test worker exists");
        let _ = worker.tasks.send(Task::Shutdown);
        if let Some(join) = worker.join.take() {
            join.join().expect("test worker must stop cleanly");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Worker {
    fn spawn(
        index: usize,
        queue_capacity: usize,
        spawner: &dyn ThreadSpawner,
    ) -> Result<Self, ThreadSpawnError> {
        let (tasks_tx, tasks_rx) = mpsc::sync_channel(queue_capacity);
        let join = spawner
            .spawn(
                std::thread::Builder::new().name(format!("nmp-verify-{index}")),
                Box::new(move || worker_loop(tasks_rx)),
            )
            .map_err(|error| ThreadSpawnError {
                role: ThreadRole::VerifierWorker,
                reason: error.to_string(),
            })?;
        Ok(Self {
            tasks: tasks_tx,
            join: Some(join),
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn worker_loop(tasks: Receiver<Task>) {
    let secp = nostr::secp256k1::Secp256k1::verification_only();
    while let Ok(task) = tasks.recv() {
        match task {
            Task::Verify {
                index,
                event,
                results,
            } => {
                let valid = event.verify_signature_with_ctx(&secp);
                // Completion means every worker-owned reference is gone, so
                // the engine can structurally unwrap the frame Arc without a
                // race into the deep-clone fallback.
                drop(event);
                // A caller may abandon a batch while the pool is shutting
                // down; that must not kill an otherwise healthy worker.
                let _ = results.send((index, valid));
            }
            Task::Shutdown => break,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for VerifierPool {
    fn drop(&mut self) {
        shutdown_workers(&mut self.workers);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn shutdown_workers(workers: &mut [Option<Worker>]) {
    for worker in workers.iter().flatten() {
        // A disconnected worker has already stopped and will be joined
        // below. A full queue drains before this bounded send completes.
        let _ = worker.tasks.send(Task::Shutdown);
    }
    for worker in workers.iter_mut() {
        if let Some(join) = worker.as_mut().and_then(|worker| worker.join.take()) {
            // Drop must remain non-panicking even if a worker encountered
            // an unexpected panic while executing application work.
            let _ = join.join();
        }
    }
}

/// Bump the observable relay-misbehavior counter for a rejected event.
pub(super) fn record_misbehavior(health: &mut RelayHealth) {
    health.invalid_signature_count += 1;
}

/// Surface an internal verifier outage without attributing it to the relay.
pub(super) fn record_unavailable(health: &mut RelayHealth) {
    health.last_error = Some("signature verification worker unavailable".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::spawn::system_spawner;
    use nostr::{EventBuilder, JsonUtil, Keys, Kind, RelayMessage};

    fn signed_event(keys: &Keys, content: &str) -> Event {
        EventBuilder::new(Kind::TextNote, content)
            .sign_with_keys(keys)
            .expect("test fixture must sign cleanly")
    }

    #[test]
    fn batch_results_match_sequential_verification_and_input_order() {
        let keys = Keys::generate();
        let events: Vec<_> = (0..97)
            .map(|index| {
                let mut event = signed_event(&keys, &format!("event-{index}"));
                if index % 7 == 0 {
                    event.content.push_str("-tampered");
                } else if index % 11 == 0 {
                    event.sig = signed_event(&keys, &format!("other-{index}")).sig;
                }
                Arc::new(event)
            })
            .collect();
        let expected: Vec<_> = events
            .iter()
            .map(|event| {
                if event.verify_signature() {
                    VerificationOutcome::Valid
                } else {
                    VerificationOutcome::Invalid
                }
            })
            .collect();
        let mut pool = VerifierPool::new(4, 2, system_spawner()).unwrap();

        assert_eq!(pool.verify_batch(&events), expected);
    }

    #[test]
    fn persistent_pool_can_verify_multiple_bursts() {
        let keys = Keys::generate();
        let mut pool = VerifierPool::new(3, 1, system_spawner()).unwrap();

        for burst in 0..8 {
            let events: Vec<_> = (0..13)
                .map(|index| Arc::new(signed_event(&keys, &format!("{burst}-{index}"))))
                .collect();
            assert_eq!(
                pool.verify_batch(&events),
                vec![VerificationOutcome::Valid; events.len()]
            );
        }

        #[cfg(not(target_arch = "wasm32"))]
        assert_eq!(pool.worker_count(), 3);
    }

    #[test]
    fn empty_batch_is_empty() {
        let mut pool = VerifierPool::new(2, 1, system_spawner()).unwrap();
        assert!(pool.verify_batch(&[]).is_empty());
    }

    #[test]
    fn zero_configuration_is_clamped_and_drop_joins_workers() {
        let pool = VerifierPool::new(0, 0, system_spawner()).unwrap();
        #[cfg(not(target_arch = "wasm32"))]
        assert_eq!(pool.worker_count(), 1);
        drop(pool);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn stopped_worker_fails_affected_batch_closed_without_panicking() {
        let keys = Keys::generate();
        let events = vec![Arc::new(signed_event(&keys, "must not escape"))];
        let mut pool = VerifierPool::new(1, 1, system_spawner()).unwrap();
        pool.stop_worker(0);

        assert_eq!(
            pool.verify_batch(&events),
            vec![VerificationOutcome::Unavailable]
        );
        assert_eq!(
            pool.verify_batch(&events),
            vec![VerificationOutcome::Valid],
            "the stopped worker lane must be replaced for future batches"
        );
    }

    #[test]
    fn verifier_outage_is_health_not_false_relay_misbehavior() {
        let mut health = RelayHealth::default();
        record_unavailable(&mut health);

        assert_eq!(health.invalid_signature_count, 0);
        assert_eq!(
            health.last_error.as_deref(),
            Some("signature verification worker unavailable")
        );
    }

    /// Reproducible real-corpus proof for #168.
    ///
    /// `NMP_CORPUS` is JSONL with one canonical event object per line. The
    /// harness wraps each object in its real relay EVENT envelope without
    /// reparsing it during setup, then times exactly one typed relay-message
    /// parse per frame, persistent-worker first-seen verification, and the
    /// known-redelivery signature-compare path for the required burst matrix.
    #[test]
    #[ignore = "requires NMP_CORPUS real-event JSONL"]
    fn real_corpus_verify_matrix() {
        use std::collections::HashMap;
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        let path = std::env::var("NMP_CORPUS").expect("set NMP_CORPUS to event JSONL");
        let source = std::fs::read_to_string(&path).expect("read real corpus");
        let wire: Vec<_> = source
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|event_json| format!(r#"["EVENT","nmp-bench",{event_json}]"#))
            .collect();
        assert!(!wire.is_empty(), "real corpus is empty");

        fn median(mut samples: Vec<Duration>) -> Duration {
            samples.sort_unstable();
            samples[samples.len() / 2]
        }

        println!("corpus={path}");
        println!("corpus_events={}", wire.len());
        for requested in [1usize, 2, 8, 32, 128, 512, wire.len()] {
            let size = requested.min(wire.len());
            let mut parse_samples = Vec::new();
            let mut verify_samples = Vec::new();
            let mut known_samples = Vec::new();
            for _ in 0..3 {
                let started = Instant::now();
                let frames: Vec<_> = wire[..size]
                    .iter()
                    .map(|raw| {
                        let parsed: RelayMessage<'static> =
                            RelayMessage::from_json(raw).expect("parse real relay EVENT once");
                        crate::pool::RelayFrame::from(parsed)
                    })
                    .collect();
                let events: Vec<_> = frames
                    .iter()
                    .map(|frame| Arc::clone(frame.event().expect("fixture wrapper must be EVENT")))
                    .collect();
                parse_samples.push(started.elapsed());

                let mut pool =
                    VerifierPool::new(super::super::DEFAULT_VERIFIER_WORKERS, 64, system_spawner())
                        .expect("benchmark verifier construction");
                let started = Instant::now();
                assert!(events.iter().all(|event| event.verify_id()));
                let valid = pool.verify_batch(black_box(&events));
                verify_samples.push(started.elapsed());
                assert!(valid
                    .iter()
                    .all(|outcome| *outcome == VerificationOutcome::Valid));

                let known: HashMap<_, _> =
                    events.iter().map(|event| (event.id, event.sig)).collect();
                let started = Instant::now();
                let hits = events
                    .iter()
                    .filter(|event| event.verify_id() && known.get(&event.id) == Some(&event.sig))
                    .count();
                known_samples.push(started.elapsed());
                assert_eq!(hits, events.len());
            }
            println!("size={size}");
            println!("  parse_count={size}");
            println!(
                "  parse_once_median_ms={:.3}",
                median(parse_samples).as_secs_f64() * 1_000.0
            );
            println!(
                "  first_seen_verify_median_ms={:.3}",
                median(verify_samples).as_secs_f64() * 1_000.0
            );
            println!(
                "  known_redelivery_median_ms={:.3}",
                median(known_samples).as_secs_f64() * 1_000.0
            );
        }
    }
}
