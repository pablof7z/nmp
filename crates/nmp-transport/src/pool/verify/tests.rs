//! Unit tests for the verifier, including the durable-dedup falsifiers.
//!
//! The first four tests are the TDD red→green falsifiers for the
//! durable-dedup invariant (#1677): a cold-start replay of already-ingested
//! ids performs zero schnorr checks, unknown ids perform schnorr, a known id
//! carrying a different signature is skipped without schnorr, and an LRU hit
//! skips both the durable read and schnorr.

use super::*;
use nostr::{EventBuilder, Keys, Kind, UnsignedEvent};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn signed_event(keys: &Keys, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .sign_with_keys(keys)
        .expect("test fixture must sign cleanly")
}

/// `KnownSig` backed by an in-memory map — stands in for the store-backed
/// impl the engine wires in production.
struct MapKnownSig {
    known: HashMap<EventId, Signature>,
}

impl KnownSig for MapKnownSig {
    fn known_signature(&self, id: &EventId) -> Option<Signature> {
        self.known.get(id).copied()
    }
}

/// `KnownSig` that counts how many times it was consulted, returning `None`
/// each time (so every id is a candidate unless the LRU answers first).
struct CountingKnownSig {
    calls: Arc<AtomicU64>,
}

impl CountingKnownSig {
    fn new() -> (Self, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        (
            Self {
                calls: Arc::clone(&calls),
            },
            calls,
        )
    }
}

impl KnownSig for CountingKnownSig {
    fn known_signature(&self, _id: &EventId) -> Option<Signature> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        None
    }
}

fn default_verifier(known_sig: Arc<dyn KnownSig>) -> Verifier {
    Verifier::new(VerifyConfig::default(), known_sig)
        .expect("test verifier construction must succeed")
}

// ---- durable-dedup falsifiers (#1677) -----------------------------

#[test]
fn cold_start_replay_of_known_ids_performs_zero_schnorr() {
    let keys = Keys::generate();
    let events: Vec<Arc<Event>> = (0..32)
        .map(|i| Arc::new(signed_event(&keys, &format!("cold-{i}"))))
        .collect();
    let known: HashMap<_, _> = events.iter().map(|e| (e.id, e.sig)).collect();
    let known_sig: Arc<dyn KnownSig> = Arc::new(MapKnownSig { known });
    let mut verifier = default_verifier(known_sig);

    let verdicts = verifier.verify_batch(&events);

    assert!(
        verdicts.iter().all(|v| *v == Verdict::Accept),
        "every known id must accept: {verdicts:?}"
    );
    assert_eq!(
        verifier.schnorr_verifications(),
        0,
        "a cold-start replay of already-ingested ids must perform zero schnorr checks"
    );
}

#[test]
fn unknown_ids_perform_schnorr() {
    let keys = Keys::generate();
    let events: Vec<Arc<Event>> = (0..16)
        .map(|i| Arc::new(signed_event(&keys, &format!("unknown-{i}"))))
        .collect();
    let n = events.len();
    let mut verifier = default_verifier(Arc::new(NullKnownSig));

    let verdicts = verifier.verify_batch(&events);

    assert!(
        verdicts.iter().all(|v| *v == Verdict::Accept),
        "every genuinely-new signed event must accept: {verdicts:?}"
    );
    assert_eq!(
        verifier.schnorr_verifications(),
        n as u64,
        "unknown ids must each perform one schnorr check (no durable/LRU hits)"
    );
}

/// A known id whose signature does not byte-match is SKIPPED, not accused.
///
/// One event id admits many valid signatures by design: NIP-01's id preimage
/// is `[0, pubkey, created_at, kind, tags, content]`, so `sig` is not covered,
/// and `nostr` signs with `OsRng` auxiliary randomness — the same author
/// signing the same body twice produces two different, equally valid
/// signatures. A mismatch is therefore evidence of nothing about the relay.
/// Owner ruling (2026-08-17): equal means that relay sent a good event, not
/// equal means skip it.
///
/// Both shapes a mismatch can take are covered, because a known id never runs
/// schnorr and so the gate cannot — and need not — tell them apart: the event
/// is already durable either way.
#[test]
fn known_id_with_a_different_signature_is_skipped_without_schnorr() {
    let keys = Keys::generate();
    let genuine = signed_event(&keys, "genuine");

    // (a) The legitimate shape: the SAME body signed a second time. Different
    // aux randomness, different 64 bytes, same id, and perfectly valid.
    let resigned = UnsignedEvent::new(
        genuine.pubkey,
        genuine.created_at,
        genuine.kind,
        genuine.tags.clone(),
        genuine.content.clone(),
    )
    .sign_with_keys(&keys)
    .expect("fixture keys sign cleanly");
    assert_eq!(
        resigned.id, genuine.id,
        "NOTHING TO OBSERVE -- re-signing the same body must reproduce the id, \
         or this fixture is not the case under test"
    );
    assert_ne!(
        resigned.sig, genuine.sig,
        "NOTHING TO OBSERVE -- `nostr` must sign with fresh aux randomness, or \
         there is no mismatch to skip"
    );
    assert!(
        resigned.verify().is_ok(),
        "NOTHING TO OBSERVE -- the second signature must itself be valid"
    );

    // (b) A signature lifted from a different event entirely.
    let other = signed_event(&keys, "other-signature-source");
    let mut transplanted = genuine.clone();
    transplanted.sig = other.sig;

    let known: HashMap<_, _> = [(genuine.id, genuine.sig)].into_iter().collect();
    let known_sig: Arc<dyn KnownSig> = Arc::new(MapKnownSig { known });
    let mut verifier = default_verifier(known_sig);

    let verdicts = verifier.verify_batch(&[
        Arc::new(resigned.clone()),
        Arc::new(transplanted.clone()),
    ]);

    assert_eq!(
        verdicts,
        vec![Verdict::Skip, Verdict::Skip],
        "a known id carrying a different signature is skipped by the DURABLE \
         byte-compare, never accused"
    );

    // Same two events again, now against a primed LRU rather than the durable
    // seam: accepting the genuine event first puts `(id, sig)` in the cache,
    // so the second batch resolves on the LRU branch.
    assert_eq!(
        verifier.verify_batch(&[Arc::new(genuine)]),
        vec![Verdict::Accept]
    );
    assert_eq!(
        verifier.verify_batch(&[Arc::new(resigned), Arc::new(transplanted)]),
        vec![Verdict::Skip, Verdict::Skip],
        "the LRU branch must skip a different signature too, never accuse"
    );

    assert_eq!(
        verifier.schnorr_verifications(),
        0,
        "a known id is resolved by byte-compare, never schnorr"
    );
}

#[test]
fn lru_hit_skips_durable_and_schnorr() {
    let keys = Keys::generate();
    let event = Arc::new(signed_event(&keys, "first-sighting"));
    let (counting, calls) = CountingKnownSig::new();
    let known_sig: Arc<dyn KnownSig> = Arc::new(counting);
    let mut verifier = default_verifier(known_sig);

    // First sighting: no LRU entry, durable consulted (returns None),
    // candidate -> schnorr, then inserted into the LRU.
    let first = verifier.verify_batch(&[Arc::clone(&event)]);
    assert_eq!(first, vec![Verdict::Accept]);
    let schnorr_after_first = verifier.schnorr_verifications();
    let durable_after_first = calls.load(Ordering::SeqCst);
    assert_eq!(schnorr_after_first, 1);
    assert_eq!(durable_after_first, 1);

    // Second sighting of the SAME event: LRU hit -> byte-compare. Neither
    // the durable seam nor a worker is consulted.
    let second = verifier.verify_batch(&[Arc::clone(&event)]);
    assert_eq!(second, vec![Verdict::Accept]);
    assert_eq!(
        verifier.schnorr_verifications(),
        schnorr_after_first,
        "an LRU hit must not perform another schnorr check"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        durable_after_first,
        "an LRU hit must not consult the durable seam"
    );
}

// ---- moved worker-pool behavioral tests (adapted to Verifier/Verdict) ----

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
                Verdict::Accept
            } else {
                Verdict::RejectMisbehavior
            }
        })
        .collect();
    let mut verifier = default_verifier(Arc::new(NullKnownSig));

    assert_eq!(verifier.verify_batch(&events), expected);
}

#[test]
fn persistent_pool_can_verify_multiple_bursts() {
    let keys = Keys::generate();
    let mut verifier = default_verifier(Arc::new(NullKnownSig));

    for burst in 0..8 {
        let events: Vec<_> = (0..13)
            .map(|index| Arc::new(signed_event(&keys, &format!("{burst}-{index}"))))
            .collect();
        assert_eq!(
            verifier.verify_batch(&events),
            vec![Verdict::Accept; events.len()]
        );
    }
}

#[test]
fn empty_batch_is_empty() {
    let mut verifier = default_verifier(Arc::new(NullKnownSig));
    assert!(verifier.verify_batch(&[]).is_empty());
}

#[test]
fn zero_configuration_is_clamped_and_drop_joins_workers() {
    let verifier = Verifier::new(
        VerifyConfig {
            workers: 0,
            queue_capacity: 0,
            lru_capacity: 0,
        },
        Arc::new(NullKnownSig),
    )
    .unwrap();
    #[cfg(not(target_arch = "wasm32"))]
    assert_eq!(verifier.pool.worker_count(), DEFAULT_VERIFIER_WORKERS);
    drop(verifier);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn stopped_worker_fails_affected_batch_closed_without_panicking() {
    let keys = Keys::generate();
    let event = Arc::new(signed_event(&keys, "must not escape"));
    let mut verifier = default_verifier(Arc::new(NullKnownSig));
    verifier.pool.stop_worker(0);

    assert_eq!(
        verifier.verify_batch(&[Arc::clone(&event)]),
        vec![Verdict::RejectUnavailable],
        "a stopped worker must fail closed, never as misbehavior"
    );
    assert_eq!(
        verifier.verify_batch(&[event]),
        vec![Verdict::Accept],
        "the stopped worker lane must be replaced for future batches"
    );
}

#[test]
fn default_verifier_width_uses_half_the_host_up_to_eight() {
    let available = std::thread::available_parallelism().map_or(2, usize::from);
    assert_eq!(
        VerifyConfig::default().workers,
        available
            .div_ceil(2)
            .clamp(DEFAULT_VERIFIER_WORKERS, MAX_DEFAULT_VERIFIER_WORKERS)
    );
}

#[test]
fn default_verification_cache_covers_a_hundred_thousand_event_replay() {
    assert!(VerifyConfig::default().lru_capacity >= 100_000);
}

#[test]
fn explicit_worker_budget_is_clamped_to_the_hard_ceiling() {
    assert_eq!(configured_workers(0), DEFAULT_VERIFIER_WORKERS);
    assert_eq!(configured_workers(1), 1);
    assert_eq!(configured_workers(4), 4);
    assert_eq!(configured_workers(usize::MAX), MAX_VERIFIER_WORKERS);
}

/// Reproducible real-corpus proof for #168, adapted to the verify crate.
///
/// `NMP_CORPUS` is JSONL with one canonical event object per line. The
/// harness wraps each object in its real relay EVENT envelope, then times
/// exactly one typed relay-message parse per frame (pulling the `Event`
/// directly out of the `["EVENT", sub, <event>]` array) and the
/// known-redelivery signature-compare path for the required burst matrix.
#[test]
#[ignore = "requires NMP_CORPUS real-event JSONL"]
fn real_corpus_verify_matrix() {
    use nostr::{JsonUtil, RelayMessage};
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
            // Pull the event straight out of the relay EVENT envelope
            // without depending on transport's RelayFrame.
            let events: Vec<Arc<Event>> = wire[..size]
                .iter()
                .map(|raw| {
                    let parsed: RelayMessage<'static> =
                        RelayMessage::from_json(raw).expect("parse real relay EVENT once");
                    let event = match parsed {
                        RelayMessage::Event { event, .. } => event.into_owned(),
                        _ => panic!("fixture wrapper must be an EVENT frame"),
                    };
                    Arc::new(event)
                })
                .collect();
            parse_samples.push(started.elapsed());

            let mut verifier = Verifier::new(VerifyConfig::default(), Arc::new(NullKnownSig))
                .expect("benchmark verifier construction");
            let started = Instant::now();
            assert!(events.iter().all(|event| event.verify_id()));
            let valid = verifier.verify_batch(black_box(&events));
            verify_samples.push(started.elapsed());
            assert!(valid.iter().all(|verdict| *verdict == Verdict::Accept));

            let known: HashMap<_, _> = events.iter().map(|event| (event.id, event.sig)).collect();
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
