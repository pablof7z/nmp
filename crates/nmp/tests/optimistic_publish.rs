//! Falsifiers for #1182: a locally accepted write appears immediately, with
//! honest provenance.
//!
//! Two facts are deliberately kept apart here, because conflating them is the
//! defect this file exists to prevent:
//!
//!   * whether an event APPEARS in a live query whose filter it matches, and
//!   * whether a RELAY carried it.
//!
//! An event accepted into the outbound publication queue is genuinely there.
//! Withholding it until some relay ACKs makes the feed lie about what the user
//! just did. The honest shape is: show it, report its provenance as the local
//! cache and ZERO relays (`Row::sources` empty -- the one existing spelling of
//! "where did this come from", never a second parallel one), and let that set
//! grow as relays actually carry it.
//!
//! | falsifier | what it pins |
//! |---|---|
//! | `a_publish_to_two_unreachable_hosts_appears_at_once_reporting_zero_relays` | #1182 f1, through NIP-29 |
//! | `an_accepting_host_enters_provenance_a_rejecting_one_never_does_and_the_row_is_never_duplicated` | #1182 f2 |
//! | `optimistic_publication_is_general_and_owes_nothing_to_nip29` | #1182 f4 -- the SAME claim with no NIP-29 anywhere, over two ordinary kinds |
//!
//! #1182 f3 (the cross-host leak PR #1173 fixed stays fixed) is
//! `group_publication_door.rs`'s
//! `a_group_records_listing_never_lets_one_hosts_member_evidence_answer_for_anothers_group`,
//! plus `nip29_owns_no_publication_visibility_rule.rs` for f5. Nothing here
//! weakens it: a row ANOTHER host served is still never projected under a pin
//! that host is not in. This file is about a row NO host has served yet, which
//! is not foreign data at all -- it is ours, still in the outbound queue.
//!
//! Every proof runs against real sockets and real in-process relays, because
//! the claim is about what an app actually receives.
//!
//! Same version-shadowing precaution as `runtime_integration.rs`: never
//! `use nostr_relay_builder::prelude::*`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use nmp::mechanism::runtime::FifoReceiver;
use nmp::nip29;
use nmp::{
    AccessContext, CacheMode, Demand, Engine, EngineConfig, Filter, Identity, LiveQuery,
    RelayState, Row, RowDelta, SourceAuthority, WriteFact, WriteIntent, WritePayload, WriteRouting,
};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{EventId, Keys, Kind, RelayUrl, Tag, Timestamp, UnsignedEvent};

const GROUP_ID: &str = "photographers";
const GROUP_KIND: u16 = 9;
/// An ordinary NIP-01 short text note -- nothing to do with NIP-29.
const TEXT_NOTE_KIND: u16 = 1;
/// An ordinary addressable long-form article -- a SECOND, structurally
/// different kind, so "general" is demonstrated rather than asserted.
const LONG_FORM_KIND: u16 = 30_023;

const SETTLE: Duration = Duration::from_secs(20);
/// How long a negative assertion waits for a wrong answer to show up. Long
/// enough that "it never arrived" is a finding and not a race.
const QUIET: Duration = Duration::from_millis(750);
/// Delivery retry is exponential with jitter (3s base). A host that comes up
/// after the first attempt failed is reached on a later attempt, so anything
/// waiting on that transition needs a budget measured in retries.
const RETRY_SETTLE: Duration = Duration::from_secs(90);

fn engine() -> Engine {
    Engine::new(EngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        ..EngineConfig::default()
    })
    .expect("an in-memory engine builds")
}

/// A real loopback URL with nothing listening on it: the port is bound just
/// long enough to learn a free number, then released. A host that cannot be
/// reached is the point -- no ACK can arrive to rescue a projection that
/// wrongly waits for one.
fn unreachable_host() -> RelayUrl {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind for a free port");
    let port = listener
        .local_addr()
        .expect("a bound listener has an address")
        .port();
    drop(listener);
    RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("a well-formed loopback relay url")
}

fn port_of(url: &RelayUrl) -> u16 {
    url.as_str_without_trailing_slash()
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .expect("a loopback relay url carries an explicit port")
}

fn signed_event(
    keys: &Keys,
    kind: u16,
    created_at: u64,
    tags: Vec<Tag>,
    content: &str,
) -> nostr::Event {
    UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(created_at),
        Kind::from_u16(kind),
        tags,
        content.to_string(),
    )
    .sign_with_keys(keys)
    .expect("fixture keys sign cleanly")
}

/// One branch of an ordinary pinned, cache-strict read -- the shape #1173
/// made NIP-29 use, expressed here with no protocol helper at all.
fn pinned_strict_branch(hosts: &[RelayUrl], kind: u16) -> Demand {
    let mut demand = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([kind])),
            ..Filter::default()
        },
        SourceAuthority::Pinned(hosts.iter().cloned().collect()),
        AccessContext::Public,
    )
    .expect("a nonempty pinned set with a non-outbox source is constructible");
    demand.cache = CacheMode::Strict;
    demand
}

/// The folded row set, plus the two things about the DELTA STREAM that
/// #1182's contract is actually about: how many times a row was announced as
/// new, and what provenance it claimed the FIRST time it appeared.
///
/// The second is the load-bearing one. "Sources end up naming the accepting
/// host" is true whether or not the event was ever shown optimistically --
/// the host echoes it back over the open subscription either way. Only the
/// first appearance distinguishes "shown the moment we accepted it, claiming
/// no relay" from "withheld until a relay ACKed".
#[derive(Default)]
struct Observed {
    rows: BTreeMap<EventId, Row>,
    added_counts: BTreeMap<EventId, usize>,
    first_sources: BTreeMap<EventId, BTreeSet<RelayUrl>>,
}

fn apply(observed: &mut Observed, deltas: Vec<RowDelta>) {
    for delta in deltas {
        match delta {
            RowDelta::Added(row) => {
                *observed.added_counts.entry(row.id()).or_default() += 1;
                observed
                    .first_sources
                    .entry(row.id())
                    .or_insert_with(|| row.sources.clone());
                observed.rows.insert(row.id(), row);
            }
            RowDelta::Updated(row) => {
                observed.rows.insert(row.id(), row);
            }
            RowDelta::SourcesGrew { id, sources } => {
                if let Some(row) = observed.rows.get_mut(&id) {
                    row.sources = sources;
                }
            }
            RowDelta::Removed(id) => {
                observed.rows.remove(&id);
            }
        }
    }
}

/// Fold the delta stream until `pred` holds over the accumulated rows.
fn wait_for_rows(
    subscription: &nmp::Subscription,
    timeout: Duration,
    observed: &mut Observed,
    pred: impl Fn(&BTreeMap<EventId, Row>) -> bool,
) {
    let deadline = Instant::now() + timeout;
    loop {
        if pred(&observed.rows) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "the rows never satisfied the predicate; saw {:?}",
                observed
                    .rows
                    .values()
                    .map(|row| (row.id(), row.sources.clone()))
                    .collect::<Vec<_>>()
            );
        }
        match subscription.recv_timeout(remaining) {
            Ok(frame) => apply(observed, frame.deltas),
            Err(error) => {
                panic!("subscription ended before the predicate was satisfied ({error:?})")
            }
        }
    }
}

/// Keep folding for a bounded quiet window so a late/wrong delta has a real
/// chance to arrive before a negative assertion runs.
fn settle(subscription: &nmp::Subscription, observed: &mut Observed, quiet: Duration) {
    while let Ok(frame) = subscription.recv_timeout(quiet) {
        apply(observed, frame.deltas);
    }
}

fn drain_until(
    receipts: &FifoReceiver<WriteFact>,
    timeout: Duration,
    mut pred: impl FnMut(&WriteFact) -> bool,
) -> Vec<WriteFact> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("the receipt stream never satisfied the predicate; saw {seen:?}");
        }
        match receipts.recv_timeout(remaining) {
            Ok(status) => {
                let done = pred(&status);
                seen.push(status);
                if done {
                    return seen;
                }
            }
            Err(error) => panic!("the receipt stream ended early ({error:?}); saw {seen:?}"),
        }
    }
}

// ===========================================================================
// #1182 falsifier 1 -- publish into a group pinned to two hosts that are BOTH
// unreachable. A matching live query emits the event immediately, and its
// provenance reports the cache and zero relays.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_publish_to_two_unreachable_hosts_appears_at_once_reporting_zero_relays() {
    let me = Keys::generate();
    let host_a = unreachable_host();
    let host_b = unreachable_host();
    assert_ne!(host_a, host_b, "two distinct unreachable hosts");

    let engine = engine();
    let group = nip29::on([host_a.clone(), host_b.clone()])
        .expect("two hosts form a scope")
        .group(GROUP_ID);

    let event = signed_event(
        &me,
        GROUP_KIND,
        1_700_000_001,
        vec![Tag::parse(["h", GROUP_ID]).expect("a well-formed h tag")],
        "sent while both hosts are down",
    );
    let known_id = event.id;

    let subscription = engine
        .observe(
            group
                .read(Filter {
                    kinds: Some(BTreeSet::from([GROUP_KIND])),
                    ..Filter::default()
                })
                .expect("a group read opens"),
            None,
        )
        .expect("a NIP-29 read is an ordinary live query");

    // The completion signal an app's chat input shows is available at LOCAL
    // acceptance -- "it will publish" -- not at a first ACK that, here, can
    // never come. Acceptance IS this call returning `Ok`: there is no
    // acceptance fact to wait for, and waiting for one would be the very
    // thing optimistic publishing exists to avoid.
    let _receipts = engine
        .publish(WriteIntent {
            payload: WritePayload::Signed(event),
            routing: WriteRouting::Explicit(vec![host_a, host_b]),
            identity: Identity::Explicit(me.public_key()),
            correlation: None,
        })
        .expect("an already-signed event is accepted by the one publish door");

    let mut observed = Observed::default();
    wait_for_rows(&subscription, SETTLE, &mut observed, |rows| {
        rows.contains_key(&known_id)
    });

    let row = &observed.rows[&known_id];
    assert_eq!(
        row.sources,
        BTreeSet::new(),
        "the row must claim NO relay carried it -- it came from the cache, and \
         nothing has ACKed it. saw {:?}",
        row.sources
    );

    drop(subscription);
    engine.shutdown();
}

// ===========================================================================
// #1182 falsifier 2 -- the continuation of falsifier 1. The event is already
// on screen claiming zero relays; THEN one host accepts and the other rejects.
// Provenance updates to name exactly the accepting host, and the row is never
// duplicated.
//
// Both hosts start down and come up afterwards, which is what makes the
// optimistic window observable at all rather than a race against a fast local
// relay: while nothing is listening, "shown with zero relays" is the only
// state the row can be in.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepting_host_enters_provenance_a_rejecting_one_never_does_and_the_row_is_never_duplicated(
) {
    let me = Keys::generate();
    // Two ports nothing is listening on YET -- the hosts are started on these
    // exact ports further down.
    let accepting_url = unreachable_host();
    let rejecting_url = unreachable_host();

    let engine = engine();
    let group = nip29::on([accepting_url.clone(), rejecting_url.clone()])
        .expect("two hosts form a scope")
        .group(GROUP_ID);

    let event = signed_event(
        &me,
        GROUP_KIND,
        1_700_000_002,
        vec![Tag::parse(["h", GROUP_ID]).expect("a well-formed h tag")],
        "accepted by one host, refused by the other",
    );
    let known_id = event.id;

    let subscription = engine
        .observe(
            group
                .read(Filter {
                    kinds: Some(BTreeSet::from([GROUP_KIND])),
                    ..Filter::default()
                })
                .expect("a group read opens"),
            None,
        )
        .expect("a NIP-29 read is an ordinary live query");

    let receipts = engine
        .publish(WriteIntent {
            payload: WritePayload::Signed(event),
            routing: WriteRouting::Explicit(vec![accepting_url.clone(), rejecting_url.clone()]),
            identity: Identity::Explicit(me.public_key()),
            correlation: None,
        })
        .expect("an already-signed event is accepted by the one publish door")
        .statuses;

    // Phase 1 -- nothing is reachable. The row is on screen anyway, claiming
    // the cache and zero relays.
    let mut observed = Observed::default();
    wait_for_rows(&subscription, SETTLE, &mut observed, |rows| {
        rows.contains_key(&known_id)
    });
    assert_eq!(
        observed.first_sources.get(&known_id),
        Some(&BTreeSet::new()),
        "the row's FIRST appearance must claim the cache and zero relays"
    );

    // Phase 2 -- the hosts come up on the exact ports the write is already
    // aimed at. One takes the event, one refuses it.
    let accepting =
        ScriptedRelay::start_on_port(port_of(&accepting_url), &RelayConfig::default()).await;
    let rejecting = ScriptedRelay::start_on_port(
        port_of(&rejecting_url),
        &RelayConfig {
            reject_writes: Some("blocked: this host refuses every event".to_string()),
            ..RelayConfig::default()
        },
    )
    .await;
    assert_eq!(
        accepting.url, accepting_url,
        "the host came up where the write is aimed"
    );
    assert_eq!(
        rejecting.url, rejecting_url,
        "the host came up where the write is aimed"
    );

    // Partial publication is routine, and both halves are ordinary per-relay
    // receipt facts on ONE receipt. Wait for BOTH -- they race, and stopping
    // at whichever lands first would make this assertion about scheduling.
    let mut acked = false;
    let mut rejected = false;
    let statuses = drain_until(&receipts, RETRY_SETTLE, |status| {
        match status {
            WriteFact::Relay {
                relay,
                state: RelayState::Published,
            } if relay == &accepting_url => acked = true,
            WriteFact::Relay {
                relay,
                state: RelayState::Rejected { reason: _ },
            } if relay == &rejecting_url => rejected = true,
            _ => {}
        }
        acked && rejected
    });
    assert!(
        acked && rejected,
        "one host must ACK and the other must refuse: {statuses:?}"
    );

    // Phase 3 -- the accepting host carried it, so it, and only it, enters the
    // row's provenance. The rejecting host never does: it never had the event.
    wait_for_rows(&subscription, RETRY_SETTLE, &mut observed, |rows| {
        rows.get(&known_id)
            .is_some_and(|row| row.sources.contains(&accepting_url))
    });
    settle(&subscription, &mut observed, QUIET);

    let row = &observed.rows[&known_id];
    assert_eq!(
        row.sources,
        BTreeSet::from([accepting_url.clone()]),
        "provenance must name EXACTLY the host that carried the event"
    );
    assert!(
        !row.sources.contains(&rejecting_url),
        "a host that refused the event never carried it and must never appear \
         in its provenance"
    );
    assert_eq!(
        observed.added_counts.get(&known_id).copied(),
        Some(1),
        "the event is ONE row throughout: appearing optimistically and later \
         gaining a relay is a provenance update, never a second Added"
    );

    drop(subscription);
    engine.shutdown();
    accepting.shutdown();
    rejecting.shutdown();
}

// ===========================================================================
// #1182 falsifier 4 -- the SAME claim with no NIP-29 anywhere: an ordinary
// `Engine::publish`, an ordinary pinned cache-strict `Demand`, and two
// ordinary kinds that no group protocol has ever heard of.
//
// This is what makes the mechanism general rather than a NIP-29 courtesy.
// Removing it turns THIS red too, not only the two tests above.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn optimistic_publication_is_general_and_owes_nothing_to_nip29() {
    let me = Keys::generate();
    let host_a = unreachable_host();
    let host_b = unreachable_host();
    let hosts = [host_a.clone(), host_b.clone()];

    let engine = engine();

    for (kind, content) in [
        (TEXT_NOTE_KIND, "an ordinary note, no group in sight"),
        (LONG_FORM_KIND, "an ordinary article, no group in sight"),
    ] {
        let event = signed_event(
            &me,
            kind,
            1_700_000_003,
            vec![Tag::identifier("d-for-the-addressable-case")],
            content,
        );
        let known_id = event.id;

        let query = LiveQuery::union(
            hosts.iter().map(|host| {
                LiveQuery::single(pinned_strict_branch(std::slice::from_ref(host), kind))
            }),
            None,
        )
        .expect("a two-branch pinned union is constructible");

        let subscription = engine
            .observe(query, None)
            .expect("an ordinary pinned live query opens");

        // Acceptance is this call returning `Ok`, for every kind alike.
        let _receipts = engine
            .publish(WriteIntent {
                payload: WritePayload::Signed(event),
                routing: WriteRouting::Explicit(hosts.to_vec()),
                identity: Identity::Explicit(me.public_key()),
                correlation: None,
            })
            .unwrap_or_else(|error| {
                panic!("kind:{kind} -- an ordinary publish is accepted: {error}")
            });

        let mut observed = Observed::default();
        wait_for_rows(&subscription, SETTLE, &mut observed, |rows| {
            rows.contains_key(&known_id)
        });
        assert_eq!(
            observed.rows[&known_id].sources,
            BTreeSet::new(),
            "kind:{kind} -- a locally accepted write reports the cache and zero \
             relays, whatever protocol it belongs to"
        );

        drop(subscription);
    }

    engine.shutdown();
}
