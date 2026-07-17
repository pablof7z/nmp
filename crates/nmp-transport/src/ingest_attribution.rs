//! Opt-in transport ingest attribution for evidence binaries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub parse_attempts: u64,
    pub parsed_frames: u64,
    pub parse_ns: u64,
    pub translator_bursts: u64,
    pub translator_events: u64,
    pub max_translator_burst: u64,
    pub verify_batches: u64,
    pub verify_candidates: u64,
    pub verify_ns: u64,
    pub delivered_events: u64,
    pub delivery_ns: u64,
}

macro_rules! counters { ($($name:ident),+ $(,)?) => { $(static $name: AtomicU64 = AtomicU64::new(0);)+ }; }
counters!(
    PARSE_ATTEMPTS,
    PARSED_FRAMES,
    PARSE_NS,
    TRANSLATOR_BURSTS,
    TRANSLATOR_EVENTS,
    MAX_TRANSLATOR_BURST,
    VERIFY_BATCHES,
    VERIFY_CANDIDATES,
    VERIFY_NS,
    DELIVERED_EVENTS,
    DELIVERY_NS
);

fn ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}
fn add(counter: &AtomicU64, duration: Duration) {
    counter.fetch_add(ns(duration), Ordering::Relaxed);
}

pub fn reset() {
    for counter in [
        &PARSE_ATTEMPTS,
        &PARSED_FRAMES,
        &PARSE_NS,
        &TRANSLATOR_BURSTS,
        &TRANSLATOR_EVENTS,
        &MAX_TRANSLATOR_BURST,
        &VERIFY_BATCHES,
        &VERIFY_CANDIDATES,
        &VERIFY_NS,
        &DELIVERED_EVENTS,
        &DELIVERY_NS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

pub fn snapshot() -> Snapshot {
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    Snapshot {
        parse_attempts: load(&PARSE_ATTEMPTS),
        parsed_frames: load(&PARSED_FRAMES),
        parse_ns: load(&PARSE_NS),
        translator_bursts: load(&TRANSLATOR_BURSTS),
        translator_events: load(&TRANSLATOR_EVENTS),
        max_translator_burst: load(&MAX_TRANSLATOR_BURST),
        verify_batches: load(&VERIFY_BATCHES),
        verify_candidates: load(&VERIFY_CANDIDATES),
        verify_ns: load(&VERIFY_NS),
        delivered_events: load(&DELIVERED_EVENTS),
        delivery_ns: load(&DELIVERY_NS),
    }
}

pub(crate) fn parse(duration: Duration, parsed: bool) {
    PARSE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    PARSED_FRAMES.fetch_add(parsed as u64, Ordering::Relaxed);
    add(&PARSE_NS, duration);
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
pub(crate) fn delivery(duration: Duration) {
    DELIVERED_EVENTS.fetch_add(1, Ordering::Relaxed);
    add(&DELIVERY_NS, duration);
}
