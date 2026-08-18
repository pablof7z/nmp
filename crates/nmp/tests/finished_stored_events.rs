//! #1235: a source that has FINISHED answering is a fact NMP holds, and until
//! now would not say.
//!
//! `features/coverage/absence-settlement.feature` states the rule verbatim as
//! the owner's own ruling -- "the moment we receive EOSE from the indexer
//! relays we use we know, one way or another" -- and adds that settlement "is
//! that confirmation and nothing else -- not a timeout, not a retry budget,
//! not a heuristic". The engine computes exactly that: an EOSE consumes the
//! request's attribution FIFO and removes its `ActiveRequestEvidence`.
//!
//! But removal is not a readable fact. The only thing an app could see was
//! `SourceEvidence`, whose six-state `SourceStatus` had no member meaning
//! "this source finished its request" and whose `reconciled_through` is a
//! different claim in both directions: `Some` from a PRIOR window while a
//! fresh request is still streaming, and `None` after a request that finished
//! but was bounded by a NIP-01 `limit` and so may claim no interval at all.
//! An app wanting "give me this snapshot once its sources have finished" had
//! nothing to key on, and mosaico put a 500ms wall clock there -- twice, in
//! `src/nmp_host/read.rs` and `tests/common/nmp_client/read.rs`, which have
//! already drifted from each other.
//!
//! These are the three discriminating cases, staged against real in-process
//! relays over real sockets.
//!
//! The mechanism under test is `EngineCore::finish_stored_events`, reached
//! from BOTH terminal arms (`emit_request_settled` and
//! `retire_request_evidence`) and read by `evidence::acquisition_evidence`.
//! Disable it -- make `finish_stored_events` return without writing, or drop
//! the `retire_request_evidence` call -- and the first two tests below fail
//! on a source that stays `Requesting` forever.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::{
    AcquisitionEvidence, Demand, Engine, EngineConfig, Filter, LiveQuery, ReadRouting,
    SourceStatus, Subscription,
};
use nmp_test_support::relays::{free_port, RelayConfig, ScriptedRelay};
use nostr::RelayUrl;

/// Long enough for a real connect/REQ/EOSE round trip on a loaded runner,
/// short enough that a genuine failure reports rather than hangs. This bounds
/// the TEST; nothing under test reads a clock.
const SETTLE: Duration = Duration::from_secs(20);
/// Longer than the full evidence wait after the relay-side REQ witness fires.
/// The request is genuinely accepted locally and reaches the relay, but its
/// EOSE cannot race the assertion that it remains outstanding.
const WITHHELD_EOSE_DELAY: Duration = Duration::from_secs(40);

const KIND: u16 = 9999;

fn engine() -> Engine {
    Engine::new(EngineConfig {
        ..EngineConfig::default()
    })
    .expect("a temporary Redb engine builds")
}

/// One branch per relay, each pinned to exactly that relay, so every source
/// fact below belongs to one host and no host's plan can prove another's.
fn query(relays: &[&RelayUrl], limit: Option<usize>) -> LiveQuery {
    let branches: Vec<LiveQuery> = relays
        .iter()
        .map(|relay| {
            Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([KIND])),
                    limit,
                    ..Filter::default()
                },
                ReadRouting::Explicit(vec![(*relay).clone()]),
            )
            .expect("a one-relay pinned set is nonempty")
        })
        .map(LiveQuery::single)
        .collect();
    LiveQuery::union(branches, None).expect("at least one branch")
}

/// Drain frames, keeping the newest evidence snapshot, until `pred` holds.
/// Every `Frame::evidence` is the observation's full current per-branch
/// snapshot, so overwriting is the correct fold.
fn evidence_until(
    subscription: &Subscription,
    pred: impl Fn(&[AcquisitionEvidence]) -> bool,
) -> Vec<AcquisitionEvidence> {
    let deadline = Instant::now() + SETTLE;
    let mut latest: Vec<AcquisitionEvidence> = Vec::new();
    loop {
        if !latest.is_empty() && pred(&latest) {
            return latest;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the evidence never satisfied the predicate; last snapshot was {latest:?}"
        );
        match subscription.recv_timeout(remaining) {
            Ok(frame) => {
                assert!(
                    frame.deltas.is_empty(),
                    "no relay here holds a kind:{KIND} event, so no row may ever arrive: {:?}",
                    frame.deltas
                );
                if !frame.evidence.is_empty() {
                    latest = frame.evidence;
                }
            }
            Err(error) => {
                panic!("the subscription ended before the predicate held ({error:?}): {latest:?}")
            }
        }
    }
}

fn source_at<'a>(evidence: &'a [AcquisitionEvidence], relay: &RelayUrl) -> &'a nmp::SourceEvidence {
    evidence
        .iter()
        .flat_map(|branch| branch.sources.iter())
        .find(|source| &source.relay == relay)
        .unwrap_or_else(|| panic!("{relay} must name a covering source: {evidence:?}"))
}

/// Whether `relay` currently reports `status`, tolerating its absence.
///
/// A predicate, unlike an assertion, is asked of snapshots taken while the
/// engine is still connecting, so a relay it names may not be a source yet.
/// Deliberately NOT `source_at`: panicking inside the wait would make the
/// FIRST frame decide the test rather than the fact it is waiting for.
fn reports(evidence: &[AcquisitionEvidence], relay: &RelayUrl, status: SourceStatus) -> bool {
    evidence
        .iter()
        .flat_map(|branch| branch.sources.iter())
        .any(|source| &source.relay == relay && source.status == status)
}

/// The headline. Two relays are asked the same question at the same moment;
/// one finishes answering while the other's accepted request remains
/// outstanding. Before #1235 both read
/// `Requesting` with `reconciled_through: None` -- byte-identical evidence for
/// "done, and there was nothing" and "still going" -- and the only thing that
/// could tell them apart was how long an app was willing to wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_finished_its_request_is_distinguishable_from_one_still_answering() {
    let finished = ScriptedRelay::start(&RelayConfig::default()).await;
    // Hold the accepted query before the relay serves stored events and EOSE.
    // Unlike the old rejection approximation, this creates a real outstanding
    // request and therefore makes `Requesting` truthful.
    let unfinished = ScriptedRelay::start(&RelayConfig {
        query_delay: Some(WITHHELD_EOSE_DELAY),
        ..RelayConfig::default()
    })
    .await;

    let engine = engine();
    let subscription = engine
        .observe(query(&[&finished.url, &unfinished.url], None), None)
        .expect("a two-branch pinned read opens");

    assert!(
        unfinished.wait_query_for_kind(KIND, SETTLE).await,
        "the unfinished relay must independently witness the exact inbound REQ"
    );
    assert_eq!(
        unfinished.query_count_for_kind(KIND),
        1,
        "the delayed relay must receive exactly the planned kind:{KIND} request"
    );

    // Wait for BOTH facts, not for the first one. The contrast is the whole
    // scenario, and the two relays connect independently: stopping as soon as
    // one has finished can catch the other still `Connecting`, which is a true
    // fact about a race rather than the one being asserted. Both states are
    // stable inside the relay's delay window, so this terminates on facts,
    // not on a guessed settlement timeout.
    let evidence = evidence_until(&subscription, |evidence| {
        reports(evidence, &finished.url, SourceStatus::FinishedStoredEvents)
            && reports(evidence, &unfinished.url, SourceStatus::Requesting)
    });

    let finished_source = source_at(&evidence, &finished.url);
    assert_eq!(
        finished_source.status,
        SourceStatus::FinishedStoredEvents,
        "this relay sent its EOSE having sent nothing; that IS the settlement the corpus \
         names, and it must be readable as one: {finished_source:?}"
    );

    let unfinished_source = source_at(&evidence, &unfinished.url);
    assert_eq!(
        unfinished_source.status,
        SourceStatus::Requesting,
        "a relay that has not yet confirmed end of stored events has an outstanding request and \
         must keep saying so -- waiting longer is not a settlement: {unfinished_source:?}"
    );
    assert_eq!(
        unfinished_source.reconciled_through, None,
        "and it proved nothing: {unfinished_source:?}"
    );

    // The distinction is per SOURCE and stays there. One relay finishing has
    // not made the query complete, and nothing on this surface says it did.
    assert!(
        evidence.iter().all(|branch| branch.shortfall.is_empty()),
        "both relays are honestly planned: {evidence:?}"
    );

    drop(subscription);
    drop(engine);
    finished.shutdown();
    unfinished.shutdown();
}

/// The boundary that keeps the new fact from being read as the old collapsed
/// verdict. A request the caller bounded with a NIP-01 `limit` may claim NO
/// coverage interval at all -- `AttributionState::record_send` poisons it
/// `LimitedRequest`, so its EOSE takes the `retire_request_evidence` arm and
/// never lands a watermark. It still FINISHED, and the two facts must be able
/// to disagree: finished, and having proven nothing.
///
/// This is not a corner. It is the exact shape mosaico's test client hits on
/// every read, which is why that copy of the heuristic had to accept a source
/// its production twin rejects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finishing_a_bounded_request_proves_nothing_and_says_so() {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let engine = engine();
    let subscription = engine
        .observe(query(&[&relay.url], Some(10)), None)
        .expect("a bounded pinned read opens");

    let evidence = evidence_until(&subscription, |evidence| {
        reports(evidence, &relay.url, SourceStatus::FinishedStoredEvents)
    });

    let source = source_at(&evidence, &relay.url);
    assert_eq!(
        source.status,
        SourceStatus::FinishedStoredEvents,
        "a bounded request still ends: the relay sent everything it was going to send"
    );
    assert_eq!(
        source.reconciled_through, None,
        "and it proved no interval, because a limited REQ may not claim one. Collapsing these \
         two into one fact is what #49 deleted and what this pair keeps deleted: {source:?}"
    );

    relay.shutdown();
}

/// `features/coverage/empty-vs-unknown.feature`'s founding distinction, now
/// with the positive half reachable. A relay that has not answered is not the
/// same as a relay that answered with nothing, and an empty row set is
/// evidence of neither. Both sources here are empty; only their acquisition
/// facts tell them apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_result_is_never_proven_by_a_source_that_has_not_answered() {
    let never_connected = RelayUrl::parse(&format!("ws://127.0.0.1:{}", free_port()))
        .expect("a bound-to-nothing loopback url parses");
    let unfinished = ScriptedRelay::start(&RelayConfig {
        query_delay: Some(WITHHELD_EOSE_DELAY),
        ..RelayConfig::default()
    })
    .await;

    let engine = engine();
    let subscription = engine
        .observe(query(&[&never_connected, &unfinished.url], None), None)
        .expect("a two-branch pinned read opens");

    assert!(
        unfinished.wait_query_for_kind(KIND, SETTLE).await,
        "the unfinished relay must independently witness the exact inbound REQ"
    );
    assert_eq!(
        unfinished.query_count_for_kind(KIND),
        1,
        "the delayed relay must receive exactly the planned kind:{KIND} request"
    );

    // The delayed relay must have got far enough for local acceptance: a
    // snapshot taken before it connected would satisfy the negative
    // assertions below for the wrong reason.
    let evidence = evidence_until(&subscription, |evidence| {
        reports(evidence, &unfinished.url, SourceStatus::Requesting)
    });

    for source in evidence.iter().flat_map(|branch| branch.sources.iter()) {
        assert_ne!(
            source.status,
            SourceStatus::FinishedStoredEvents,
            "nothing here has confirmed end of stored events, so nothing may report having \
             finished -- a source that cannot connect and one still answering are both open \
             open questions: {source:?}"
        );
        assert_eq!(
            source.reconciled_through, None,
            "and neither proved an interval: {source:?}"
        );
    }

    drop(subscription);
    drop(engine);
    unfinished.shutdown();
}
