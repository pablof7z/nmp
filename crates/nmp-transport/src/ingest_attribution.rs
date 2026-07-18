//! Opt-in transport ingest attribution for evidence binaries.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub committed_observation_lookups: u64,
    pub committed_observation_hits: u64,
    pub committed_observation_publications: u64,
    pub committed_observation_invalidations: u64,
    pub diagnostic_duplicate_ceiling_lookups: u64,
    pub diagnostic_duplicate_ceiling_hits: u64,
    pub diagnostic_duplicate_ceiling_inserts: u64,
    pub diagnostic_preparsed_ceiling_lookups: u64,
    pub diagnostic_preparsed_ceiling_hits: u64,
    pub parse_attempts: u64,
    pub parsed_frames: u64,
    pub parse_ns: u64,
    pub event_id_validation_attempts: u64,
    pub event_id_validation_skips: u64,
    pub event_id_validation_ns: u64,
    pub translator_bursts: u64,
    pub translator_events: u64,
    pub max_translator_burst: u64,
    pub verify_batches: u64,
    pub verify_candidates: u64,
    pub verify_ns: u64,
    pub verify_dispatch_ns: u64,
    pub verify_collect_ns: u64,
    pub verify_worker_ns: u64,
    pub signature_verification_attempts: u64,
    pub signature_verification_skips: u64,
    pub verify_task_submissions: u64,
    pub verify_result_messages: u64,
    pub verify_worker_candidates: u64,
    pub max_verify_lane_candidates: u64,
    pub delivered_events: u64,
    pub delivery_ns: u64,
    pub event_fallback_clones: u64,
}

macro_rules! counters { ($($name:ident),+ $(,)?) => { $(static $name: AtomicU64 = AtomicU64::new(0);)+ }; }
counters!(
    COMMITTED_OBSERVATION_LOOKUPS,
    COMMITTED_OBSERVATION_HITS,
    COMMITTED_OBSERVATION_PUBLICATIONS,
    COMMITTED_OBSERVATION_INVALIDATIONS,
    DIAGNOSTIC_DUPLICATE_CEILING_LOOKUPS,
    DIAGNOSTIC_DUPLICATE_CEILING_HITS,
    DIAGNOSTIC_DUPLICATE_CEILING_INSERTS,
    DIAGNOSTIC_PREPARSED_CEILING_LOOKUPS,
    DIAGNOSTIC_PREPARSED_CEILING_HITS,
    PARSE_ATTEMPTS,
    PARSED_FRAMES,
    PARSE_NS,
    EVENT_ID_VALIDATION_ATTEMPTS,
    EVENT_ID_VALIDATION_SKIPS,
    EVENT_ID_VALIDATION_NS,
    TRANSLATOR_BURSTS,
    TRANSLATOR_EVENTS,
    MAX_TRANSLATOR_BURST,
    VERIFY_BATCHES,
    VERIFY_CANDIDATES,
    VERIFY_NS,
    VERIFY_DISPATCH_NS,
    VERIFY_COLLECT_NS,
    VERIFY_WORKER_NS,
    SIGNATURE_VERIFICATION_ATTEMPTS,
    SIGNATURE_VERIFICATION_SKIPS,
    VERIFY_TASK_SUBMISSIONS,
    VERIFY_RESULT_MESSAGES,
    VERIFY_WORKER_CANDIDATES,
    MAX_VERIFY_LANE_CANDIDATES,
    DELIVERED_EVENTS,
    DELIVERY_NS,
    EVENT_FALLBACK_CLONES
);

static SKIP_EVENT_ID_VALIDATION: AtomicBool = AtomicBool::new(false);
static SKIP_SIGNATURE_VERIFICATION: AtomicBool = AtomicBool::new(false);

fn ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}
fn add(counter: &AtomicU64, duration: Duration) {
    counter.fetch_add(ns(duration), Ordering::Relaxed);
}

pub fn reset() {
    for counter in [
        &COMMITTED_OBSERVATION_LOOKUPS,
        &COMMITTED_OBSERVATION_HITS,
        &COMMITTED_OBSERVATION_PUBLICATIONS,
        &COMMITTED_OBSERVATION_INVALIDATIONS,
        &DIAGNOSTIC_DUPLICATE_CEILING_LOOKUPS,
        &DIAGNOSTIC_DUPLICATE_CEILING_HITS,
        &DIAGNOSTIC_DUPLICATE_CEILING_INSERTS,
        &DIAGNOSTIC_PREPARSED_CEILING_LOOKUPS,
        &DIAGNOSTIC_PREPARSED_CEILING_HITS,
        &PARSE_ATTEMPTS,
        &PARSED_FRAMES,
        &PARSE_NS,
        &EVENT_ID_VALIDATION_ATTEMPTS,
        &EVENT_ID_VALIDATION_SKIPS,
        &EVENT_ID_VALIDATION_NS,
        &TRANSLATOR_BURSTS,
        &TRANSLATOR_EVENTS,
        &MAX_TRANSLATOR_BURST,
        &VERIFY_BATCHES,
        &VERIFY_CANDIDATES,
        &VERIFY_NS,
        &VERIFY_DISPATCH_NS,
        &VERIFY_COLLECT_NS,
        &VERIFY_WORKER_NS,
        &SIGNATURE_VERIFICATION_ATTEMPTS,
        &SIGNATURE_VERIFICATION_SKIPS,
        &VERIFY_TASK_SUBMISSIONS,
        &VERIFY_RESULT_MESSAGES,
        &VERIFY_WORKER_CANDIDATES,
        &MAX_VERIFY_LANE_CANDIDATES,
        &DELIVERED_EVENTS,
        &DELIVERY_NS,
        &EVENT_FALLBACK_CLONES,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

pub fn snapshot() -> Snapshot {
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    Snapshot {
        committed_observation_lookups: load(&COMMITTED_OBSERVATION_LOOKUPS),
        committed_observation_hits: load(&COMMITTED_OBSERVATION_HITS),
        committed_observation_publications: load(&COMMITTED_OBSERVATION_PUBLICATIONS),
        committed_observation_invalidations: load(&COMMITTED_OBSERVATION_INVALIDATIONS),
        diagnostic_duplicate_ceiling_lookups: load(&DIAGNOSTIC_DUPLICATE_CEILING_LOOKUPS),
        diagnostic_duplicate_ceiling_hits: load(&DIAGNOSTIC_DUPLICATE_CEILING_HITS),
        diagnostic_duplicate_ceiling_inserts: load(&DIAGNOSTIC_DUPLICATE_CEILING_INSERTS),
        diagnostic_preparsed_ceiling_lookups: load(&DIAGNOSTIC_PREPARSED_CEILING_LOOKUPS),
        diagnostic_preparsed_ceiling_hits: load(&DIAGNOSTIC_PREPARSED_CEILING_HITS),
        parse_attempts: load(&PARSE_ATTEMPTS),
        parsed_frames: load(&PARSED_FRAMES),
        parse_ns: load(&PARSE_NS),
        event_id_validation_attempts: load(&EVENT_ID_VALIDATION_ATTEMPTS),
        event_id_validation_skips: load(&EVENT_ID_VALIDATION_SKIPS),
        event_id_validation_ns: load(&EVENT_ID_VALIDATION_NS),
        translator_bursts: load(&TRANSLATOR_BURSTS),
        translator_events: load(&TRANSLATOR_EVENTS),
        max_translator_burst: load(&MAX_TRANSLATOR_BURST),
        verify_batches: load(&VERIFY_BATCHES),
        verify_candidates: load(&VERIFY_CANDIDATES),
        verify_ns: load(&VERIFY_NS),
        verify_dispatch_ns: load(&VERIFY_DISPATCH_NS),
        verify_collect_ns: load(&VERIFY_COLLECT_NS),
        verify_worker_ns: load(&VERIFY_WORKER_NS),
        signature_verification_attempts: load(&SIGNATURE_VERIFICATION_ATTEMPTS),
        signature_verification_skips: load(&SIGNATURE_VERIFICATION_SKIPS),
        verify_task_submissions: load(&VERIFY_TASK_SUBMISSIONS),
        verify_result_messages: load(&VERIFY_RESULT_MESSAGES),
        verify_worker_candidates: load(&VERIFY_WORKER_CANDIDATES),
        max_verify_lane_candidates: load(&MAX_VERIFY_LANE_CANDIDATES),
        delivered_events: load(&DELIVERED_EVENTS),
        delivery_ns: load(&DELIVERY_NS),
        event_fallback_clones: load(&EVENT_FALLBACK_CLONES),
    }
}

/// Configure unsafe validation bypasses for a benchmark-only favorable ceiling.
///
/// These switches exist only behind `bench-instrumentation`; ordinary builds
/// cannot construct a transport that skips either Nostr validation step.
pub fn configure_validation_ceiling(skip_event_id: bool, skip_signature: bool) {
    SKIP_EVENT_ID_VALIDATION.store(skip_event_id, Ordering::Release);
    SKIP_SIGNATURE_VERIFICATION.store(skip_signature, Ordering::Release);
}

pub(crate) fn skip_event_id_validation() -> bool {
    SKIP_EVENT_ID_VALIDATION.load(Ordering::Acquire)
}

pub(crate) fn skip_signature_verification() -> bool {
    SKIP_SIGNATURE_VERIFICATION.load(Ordering::Acquire)
}

pub(crate) fn committed_observation_lookup(hit: bool) {
    COMMITTED_OBSERVATION_LOOKUPS.fetch_add(1, Ordering::Relaxed);
    COMMITTED_OBSERVATION_HITS.fetch_add(hit as u64, Ordering::Relaxed);
}

pub(crate) fn committed_observation_update(publications: u64, invalidations: u64) {
    COMMITTED_OBSERVATION_PUBLICATIONS.fetch_add(publications, Ordering::Relaxed);
    COMMITTED_OBSERVATION_INVALIDATIONS.fetch_add(invalidations, Ordering::Relaxed);
}

pub(crate) fn diagnostic_duplicate_ceiling_lookup(hit: bool) {
    DIAGNOSTIC_DUPLICATE_CEILING_LOOKUPS.fetch_add(1, Ordering::Relaxed);
    DIAGNOSTIC_DUPLICATE_CEILING_HITS.fetch_add(hit as u64, Ordering::Relaxed);
}

pub(crate) fn diagnostic_duplicate_ceiling_insert() {
    DIAGNOSTIC_DUPLICATE_CEILING_INSERTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn diagnostic_preparsed_ceiling_lookup(hit: bool) {
    DIAGNOSTIC_PREPARSED_CEILING_LOOKUPS.fetch_add(1, Ordering::Relaxed);
    DIAGNOSTIC_PREPARSED_CEILING_HITS.fetch_add(hit as u64, Ordering::Relaxed);
}

pub(crate) fn parse(duration: Duration, parsed: bool) {
    PARSE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    PARSED_FRAMES.fetch_add(parsed as u64, Ordering::Relaxed);
    add(&PARSE_NS, duration);
}
pub(crate) fn event_id_validation(duration: Duration, skipped: bool) {
    EVENT_ID_VALIDATION_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    EVENT_ID_VALIDATION_SKIPS.fetch_add(skipped as u64, Ordering::Relaxed);
    add(&EVENT_ID_VALIDATION_NS, duration);
}
pub(crate) fn translator_burst(events: usize) {
    TRANSLATOR_BURSTS.fetch_add(1, Ordering::Relaxed);
    TRANSLATOR_EVENTS.fetch_add(events as u64, Ordering::Relaxed);
    MAX_TRANSLATOR_BURST.fetch_max(events as u64, Ordering::Relaxed);
}
pub(crate) fn verify(duration: Duration, candidates: usize) {
    VERIFY_BATCHES.fetch_add(1, Ordering::Relaxed);
    VERIFY_CANDIDATES.fetch_add(candidates as u64, Ordering::Relaxed);
    add(&VERIFY_NS, duration);
}
pub(crate) fn verify_dispatch(duration: Duration, tasks: usize) {
    add(&VERIFY_DISPATCH_NS, duration);
    VERIFY_TASK_SUBMISSIONS.fetch_add(tasks as u64, Ordering::Relaxed);
}
pub(crate) fn verify_collect(duration: Duration, messages: usize) {
    add(&VERIFY_COLLECT_NS, duration);
    VERIFY_RESULT_MESSAGES.fetch_add(messages as u64, Ordering::Relaxed);
}
pub(crate) fn verify_worker(duration: Duration, candidates: usize) {
    add(&VERIFY_WORKER_NS, duration);
    VERIFY_WORKER_CANDIDATES.fetch_add(candidates as u64, Ordering::Relaxed);
    MAX_VERIFY_LANE_CANDIDATES.fetch_max(candidates as u64, Ordering::Relaxed);
}
pub(crate) fn signature_verification(skipped: bool) {
    SIGNATURE_VERIFICATION_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    SIGNATURE_VERIFICATION_SKIPS.fetch_add(skipped as u64, Ordering::Relaxed);
}
pub(crate) fn delivery(duration: Duration) {
    DELIVERED_EVENTS.fetch_add(1, Ordering::Relaxed);
    add(&DELIVERY_NS, duration);
}

pub(crate) fn event_fallback_clone() {
    EVENT_FALLBACK_CLONES.fetch_add(1, Ordering::Relaxed);
}
