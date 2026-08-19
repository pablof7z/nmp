//! Persistent ingest-time signature verification workers + durable dedup.
//!
//! See the crate-level doc for the trust-gate placement rationale. This
//! module owns:
//!
//! - the in-memory `VerifiedEventCache` LRU of verified `(id, sig)` pairs;
//! - the durable dedup-by-id through [`KnownSig`] (a known id is a
//!   signature byte-compare against the stored known-good signature — no
//!   schnorr);
//! - the candidate-by-pair dedup within one burst (identical unknown
//!   `(id, sig)` pairs share ONE schnorr check, but every input still gets
//!   its own verdict);
//! - the persistent native verifier workers (one secp256k1 context each,
//!   bounded queues, worker-replacement-on-death, fail-closed
//!   [`Verdict::RejectUnavailable`], `Drop` join) and the wasm32 sequential
//!   path.
//!
//! [`Verifier::schnorr_verifications`] is the falsifier for the
//! durable-dedup invariant: durable and LRU hits never reach a worker, so a
//! cold-start replay of already-ingested ids performs zero schnorr checks.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nostr::secp256k1::schnorr::Signature;
use nostr::Event;
use nostr::EventId;

use super::spawn::{system_spawner, ThreadSpawner};
use super::{ThreadRole, ThreadSpawnError};

#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{self, Receiver, SyncSender};
#[cfg(not(target_arch = "wasm32"))]
use std::thread::JoinHandle;

#[cfg(target_arch = "wasm32")]
use nostr::secp256k1::{Secp256k1, VerifyOnly};

/// Small fixed verifier set owned by one engine. Signature verification is
/// CPU-bound and fed through bounded queues; copying host parallelism into
/// every engine multiplied OS threads without imposing a process budget.
pub const DEFAULT_VERIFIER_WORKERS: usize = 2;

/// Hard ceiling for an explicitly configured per-engine verifier pool.
/// The default remains deliberately small; embedders opting into a wider
/// pool still cannot create an unbounded number of OS threads.
pub const MAX_VERIFIER_WORKERS: usize = 16;

/// Upper bound for the host-aware verifier width selected by
/// [`VerifyConfig::default`]. Explicit configurations may still request up
/// to [`MAX_VERIFIER_WORKERS`].
pub const MAX_DEFAULT_VERIFIER_WORKERS: usize = 8;

/// Durable dedup-by-id seam. Returns the known-good signature for an
/// already-ingested event id, if any. Wired at the engine with a
/// store-backed impl; [`NullKnownSig`] always returns `None`.
///
/// `nmp-transport` does NOT depend on `nmp-store`: this trait is the one
/// closing layer that lets the trust gate read durable identity without
/// pulling the store into the bottom wire layer.
pub trait KnownSig: Send + Sync {
    fn known_signature(&self, id: &EventId) -> Option<Signature>;
}

/// Test/default impl: no durable knowledge (every id is a candidate).
pub struct NullKnownSig;
impl KnownSig for NullKnownSig {
    fn known_signature(&self, _id: &EventId) -> Option<Signature> {
        None
    }
}

/// The trust decision for one already-parsed, id-valid, non-stale event.
///
/// `RejectUnavailable` is deliberately distinct from `RejectMisbehavior`: an
/// internal verifier-worker failure must drop the affected event and surface
/// as relay health, but must not falsely accuse the relay of cryptographic
/// misbehavior. Transport maps `RejectMisbehavior`/`RejectUnavailable` onto
/// `RelayHealth` accounting; `Skip` records nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    /// The relay sent an event whose id we already know, carrying a
    /// DIFFERENT signature. Drop the frame; accuse nobody.
    ///
    /// One event id has arbitrarily many valid signatures — NIP-01's id
    /// preimage is
    /// `[0, pubkey, created_at, kind, tags, content]`, so `sig` is not
    /// covered, and `nostr` signs with `OsRng` auxiliary randomness. A
    /// mismatch is therefore evidence of nothing about the relay, and this
    /// used to be `RejectMisbehavior` on exactly that false premise.
    ///
    /// The consequence, stated honestly because it is not free: a skipped
    /// frame never becomes a `PoolEvent`, so **a live query can silently
    /// lose the event**. On the durable branch that is nearly harmless — the
    /// id is known because the row is resident, so nothing is lost but the
    /// provenance merge. On the LRU branch it is not: the cache outlives
    /// residency, so an id refused, tombstoned, superseded or GC'd since it
    /// was cached still reads as known, and a redelivery carrying a second
    /// valid signature is dropped rather than re-admitted. Tracked in #1862.
    Skip,
    RejectMisbehavior,
    RejectUnavailable,
}

#[derive(Debug, Clone, Copy)]
pub struct VerifyConfig {
    pub workers: usize,
    pub queue_capacity: usize,
    pub lru_capacity: usize,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        let workers = std::thread::available_parallelism()
            .map_or(DEFAULT_VERIFIER_WORKERS, usize::from)
            .div_ceil(2)
            .clamp(DEFAULT_VERIFIER_WORKERS, MAX_DEFAULT_VERIFIER_WORKERS);
        Self {
            workers,
            queue_capacity: 64,
            lru_capacity: 131_072,
        }
    }
}

/// Fail-closed internal result of one worker schnorr check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationOutcome {
    Valid,
    Invalid,
    Unavailable,
}

/// In-memory LRU of verified `(id, sig)` pairs. A hit is a signature
/// byte-compare (no schnorr, no durable read). Eviction only causes later
/// re-verification; it never changes policy.
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

/// The public trust-gate handle. Owns the persistent verifier workers, the
/// verified `(id, sig)` LRU, the durable [`KnownSig`] seam, and the
/// schnorr-call falsifier counter.
pub struct Verifier {
    pool: VerifierPool,
    cache: VerifiedEventCache,
    known_sig: Arc<dyn KnownSig>,
    schnorr_calls: Arc<AtomicU64>,
}

impl Verifier {
    /// Build the verifier: persistent native workers (or the wasm32
    /// sequential path), an empty LRU, and the durable seam.
    pub fn new(
        config: VerifyConfig,
        known_sig: Arc<dyn KnownSig>,
    ) -> Result<Self, ThreadSpawnError> {
        Self::new_with_spawner(config, known_sig, system_spawner())
    }

    pub(super) fn new_with_spawner(
        config: VerifyConfig,
        known_sig: Arc<dyn KnownSig>,
        spawner: Arc<dyn ThreadSpawner>,
    ) -> Result<Self, ThreadSpawnError> {
        let schnorr_calls = Arc::new(AtomicU64::new(0));
        let pool = VerifierPool::new(
            config.workers,
            config.queue_capacity,
            spawner,
            Arc::clone(&schnorr_calls),
        )?;
        Ok(Self {
            pool,
            cache: VerifiedEventCache::new(config.lru_capacity),
            known_sig,
            schnorr_calls,
        })
    }

    /// Verify a batch of already-parsed, id-valid, non-stale events. Returns
    /// one [`Verdict`] per input, in order.
    ///
    /// Per event:
    /// 1. LRU hit → byte-compare stored sig vs `event.sig` (no schnorr, no
    ///    durable read): `Accept` if equal else [`Verdict::Skip`].
    /// 2. Else durable `KnownSig` hit → byte-compare (no schnorr): `Accept`
    ///    (also inserted into the LRU) or [`Verdict::Skip`].
    /// 3. Else candidate → submit to the worker pool for schnorr. Identical
    ///    unknown `(id, sig)` pairs share ONE schnorr check within the burst,
    ///    but every input still gets its own verdict.
    pub fn verify_batch(&mut self, events: &[Arc<Event>]) -> Vec<Verdict> {
        if events.is_empty() {
            return Vec::new();
        }

        // Resolve every LRU / durable hit inline, and collect the unique
        // unknown (id, sig) pairs that actually need a worker schnorr check.
        let mut verdicts: Vec<Option<Verdict>> = vec![None; events.len()];
        let mut candidates: Vec<Arc<Event>> = Vec::new();
        let mut candidate_by_pair: HashMap<(EventId, Signature), usize> = HashMap::new();
        // Input positions awaiting their candidate verdict, paired with the
        // unique candidate index their (id, sig) pair resolved to.
        let mut pending: Vec<(usize, usize)> = Vec::new();
        for (index, event) in events.iter().enumerate() {
            // Equal means this relay sent a good event. Unequal means only
            // that it signed the same body with different aux randomness (or
            // that a second, equally valid signature of the same body is in
            // circulation) — skip it, do not accuse. See `Verdict::Skip`.
            if let Some(known) = self.cache.get(&event.id) {
                verdicts[index] = Some(if known == event.sig {
                    Verdict::Accept
                } else {
                    Verdict::Skip
                });
                continue;
            }
            if let Some(stored) = self.known_sig.known_signature(&event.id) {
                if stored == event.sig {
                    self.cache.insert(event.id, event.sig);
                    verdicts[index] = Some(Verdict::Accept);
                } else {
                    verdicts[index] = Some(Verdict::Skip);
                }
                continue;
            }
            // Candidate: dedup identical unknown (id, sig) pairs to one
            // schnorr check, but every position keeps its own verdict.
            let pair = (event.id, event.sig);
            let candidate = *candidate_by_pair.entry(pair).or_insert_with(|| {
                let idx = candidates.len();
                candidates.push(Arc::clone(event));
                idx
            });
            pending.push((index, candidate));
        }

        let candidate_results = self.pool.verify_batch(&candidates);
        for (index, candidate) in pending {
            let outcome = candidate_results[candidate];
            let event = &events[index];
            verdicts[index] = Some(resolve_candidate(&mut self.cache, event, outcome));
        }

        verdicts
            .into_iter()
            .map(|verdict| verdict.expect("every position is resolved"))
            .collect()
    }

    /// Number of schnorr verifications actually performed (falsifier for the
    /// durable-dedup invariant). Always present, not feature-gated. Durable
    /// and LRU hits never reach a worker and never increment this.
    pub fn schnorr_verifications(&self) -> u64 {
        self.schnorr_calls.load(Ordering::Relaxed)
    }

}

fn resolve_candidate(
    cache: &mut VerifiedEventCache,
    event: &Arc<Event>,
    cryptographically_valid: VerificationOutcome,
) -> Verdict {
    match (cache.get(&event.id), cryptographically_valid) {
        (Some(known), VerificationOutcome::Valid) if known == event.sig => Verdict::Accept,
        // The cache filled while this candidate was in flight and now holds a
        // different signature for the same id. A schnorr-VALID signature that
        // merely differs is not misbehavior (see `Verdict::Skip`); a schnorr-
        // INVALID one is, and stays an accusation.
        (Some(_), VerificationOutcome::Valid) => Verdict::Skip,
        (Some(_), VerificationOutcome::Invalid) => Verdict::RejectMisbehavior,
        (Some(_), VerificationOutcome::Unavailable) => Verdict::RejectUnavailable,
        (None, VerificationOutcome::Valid) => {
            cache.insert(event.id, event.sig);
            Verdict::Accept
        }
        (None, VerificationOutcome::Invalid) => Verdict::RejectMisbehavior,
        (None, VerificationOutcome::Unavailable) => Verdict::RejectUnavailable,
    }
}

// ---------------------------------------------------------------------------
// Persistent verifier worker pool (native threads + wasm32 sequential path).
// ---------------------------------------------------------------------------

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

struct VerifierPool {
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
    schnorr_calls: Arc<AtomicU64>,
}

impl VerifierPool {
    fn new(
        worker_count: usize,
        queue_capacity: usize,
        spawner: Arc<dyn ThreadSpawner>,
        schnorr_calls: Arc<AtomicU64>,
    ) -> Result<Self, ThreadSpawnError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let worker_count = configured_workers(worker_count);
            let queue_capacity = queue_capacity.max(1);
            let mut workers = Vec::with_capacity(worker_count);
            for index in 0..worker_count {
                match Worker::spawn(
                    index,
                    queue_capacity,
                    spawner.as_ref(),
                    Arc::clone(&schnorr_calls),
                ) {
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
                schnorr_calls,
            })
        }

        #[cfg(target_arch = "wasm32")]
        {
            let _ = (worker_count, queue_capacity, spawner);
            Ok(Self {
                secp: Secp256k1::verification_only(),
                schnorr_calls,
            })
        }
    }

    fn verify_batch(&mut self, events: &[Arc<Event>]) -> Vec<VerificationOutcome> {
        #[cfg(feature = "bench-instrumentation")]
        let started = std::time::Instant::now();
        #[cfg(not(target_arch = "wasm32"))]
        {
            if events.is_empty() {
                return Vec::new();
            }

            #[cfg(feature = "bench-instrumentation")]
            let dispatch_started = std::time::Instant::now();
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
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::verify_dispatch(dispatch_started.elapsed(), events.len());

            // Start fail-closed. Successfully completed tasks overwrite their
            // slot; tasks rejected by a dead worker or abandoned by a worker
            // panic remain `Unavailable`.
            #[cfg(feature = "bench-instrumentation")]
            let collect_started = std::time::Instant::now();
            let mut ordered = vec![VerificationOutcome::Unavailable; events.len()];
            #[cfg(feature = "bench-instrumentation")]
            let mut result_messages = 0usize;
            for (index, valid) in results_rx {
                #[cfg(feature = "bench-instrumentation")]
                {
                    result_messages = result_messages.saturating_add(1);
                }
                ordered[index] = if valid {
                    VerificationOutcome::Valid
                } else {
                    VerificationOutcome::Invalid
                };
            }
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::verify_collect(collect_started.elapsed(), result_messages);
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::verify(started.elapsed(), events.len());
            ordered
        }

        #[cfg(target_arch = "wasm32")]
        {
            let outcomes = events
                .iter()
                .map(|event| {
                    let valid = event.verify_signature_with_ctx(&self.secp);
                    // Count once per actual schnorr check on the wasm path.
                    self.schnorr_calls.fetch_add(1, Ordering::Relaxed);
                    if valid {
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
        if let Ok(worker) = Worker::spawn(
            index,
            self.queue_capacity,
            self.spawner.as_ref(),
            Arc::clone(&self.schnorr_calls),
        ) {
            self.workers[index] = Some(worker);
        }
    }
}

fn configured_workers(configured: usize) -> usize {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if configured == 0 {
            DEFAULT_VERIFIER_WORKERS
        } else {
            configured.min(MAX_VERIFIER_WORKERS)
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = configured;
        1
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Worker {
    fn spawn(
        index: usize,
        queue_capacity: usize,
        spawner: &dyn ThreadSpawner,
        schnorr_calls: Arc<AtomicU64>,
    ) -> Result<Self, ThreadSpawnError> {
        let (tasks_tx, tasks_rx) = mpsc::sync_channel(queue_capacity);
        let join = spawner
            .spawn(
                std::thread::Builder::new().name(format!("nmp-verify-{index}")),
                Box::new(move || worker_loop(tasks_rx, schnorr_calls)),
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
fn worker_loop(tasks: Receiver<Task>, schnorr_calls: Arc<AtomicU64>) {
    let secp = nostr::secp256k1::Secp256k1::verification_only();
    while let Ok(task) = tasks.recv() {
        match task {
            Task::Verify {
                index,
                event,
                results,
            } => {
                #[cfg(feature = "bench-instrumentation")]
                let verify_started = std::time::Instant::now();
                #[cfg(feature = "bench-instrumentation")]
                let skip_signature = crate::ingest_attribution::skip_signature_verification();
                #[cfg(not(feature = "bench-instrumentation"))]
                let skip_signature = false;
                let valid = skip_signature || event.verify_signature_with_ctx(&secp);
                // Falsifier for the durable-dedup invariant: count once per
                // actual worker schnorr call. Durable/LRU hits never reach a
                // worker, so a cold-start replay of known ids stays zero.
                schnorr_calls.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "bench-instrumentation")]
                {
                    crate::ingest_attribution::verify_worker(verify_started.elapsed(), 1);
                    crate::ingest_attribution::signature_verification(skip_signature);
                }
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
        #[cfg(not(target_arch = "wasm32"))]
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

