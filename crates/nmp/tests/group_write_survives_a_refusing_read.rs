//! #1122 PROTOCOL-GROUPISANIDENTITY-003: "I can write into a group whose
//! content one host refuses to let me read."
//!
//! Two hosts serve the SAME group id. Host A refuses every query outright
//! (`CLOSED`, never `EOSE` -- `RelayConfig::reject_queries`, the exact knob
//! `nmp-bdd`'s unused `relay "<name>" refuses my reads until I am a member`
//! step already stages). Host B is healthy and already holds one of the
//! group's own kind:9 events. A `Group::read` built from BOTH hosts is
//! observed as one ordinary live query, and a `join_request` is published
//! through the SAME `Group` value.
//!
//! The engine's evidence surface (`crates/nmp/src/core/evidence.rs`'s module
//! doc) deliberately has no aggregate complete/empty verdict, so "the query
//! does not report the group as empty because of host A" cannot be asserted
//! as a single boolean. It is proved as three separate, independently
//! checkable facts instead:
//!
//!   (a) host B's row still surfaces in the live query's row set, sourced
//!       from host B -- the healthy host's answer is never suppressed by
//!       host A's refusal;
//!   (b) host A's refusal reaches the app as an explicit PER-HOST fact:
//!       `Frame::execution` carries an `ObservationFact::RelayClosed`
//!       (facade kind `"relay_closed"`) naming host A and the relay's own
//!       CLOSED reason string, host A's connected Public source reads
//!       `SourceStatus::Error` after that exact request is retired, AND it
//!       never accumulates a `reconciled_through` watermark -- the type's own
//!       documented meaning of "unproven", never silently upgraded to
//!       "complete";
//!   (c) the join request reaches the door and is acked by both hosts --
//!       `RelayConfig::reject_queries` refuses reads only; the SAME hosts'
//!       write policy is untouched, so writing into a group you cannot read
//!       yet (`Group::join_request`'s own doc) is not merely expressible,
//!       it is delivered.
//!
//! ## What `reject_queries` actually proves
//!
//! `crates/nmp-test-support/src/relays.rs`'s `LoggingQueryPolicy` answers
//! every REQ with `CLOSED "error: nmp-bdd scripted relay: configured to
//! never confirm end of stored events"`. That message's prefix is `error`,
//! which `crates/nmp/src/core/auth_transport.rs`'s `RelayMessage::Closed`
//! match does NOT read as `auth-required`/`restricted` (those get their own
//! AUTH-policy branch); it falls through to the general arm and calls
//! `EngineCore::close_requests_for_sub`, which is what emits the
//! `RelayClosed` observation fact this test asserts on.
//!
//! `CLOSED` does not drop the transport session -- host A's connection stays
//! up -- but it does retire the exact accepted request. The router still
//! plans that source while the reducer has neither a live placement nor an
//! owned local retry, so a connected Public source truthfully reads
//! `SourceStatus::Error`; a dropped session would instead read
//! `Disconnected`. There is no invented `SourceStatus::Refused` variant.
//! The honest, provable per-host facts are therefore the explicit
//! `relay_closed` execution fact naming the host and its wire reason, the
//! connected source's `Error` state, and `reconciled_through: None` -- never
//! a lie that the read completed or proved an empty result.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use nmp::{
    nip29, AccessContext, AcquisitionEvidence, Engine, EngineConfig, EventId, Filter, PublicKey,
    RelayState, Row, RowDelta, SourceStatus, Subscription, WriteFact,
};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{Keys, Kind, RelayUrl, Timestamp, UnsignedEvent};

const GROUP_ID: &str = "photographers";
const GROUP_KIND: u16 = 9;

/// Long enough for a real connect/REQ/CLOSED/EOSE round trip on a loaded CI
/// runner, short enough that a genuine failure reports rather than hangs.
const SETTLE: Duration = Duration::from_secs(20);

fn bare_engine() -> Engine {
    Engine::new(EngineConfig {
        ..EngineConfig::default()
    })
    .expect("an in-memory engine builds")
}

/// A bare engine, plus one registered signing capability for `keys` -- what
/// `Identity::Explicit(keys.public_key())` (every `Group::publish` call,
/// `join_request` included) needs to actually produce a signature. No
/// indexer and no current account: an `Explicit`-routed group write consults
/// neither.
fn engine_with_signer_for(keys: &Keys) -> Engine {
    let engine = bare_engine();
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), false)
        .expect("the account and local provider register");
    engine
}

/// One relay-signed NIP-29 kind:9 fixture: host B's own content, seeded
/// directly (a `Given`, never routed through the engine).
fn relay_signed_kind9(signer: &Keys, created_at: u64) -> nostr::Event {
    UnsignedEvent::new(
        signer.public_key(),
        Timestamp::from(created_at),
        Kind::from(GROUP_KIND),
        vec![nostr::Tag::parse(["h", GROUP_ID]).expect("h row parses")],
        "seeded".to_string(),
    )
    .sign_with_keys(signer)
    .expect("fixture keys sign cleanly")
}

fn apply(current: &mut BTreeMap<EventId, Row>, deltas: Vec<RowDelta>) {
    for delta in deltas {
        match delta {
            RowDelta::Added(row) => {
                current.insert(row.id(), row);
            }
            RowDelta::Updated(row) => {
                current.insert(row.id(), row);
            }
            RowDelta::SourcesGrew { id, sources } => {
                if let Some(row) = current.get_mut(&id) {
                    row.sources = sources;
                }
            }
            RowDelta::Removed(id) => {
                current.remove(&id);
            }
        }
    }
}

/// What one poll pass accumulates: the live row projection (deltas applied
/// in order, same discipline as `group_publication_door.rs`'s
/// `wait_for_group_rows`), the MOST RECENT `AcquisitionEvidence` snapshot
/// (every `Frame::evidence` is already the observation's full current
/// per-branch snapshot -- see `EngineCore::refresh_observation`'s own doc:
/// "evidence can change with no row change at all ... that case still
/// emits" -- so overwriting, never merging, is the correct fold), and every
/// `relay_closed` execution fact ever seen (a discrete one-shot fact, so
/// those DO accumulate).
#[derive(Default)]
struct Observed {
    rows: BTreeMap<EventId, Row>,
    evidence: Vec<AcquisitionEvidence>,
    relay_closed_relays: BTreeSet<RelayUrl>,
    relay_closed_reasons: Vec<String>,
}

/// Drain `subscription` until `pred(&Observed)` holds, folding every frame's
/// deltas/evidence/execution into one running view. Bounded -- reports what
/// it DID see when it gives up, because "never connected" and "connected but
/// never proven" are different failures worth telling apart.
fn poll_until(
    subscription: &Subscription,
    timeout: Duration,
    pred: impl Fn(&Observed) -> bool,
) -> Observed {
    let deadline = Instant::now() + timeout;
    let mut observed = Observed::default();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "live query never satisfied the predicate; rows={:?} evidence={:?} \
                 relay_closed_relays={:?}",
                observed.rows.keys().collect::<Vec<_>>(),
                observed.evidence,
                observed.relay_closed_relays
            );
        }
        match subscription.recv_timeout(remaining) {
            Ok(frame) => {
                apply(&mut observed.rows, frame.deltas);
                if !frame.evidence.is_empty() {
                    observed.evidence = frame.evidence;
                }
                for fact in &frame.execution {
                    if fact.kind == "relay_closed" {
                        for (key, value) in &fact.attributes {
                            if key == "relay" {
                                if let Ok(relay) = RelayUrl::parse(value) {
                                    observed.relay_closed_relays.insert(relay);
                                }
                            }
                            if key == "reason" {
                                observed.relay_closed_reasons.push(value.clone());
                            }
                        }
                    }
                }
                if pred(&observed) {
                    return observed;
                }
            }
            Err(error) => panic!(
                "subscription ended before the predicate was satisfied ({error:?}); rows={:?} \
                 evidence={:?} relay_closed_relays={:?}",
                observed.rows.keys().collect::<Vec<_>>(),
                observed.evidence,
                observed.relay_closed_relays
            ),
        }
    }
}

fn drain_until_all_acked(
    receipts: &nmp::mechanism::runtime::FifoReceiver<WriteFact>,
    expected: &BTreeSet<RelayUrl>,
) -> Vec<WriteFact> {
    let deadline = Instant::now() + SETTLE;
    let mut seen = Vec::new();
    let mut acked: BTreeSet<RelayUrl> = BTreeSet::new();
    loop {
        if &acked == expected {
            return seen;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("not every expected host acked; acked {acked:?} of {expected:?}; saw {seen:?}");
        }
        match receipts.recv_timeout(remaining) {
            Ok(status) => {
                if let WriteFact::Relay {
                    relay,
                    state: RelayState::Published,
                } = &status
                {
                    acked.insert(relay.clone());
                }
                seen.push(status);
            }
            Err(error) => panic!(
                "receipt stream ended early ({error:?}); acked {acked:?} of {expected:?}; \
                 saw {seen:?}"
            ),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_request_is_delivered_while_the_same_groups_read_reports_one_hosts_refusal_as_an_explicit_fact_and_never_as_a_false_empty(
) {
    let host_b_signer = Keys::generate(); // stands in for host B's own relay-signed content
    let writer_keys = Keys::generate();
    let writer: PublicKey = writer_keys.public_key(); // the identity publishing the join request

    let host_a = ScriptedRelay::start(&RelayConfig {
        reject_queries: true,
        ..RelayConfig::default()
    })
    .await;
    let host_b = ScriptedRelay::start(&RelayConfig::default()).await;

    // Host B already holds one of the group's own kind:9 events -- the
    // healthy host's pre-existing content a working read must surface.
    let seeded = relay_signed_kind9(&host_b_signer, 1_700_000_000);
    host_b.seed_signed_event(&seeded).await;

    let engine = engine_with_signer_for(&writer_keys);
    let scope =
        nip29::on([host_a.url.clone(), host_b.url.clone()]).expect("two hosts form a scope");
    let group = scope.group(GROUP_ID);

    // ---- the read: one live query, one branch per host -----------------
    let query = group
        .read(Filter {
            kinds: Some(BTreeSet::from([GROUP_KIND])),
            ..Filter::default()
        })
        .expect("a two-host group read declares two branches");
    let subscription = engine
        .observe(query, None)
        .expect("a group read is an ordinary live query");

    // ---- the write: the SAME group value, no read prerequisite ---------
    let receipts = group
        .join_request(&engine, writer, None)
        .expect("a join request is accepted with no subscription open at all")
        .statuses;
    let expected_hosts = BTreeSet::from([host_a.url.clone(), host_b.url.clone()]);
    let write_statuses = drain_until_all_acked(&receipts, &expected_hosts);
    assert!(
        write_statuses
            .iter()
            .any(|status| matches!(status, WriteFact::Relay { relay, state: RelayState::Published } if *relay == host_a.url)),
        "host A refuses READS, not writes -- it must still ack the join request: {write_statuses:?}"
    );
    assert!(
        write_statuses
            .iter()
            .any(|status| matches!(status, WriteFact::Relay { relay, state: RelayState::Published } if *relay == host_b.url)),
        "host B must ack the join request too: {write_statuses:?}"
    );
    let delivered_to_b = {
        let deadline = Instant::now() + SETTLE;
        loop {
            let admitted: Vec<_> = host_b
                .admitted_events()
                .into_iter()
                .filter(|event| event.kind == Kind::from(9021u16))
                .collect();
            if !admitted.is_empty() {
                break admitted;
            }
            assert!(
                Instant::now() < deadline,
                "host B never admitted the join request"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    assert_eq!(
        delivered_to_b.len(),
        1,
        "exactly one join request reached host B"
    );
    assert_eq!(
        delivered_to_b[0].pubkey.to_string(),
        writer.to_string(),
        "the join request carries the writer's own pubkey"
    );

    // ---- the read's three facts -----------------------------------------
    let observed = poll_until(&subscription, SETTLE, |observed| {
        observed.rows.contains_key(&seeded.id)
            && observed.relay_closed_relays.contains(&host_a.url)
            && observed
                .evidence
                .iter()
                .flat_map(|entry| entry.sources.iter())
                .any(|source| source.relay == host_a.url && source.status == SourceStatus::Error)
    });

    // (a) host B's row surfaces normally, sourced from host B -- the group
    // is never reported empty because of host A.
    let seeded_row = observed
        .rows
        .get(&seeded.id)
        .expect("host B's seeded event surfaced");
    assert_eq!(
        seeded_row.sources,
        BTreeSet::from([host_b.url.clone()]),
        "host B's row is sourced from host B alone"
    );

    // (b) host A's refusal is an explicit PER-HOST fact, both on the
    // execution trace and on the acquisition evidence -- never silently
    // folded into the row set's shape.
    assert!(
        observed.relay_closed_relays.contains(&host_a.url),
        "host A's CLOSED must surface as its own relay_closed execution fact: saw {:?}",
        observed.relay_closed_relays
    );
    assert!(
        observed
            .relay_closed_reasons
            .iter()
            .any(|reason| reason.contains("never confirm end of stored events")),
        "the relay's own CLOSED wire reason must reach the app verbatim: {:?}",
        observed.relay_closed_reasons
    );
    let host_a_source = observed
        .evidence
        .iter()
        .flat_map(|entry| entry.sources.iter())
        .find(|source| source.relay == host_a.url)
        .expect("host A still names a covering source for this query's subtree");
    assert_eq!(
        host_a_source.access,
        AccessContext::Public,
        "the refused read remains scoped to the connected Public session"
    );
    assert_eq!(
        host_a_source.status,
        SourceStatus::Error,
        "CLOSED retires the accepted request without dropping host A's Public transport; \
         a dropped transport would read Disconnected, while this connected plan now has \
         neither a live placement nor an owned retry"
    );
    assert_eq!(
        host_a_source.reconciled_through, None,
        "host A never lands a coverage watermark: CLOSED never yields the EOSE that would \
         record one, so this source can never be misread as proven or complete"
    );
    let host_b_source = observed
        .evidence
        .iter()
        .flat_map(|entry| entry.sources.iter())
        .find(|source| source.relay == host_b.url)
        .expect("host B names a covering source for this query's subtree");
    assert!(
        host_b_source.reconciled_through.is_some(),
        "host B DID complete a real EOSE round trip and must carry a proven watermark, \
         in direct contrast to host A's None: {host_b_source:?}"
    );

    // (c) never a false empty: the group's row set is genuinely nonempty
    // and genuinely sourced, at the exact same moment host A's refusal is
    // on record.
    assert!(
        !observed.rows.is_empty(),
        "the group's row set must not present as empty because one host refused"
    );

    drop(subscription);
    engine.shutdown();
    host_a.shutdown();
    host_b.shutdown();
}
