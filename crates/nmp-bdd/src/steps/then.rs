//! `Then` — an observable outcome, always one of the four channels
//! (approach doc §1.3): rows on a feed, receipt states, diagnostics facts,
//! acquisition-evidence facts. Every assertion below reads ONLY through
//! `NmpWorld`'s public observers (`feed_*`/`receipt_*`/`diagnostics_*`/
//! `relay_contacted`/`relay_untouched_since_snapshot`) -- never anything
//! engine-internal.

use std::collections::BTreeSet;

use cucumber::then;

use nmp_engine::outbox::WriteStatus;

use crate::steps::{parse_people, parse_tag};
use crate::world::NmpWorld;
use nmp_test_support::relays::{WireRecord, WireReq};

/// Parse the kind numbers a diagnostics filter's exact wire JSON asks for
/// (`RelayDiagnosticsSnapshot::filters`/`FilterCoverageEntry::filter` are
/// rendered as `ConcreteFilter::to_nostr().as_json()` -- see that module's
/// doc), by round-tripping it back through the pinned `nostr` crate's own
/// `Filter` type. The diagnostics-only, non-internal way to ask "what kind
/// is this wire filter for".
fn filter_kinds(json: &str) -> Vec<u16> {
    use nostr::JsonUtil;
    nostr::Filter::from_json(json)
        .ok()
        .and_then(|f| f.kinds)
        .map(|ks| ks.into_iter().map(|k| k.as_u16()).collect())
        .unwrap_or_default()
}

/// The default discovery-kind set (`nmp_router::DiscoveryKinds::default`,
/// re-derived here rather than depending on that crate's internal type just
/// for this one check): kind:0, kind:3, and the whole NIP-01 REPLACEABLE
/// range 10000..=19999.
fn is_discovery_kind(k: u16) -> bool {
    k == 0 || k == 3 || (10_000..=19_999).contains(&k)
}

fn parse_relay_list_tail(tail: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = tail.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if let Some(len) = tail[i + 1..].find('"') {
                out.push(tail[i + 1..i + 1 + len].to_string());
                i += 1 + len + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[then(regex = r#"^my feed shows (\S+)'s notes$"#)]
async fn feed_shows_persons_notes(w: &mut NmpWorld, person: String) {
    let pk = w.pubkey_hex(&person);
    let shown = w.feed_eventually(|rows, _| rows.iter().any(|e| e.pubkey.to_hex() == pk));
    assert!(
        shown,
        "expected my feed to eventually show {person}'s notes"
    );
}

#[then(regex = r#"^my feed shows the note saying "([^"]+)"$"#)]
async fn feed_shows_note_text(w: &mut NmpWorld, text: String) {
    let shown = w.feed_eventually(|rows, _| rows.iter().any(|e| e.content == text));
    assert!(
        shown,
        "expected my feed to eventually show a note saying {text:?}"
    );
}

#[then(regex = r#"^notes from (\S+) no longer arrive$"#)]
async fn notes_no_longer_arrive(w: &mut NmpWorld, person: String) {
    let pk = w.pubkey_hex(&person);
    let pk_for_gone = pk.clone();
    let gone = w.feed_eventually(|rows, _| !rows.iter().any(|e| e.pubkey.to_hex() == pk_for_gone));
    assert!(
        gone,
        "expected {person}'s notes to eventually disappear from my feed"
    );
    let stays_gone = w.feed_never(|rows| rows.iter().any(|e| e.pubkey.to_hex() == pk));
    assert!(
        stays_gone,
        "expected {person}'s notes to never reappear in my feed"
    );
}

#[then(regex = r#"^my feed is empty$"#)]
async fn feed_is_empty(w: &mut NmpWorld) {
    let stays_empty = w.feed_never(|rows| !rows.is_empty());
    assert!(stays_empty, "expected my feed to stay empty");
}

#[then(regex = r#"^the query does not claim its empty result is complete$"#)]
async fn empty_result_is_not_claimed_complete(w: &mut NmpWorld) {
    // #49: there is no `Unknown` verdict and no authoritative-empty claim to
    // read. An empty feed is honest only while a planned source is still
    // unproven -- at least one source carries no `reconciled_through`
    // watermark (or the subtree surfaces a shortfall), so nothing presents
    // the emptiness as complete. The absence of any aggregate/`isComplete`
    // field is itself structural (there is no such surface to assert on).
    let not_claimed_complete = w.feed_eventually(|rows, evidence| {
        rows.is_empty()
            && (evidence
                .sources
                .iter()
                .any(|s| s.reconciled_through.is_none())
                || !evidence.shortfall.is_empty())
    });
    assert!(
        not_claimed_complete,
        "expected the empty feed to carry an unproven planned source \
         (no authoritative-empty / global-complete claim)"
    );
}

#[then(regex = r#"^the subscriptions serving (.+) are untouched$"#)]
async fn subscriptions_untouched(w: &mut NmpWorld, list: String) {
    for person in parse_people(&list) {
        let relays = w.write_relay_of(&person);
        assert!(
            !relays.is_empty(),
            "{person} has no declared write relay to check for untouched-ness"
        );
        for relay in relays {
            assert!(
                w.relay_untouched_since_snapshot(&relay),
                "expected {person}'s relay {relay:?} to receive no new REQ/EVENT"
            );
        }
    }
}

#[then(regex = r#"^the indexers are asked only for relay lists and profiles$"#)]
async fn indexers_discovery_only(w: &mut NmpWorld) {
    // "relay lists and profiles" is this scenario's plain-language gloss of
    // the structural invariant actually being asserted: an indexer relay
    // (`Lane::IndexerDiscovery`) may carry kind:0/3/1xxxx (relay lists,
    // profiles, contact lists, mute lists, ...) but NEVER a content atom
    // (kind:1) -- see `nmp_router::DiscoveryKinds`'s doc ("indexers are
    // never a content fallback").
    let names: Vec<String> = w.indexer_names().to_vec();
    let urls: Vec<_> = names.iter().map(|n| w.relay_url(n)).collect();
    let snapshot = w
        .diagnostics_matching(|snap| {
            urls.iter()
                .any(|u| snap.relays.iter().any(|r| &r.relay == u))
        })
        .expect("diagnostics never showed any indexer activity at all");
    for (name, url) in names.iter().zip(urls.iter()) {
        let Some(relay_diag) = snapshot.relays.iter().find(|r| &r.relay == url) else {
            // This particular indexer was never contacted -- trivially
            // discovery-only (nothing was ever asked of it).
            continue;
        };
        for filter_json in &relay_diag.filters {
            for kind in filter_kinds(filter_json) {
                assert!(
                    is_discovery_kind(kind),
                    "indexer {name:?} carries a non-discovery filter (kind {kind}): {filter_json}"
                );
            }
        }
    }
}

#[then(regex = r#"^(\S+)'s notes arrive from "([^"]+)"$"#)]
async fn persons_notes_arrive_from(w: &mut NmpWorld, person: String, relay_name: String) {
    let relay_url = w.relay_url(&relay_name);
    let arrived = w
        .diagnostics_matching(|snap| {
            snap.relays.iter().any(|r| {
                r.relay == relay_url && r.events_by_kind.iter().any(|(k, n)| *k == 1 && *n > 0)
            })
        })
        .is_some();
    assert!(
        arrived,
        "expected {person}'s notes (kind 1) to have been received from {relay_name:?}"
    );
}

#[then(regex = r#"^no relay outside the indexers(.*) was ever contacted$"#)]
async fn no_relay_outside_the_plan(w: &mut NmpWorld, extra: String) {
    let mut allowed: Vec<String> = w.indexer_names().to_vec();
    allowed.extend(parse_relay_list_tail(&extra));
    for relay in w.relay_names().cloned().collect::<Vec<_>>() {
        if allowed.contains(&relay) {
            continue;
        }
        assert!(
            !w.relay_contacted(&relay),
            "relay {relay:?} is outside the routing plan but was contacted"
        );
    }
}

#[then(regex = r#"^relay "([^"]+)" received no connection at all$"#)]
async fn relay_received_no_connection(w: &mut NmpWorld, name: String) {
    assert!(
        !w.relay_contacted(&name),
        "expected relay {name:?} to never be contacted"
    );
}

#[then(regex = r#"^every contacted relay appears in the diagnostics with its routing lane$"#)]
async fn every_contacted_relay_has_a_lane(w: &mut NmpWorld) {
    let contacted: Vec<String> = w
        .relay_names()
        .filter(|name| w.relay_contacted(name))
        .cloned()
        .collect();
    let urls: Vec<_> = contacted.iter().map(|n| w.relay_url(n)).collect();
    let snapshot = w
        .diagnostics_matching(|snap| {
            urls.iter().all(|u| {
                snap.relays
                    .iter()
                    .any(|r| &r.relay == u && !r.by_lane.is_empty())
            })
        })
        .expect("diagnostics never agreed that every contacted relay has an assigned lane");
    for (name, url) in contacted.iter().zip(urls.iter()) {
        let has_lane = snapshot
            .relays
            .iter()
            .any(|r| &r.relay == url && !r.by_lane.is_empty());
        assert!(
            has_lane,
            "relay {name:?} was contacted but has no lane in the diagnostics snapshot"
        );
    }
}

#[then(regex = r#"^the receipt first reports only accepted -- never sent$"#)]
async fn receipt_first_accepted(w: &mut NmpWorld) {
    let has_any = w.receipt_eventually(|seen| !seen.is_empty());
    assert!(has_any, "expected at least one receipt status");
    let first_is_accepted =
        w.receipt_eventually(|seen| matches!(seen.first(), Some(WriteStatus::Accepted)));
    assert!(
        first_is_accepted,
        "expected the receipt's FIRST status to be Accepted, never a converged Sent"
    );
}

#[then(regex = r#"^the receipt reports the note acked by "([^"]+)"$"#)]
async fn receipt_acked_by(w: &mut NmpWorld, relay_name: String) {
    let relay_url = w.relay_url(&relay_name);
    let acked = w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Acked(url) if *url == relay_url))
    });
    assert!(
        acked,
        "expected the receipt to report acked by {relay_name:?}"
    );
}

#[then(regex = r#"^the receipt reports the note rejected by "([^"]+)"$"#)]
async fn receipt_rejected_by(w: &mut NmpWorld, relay_name: String) {
    let relay_url = w.relay_url(&relay_name);
    let rejected = w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Rejected(url, _) if *url == relay_url))
    });
    assert!(
        rejected,
        "expected the receipt to report rejected by {relay_name:?}"
    );
}

// ---- wire subscription aggregation --------------------------------------
//
// A FIFTH observable channel, added for
// `features/routing/subscription-collapse.feature`: the REQ/CLOSE frames NMP
// actually put on a relay socket, decoded by the world's own scripted relay
// (`relays::WireRecord`). It is deliberately NOT read out of engine
// diagnostics -- the contract here is "what did the relay receive", and a
// spec about wire economy must not take the thing under test as its witness.
//
// Every step below settles the wire first (`wire_settled`; see
// `world::WIRE_QUIET`): each of these assertions is a COUNT, and a count read
// while demand is still recompiling measures when it was taken, not the plan.

/// The values `sub_id`'s CURRENT filter asks for under `tag`. NIP-01
/// replacement means only a subscription's most recent REQ is live, so a
/// value that was once asked for and has since been replaced away is
/// correctly not counted.
fn live_tag_values(record: &WireRecord, sub_id: &str, tag: char) -> BTreeSet<String> {
    record
        .latest_req_on(sub_id)
        .map(|req| req.tag_values(tag))
        .unwrap_or_default()
}

fn live_authors(record: &WireRecord, sub_id: &str) -> BTreeSet<String> {
    record
        .latest_req_on(sub_id)
        .map(WireReq::authors)
        .unwrap_or_default()
}

#[then(regex = r#"^relay "([^"]+)" serves every "([a-zA-Z])" watch with (\d+) subscriptions?$"#)]
async fn relay_serves_tag_with_n_subscriptions(
    w: &mut NmpWorld,
    relay: String,
    tag: String,
    expected: usize,
) {
    w.wire_settled().await;
    let tag = parse_tag(&tag);
    let record = w.wire_record(&relay);
    let ids = record.live_subscription_ids_naming_tag(tag);
    assert_eq!(
        ids.len(),
        expected,
        "relay {relay:?} is serving the #{tag} watches with {} live subscriptions, not \
         {expected}: \
         {} REQs carrying #{tag} arrived, each asking for {:?}",
        ids.len(),
        record.reqs_naming_tag(tag).len(),
        record
            .reqs_naming_tag(tag)
            .iter()
            .map(|r| r.tag_values(tag))
            .collect::<Vec<_>>()
    );
}

/// The BOUNDED form of the count assertion, for the case where an exact
/// number is not a fact about the contract.
///
/// Past the per-filter value bound the coalescer chunks, and how many chunks
/// it produces is an artifact of its greedy merge ORDER, not `⌈n/bound⌉`:
/// mutually-mergeable filters pair up in a doubling cascade, so chunks stall
/// at the largest power of two under the bound. What IS provable is a window
/// -- a terminal state has no mergeable pair left, so every pair of chunks
/// sums over the bound, which means at most one chunk is half-full or less.
/// The contract worth asserting is therefore "comfortably inside the relay's
/// concurrent-subscription ceiling", which is what this step says.
#[then(
    regex = r#"^relay "([^"]+)" serves every "([a-zA-Z])" watch with at most (\d+) subscriptions?$"#
)]
async fn relay_serves_tag_with_at_most_n_subscriptions(
    w: &mut NmpWorld,
    relay: String,
    tag: String,
    bound: usize,
) {
    w.wire_settled().await;
    let tag = parse_tag(&tag);
    let record = w.wire_record(&relay);
    let ids = record.live_subscription_ids_naming_tag(tag);
    assert!(
        ids.len() <= bound,
        "relay {relay:?} is serving the #{tag} watches with {} live subscriptions, \
         over the bound of {bound}: each carries {:?} value(s)",
        ids.len(),
        ids.iter()
            .map(|id| live_tag_values(&record, id, tag).len())
            .collect::<Vec<_>>()
    );
}

/// Across tag names a filter is a CONJUNCTION, and so is a tag alongside an
/// author list: a REQ naming both `#p` and `authors` demands both at once and
/// matches neither original watch. The tag-name twin of this
/// (`never received a request naming both "p" and "t"`) guards the same
/// property within the tag axis; this one guards it ACROSS axes, which is
/// where a rule that unioned two components at a time would break it.
#[then(
    regex = r#"^relay "([^"]+)" never received a request naming both "([a-zA-Z])" and authors$"#
)]
async fn no_request_names_a_tag_and_authors(w: &mut NmpWorld, relay: String, tag: String) {
    w.wire_settled().await;
    let tag = parse_tag(&tag);
    let record = w.wire_record(&relay);
    for req in &record.reqs {
        assert!(
            !(req.names_tag(tag) && !req.authors().is_empty()),
            "a REQ on {relay:?} demands #{tag} AND an author list at once ({:?}); \
             those are two independent selections, and a filter carrying both \
             matches neither original watch",
            req.filters
        );
    }
}

#[then(
    regex = r#"^one subscription on relay "([^"]+)" asks for every "([a-zA-Z])" value I watch$"#
)]
async fn one_subscription_carries_every_tag_value(w: &mut NmpWorld, relay: String, tag: String) {
    w.wire_settled().await;
    let tag = parse_tag(&tag);
    let wanted = w.watched_tag_values(tag);
    assert!(!wanted.is_empty(), "no #{tag} value is being watched");
    let record = w.wire_record(&relay);
    let carried: Vec<BTreeSet<String>> = record
        .live_subscription_ids_naming_tag(tag)
        .iter()
        .map(|id| live_tag_values(&record, id, tag))
        .collect();
    assert!(
        carried.iter().any(|values| values.is_superset(&wanted)),
        "no single subscription on {relay:?} asks for all {} watched #{tag} values; \
         the live subscriptions carry {carried:?}",
        wanted.len()
    );
}

#[then(
    regex = r#"^every "([a-zA-Z])" value I watch is covered by some subscription on relay "([^"]+)"$"#
)]
async fn every_tag_value_is_covered(w: &mut NmpWorld, tag: String, relay: String) {
    w.wire_settled().await;
    let tag = parse_tag(&tag);
    let wanted = w.watched_tag_values(tag);
    let record = w.wire_record(&relay);
    let covered: BTreeSet<String> = record
        .live_subscription_ids()
        .iter()
        .flat_map(|id| live_tag_values(&record, id, tag))
        .collect();
    let missing: Vec<&String> = wanted.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "{} watched #{tag} value(s) reach no live subscription on {relay:?} at all: {missing:?}",
        missing.len()
    );
}

#[then(
    regex = r#"^no subscription on relay "([^"]+)" carries more than (\d+) "([a-zA-Z])" values$"#
)]
async fn no_subscription_exceeds_the_value_bound(
    w: &mut NmpWorld,
    relay: String,
    bound: usize,
    tag: String,
) {
    w.wire_settled().await;
    let tag = parse_tag(&tag);
    let record = w.wire_record(&relay);
    for id in record.live_subscription_ids_naming_tag(tag) {
        let carried = live_tag_values(&record, &id, tag).len();
        assert!(
            carried <= bound,
            "a subscription on {relay:?} carries {carried} #{tag} values, over the bound of {bound}"
        );
    }
}

/// True iff some REQ on one of `ids` re-used an already-live subscription id
/// -- the wire signature of a widen (or shrink) in place.
fn widened_in_place(record: &WireRecord, ids: &[String]) -> bool {
    record
        .reqs
        .iter()
        .any(|req| req.replaces && ids.contains(&req.sub_id))
}

/// Which of `ids` the relay was asked to close. Scoped to the axis under
/// test on purpose: a pinned demand also drives relay-list discovery of its
/// authors, and THAT subscription is legitimately closed once it completes.
/// A relay-wide "no CLOSE" assertion would intermittently catch it and make
/// the author-axis regression guard flaky for a reason that has nothing to do
/// with the contract.
fn closes_among<'a>(record: &'a WireRecord, ids: &[String]) -> Vec<&'a String> {
    record.closes.iter().filter(|id| ids.contains(id)).collect()
}

#[then(regex = r#"^relay "([^"]+)" widened the "([a-zA-Z])" subscription in place$"#)]
async fn relay_widened_tag_in_place(w: &mut NmpWorld, relay: String, tag: String) {
    w.wire_settled().await;
    let tag = parse_tag(&tag);
    let record = w.wire_record(&relay);
    let ids = record.subscription_ids_naming_tag(tag);
    assert!(
        widened_in_place(&record, &ids),
        "no REQ on {relay:?} ever re-used a live #{tag} subscription id -- nothing was \
         widened in place; {} REQs opened {} distinct #{tag} subscriptions",
        record.reqs_naming_tag(tag).len(),
        ids.len()
    );
}

#[then(regex = r#"^relay "([^"]+)" widened the author subscription in place$"#)]
async fn relay_widened_authors_in_place(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    let ids = record.subscription_ids_naming_authors();
    assert!(
        widened_in_place(&record, &ids),
        "no REQ on {relay:?} ever re-used a live author subscription id -- nothing was \
         widened in place; {} REQs opened {} distinct author subscriptions",
        record.reqs_naming_authors().len(),
        ids.len()
    );
}

#[then(regex = r#"^relay "([^"]+)" was never asked to close a "([a-zA-Z])" subscription$"#)]
async fn relay_closed_no_tag_subscription(w: &mut NmpWorld, relay: String, tag: String) {
    w.wire_settled().await;
    let tag = parse_tag(&tag);
    let record = w.wire_record(&relay);
    let closed = closes_among(&record, &record.subscription_ids_naming_tag(tag));
    assert!(
        closed.is_empty(),
        "relay {relay:?} was asked to close {} #{tag} subscription(s) -- growing or \
         shrinking a value set must replace a live subscription, never retire it",
        closed.len()
    );
}

#[then(regex = r#"^relay "([^"]+)" was never asked to close an author subscription$"#)]
async fn relay_closed_no_author_subscription(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    let closed = closes_among(&record, &record.subscription_ids_naming_authors());
    assert!(
        closed.is_empty(),
        "relay {relay:?} was asked to close {} author subscription(s) -- growing or \
         shrinking a value set must replace a live subscription, never retire it",
        closed.len()
    );
}

#[then(regex = r#"^relay "([^"]+)" never revives a request it was told to stop$"#)]
async fn relay_never_revives_a_stopped_request(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    let revived = record.revived_subscription_ids();
    assert!(
        revived.is_empty(),
        "relay {relay:?} was asked to reopen {} request(s) it had already been \
         told to stop ({revived:?}) -- an answer still on its way for the \
         stopped one would then be indistinguishable from an answer to the new \
         one, and the query would claim it holds data that never arrived",
        revived.len()
    );
}

#[then(regex = r#"^relay "([^"]+)" was never asked for the same thing twice$"#)]
async fn relay_never_asked_twice(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    let redundant = record.redundant_reqs();
    assert!(
        redundant.is_empty(),
        "relay {relay:?} received {} REQ(s) that re-sent a live subscription's filter \
         unchanged -- the relay re-runs the query and re-streams its matches for nothing: {:?}",
        redundant.len(),
        redundant.iter().map(|r| &r.filters).collect::<Vec<_>>()
    );
}

#[then(
    regex = r#"^relay "([^"]+)" never received a request naming both "([a-zA-Z])" and "([a-zA-Z])"$"#
)]
async fn no_request_names_both_tags(w: &mut NmpWorld, relay: String, left: String, right: String) {
    w.wire_settled().await;
    let (left, right) = (parse_tag(&left), parse_tag(&right));
    let record = w.wire_record(&relay);
    for req in &record.reqs {
        assert!(
            !(req.names_tag(left) && req.names_tag(right)),
            "a REQ on {relay:?} demands #{left} AND #{right} at once ({:?}); \
             within one tag name a value list is a choice, but ACROSS tag names a \
             filter is a conjunction -- such a merge matches neither original watch",
            req.filters
        );
    }
}

#[then(regex = r#"^relay "([^"]+)" was never asked for everything of a kind$"#)]
async fn relay_never_asked_unfiltered(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    for req in &record.reqs {
        assert!(
            !req.narrows_by_kind_alone(),
            "a REQ on {relay:?} narrows by kind alone -- an empty resolved set widened \
             into 'send me everything': {:?}",
            req.filters
        );
    }
}

#[then(regex = r#"^relay "([^"]+)" serves every author watch with (\d+) subscriptions?$"#)]
async fn relay_serves_authors_with_n_subscriptions(
    w: &mut NmpWorld,
    relay: String,
    expected: usize,
) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    let ids = record.live_subscription_ids_naming_authors();
    assert_eq!(
        ids.len(),
        expected,
        "relay {relay:?} is serving the author watches with {} live subscriptions, not \
         {expected}: \
         {} REQs carrying authors arrived, each asking for {:?}",
        ids.len(),
        record.reqs_naming_authors().len(),
        record
            .reqs_naming_authors()
            .iter()
            .map(|r| r.authors().len())
            .collect::<Vec<_>>()
    );
}

#[then(regex = r#"^one subscription on relay "([^"]+)" asks for every author I watch$"#)]
async fn one_subscription_carries_every_author(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let wanted = w.watched_authors();
    assert!(!wanted.is_empty(), "no author is being watched");
    let record = w.wire_record(&relay);
    let carried: Vec<usize> = record
        .live_subscription_ids()
        .iter()
        .map(|id| live_authors(&record, id).len())
        .collect();
    assert!(
        record
            .live_subscription_ids()
            .iter()
            .any(|id| live_authors(&record, id).is_superset(&wanted)),
        "no single subscription on {relay:?} asks for all {} watched authors; \
         the live subscriptions carry {carried:?} authors each",
        wanted.len()
    );
}

#[then(regex = r#"^every author I watch is covered by some subscription on relay "([^"]+)"$"#)]
async fn every_author_is_covered(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let wanted = w.watched_authors();
    let record = w.wire_record(&relay);
    let covered: BTreeSet<String> = record
        .live_subscription_ids()
        .iter()
        .flat_map(|id| live_authors(&record, id))
        .collect();
    let missing: Vec<&String> = wanted.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "{} watched author(s) reach no live subscription on {relay:?} at all: {missing:?} -- \
         a demand was silently dropped, and nothing ever repairs it",
        missing.len()
    );
}

// ---- the per-relay subscription budget (#931) ---------------------------

/// One relay's row in the latest diagnostics snapshot, once `pred` holds for
/// it. Diagnostics is a polled observable here on purpose: what a relay says
/// about itself arrives over its own HTTP fetch, some time after the first
/// subscription reached its socket.
fn relay_row_matching(
    w: &NmpWorld,
    relay: &str,
    pred: impl Fn(&nmp_engine::core::RelayDiagnosticsSnapshot) -> bool,
) -> Option<nmp_engine::core::RelayDiagnosticsSnapshot> {
    let url = w.relay_url(relay);
    w.diagnostics_matching(|snap| snap.relays.iter().any(|row| row.relay == url && pred(row)))
        .and_then(|snap| snap.relays.into_iter().find(|row| row.relay == url))
}

/// The latest row for `relay`, whatever it says. Used by the negative
/// assertions, which must read a row that EXISTS and find the fact absent
/// rather than mistake "no diagnostics yet" for "nothing was refused".
fn latest_relay_row(w: &NmpWorld, relay: &str) -> nmp_engine::core::RelayDiagnosticsSnapshot {
    relay_row_matching(w, relay, |_| true)
        .unwrap_or_else(|| panic!("diagnostics never showed relay {relay:?} at all"))
}

#[then(regex = r#"^relay "([^"]+)" is known to allow only (\d+) subscriptions? at a time$"#)]
async fn relay_known_to_allow_n(w: &mut NmpWorld, relay: String, expected: usize) {
    w.wire_settled().await;
    let row = relay_row_matching(w, &relay, |row| row.subscription_budget.is_some())
        .unwrap_or_else(|| {
            panic!("nothing was ever learned about how many subscriptions {relay:?} allows")
        });
    assert_eq!(
        row.subscription_budget,
        Some(expected),
        "{relay:?} published a limit of {expected}, but it is known as {:?}",
        row.subscription_budget
    );
}

#[then(regex = r#"^nothing is known about how many subscriptions relay "([^"]+)" allows$"#)]
async fn nothing_known_about_relay_limit(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let row = latest_relay_row(w, &relay);
    assert_eq!(
        row.subscription_budget, None,
        "a relay that published nothing must not be credited with a limit, \
         yet {relay:?} is treated as allowing {:?}",
        row.subscription_budget
    );
}

#[then(regex = r#"^relay "([^"]+)" is holding (\d+) subscriptions?$"#)]
async fn relay_holding_n_subscriptions(w: &mut NmpWorld, relay: String, expected: usize) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    let live = record.live_subscription_ids();
    assert_eq!(
        live.len(),
        expected,
        "{relay:?} is holding {} live subscriptions, not {expected}: {live:?}",
        live.len()
    );
}

#[then(regex = r#"^relay "([^"]+)" is never asked to hold more than (\d+) subscriptions$"#)]
async fn relay_never_over_n_subscriptions(w: &mut NmpWorld, relay: String, bound: usize) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    let live = record.live_subscription_ids();
    assert!(
        live.len() <= bound,
        "{relay:?} is holding {} live subscriptions, over the {bound} it allows: {live:?}",
        live.len()
    );
}

#[then(regex = r#"^nothing I asked for was refused for want of a subscription$"#)]
async fn nothing_refused_for_want_of_a_subscription(w: &mut NmpWorld) {
    w.wire_settled().await;
    let snapshot = w
        .diagnostics_matching(|_| true)
        .expect("diagnostics never arrived at all");
    let refused: Vec<(String, usize)> = snapshot
        .relays
        .iter()
        .filter(|row| row.subscriptions_refused > 0)
        .map(|row| (row.relay.to_string(), row.subscriptions_refused))
        .collect();
    assert!(
        refused.is_empty(),
        "subscriptions were refused for want of budget when none should have been: {refused:?}"
    );
    assert_eq!(
        snapshot.sessions_refused_by_subscription_budget, 0,
        "a whole relay was refused for want of budget"
    );
    assert_eq!(
        w.watches_reporting_a_local_limit(0),
        0,
        "a watch was told its demand was locally limited"
    );
}

#[then(regex = r#"^relay "([^"]+)" refused (\d+) subscriptions? it could not hold$"#)]
async fn relay_refused_n_subscriptions(w: &mut NmpWorld, relay: String, expected: usize) {
    w.wire_settled().await;
    let row = relay_row_matching(w, &relay, |row| row.subscriptions_refused >= expected)
        .unwrap_or_else(|| panic!("diagnostics never showed relay {relay:?} at all"));
    assert_eq!(
        row.subscriptions_refused, expected,
        "{relay:?} reports {} refused subscription(s), not {expected}, while holding {}",
        row.subscriptions_refused, row.wire_sub_count
    );
}

#[then(regex = r#"^(\d+) of my watches (?:is|are) told it could not be requested in full$"#)]
async fn n_watches_told_they_were_limited(w: &mut NmpWorld, expected: usize) {
    w.wire_settled().await;
    let reporting = w.watches_reporting_a_local_limit(expected);
    assert_eq!(
        reporting, expected,
        "{reporting} watch(es) were told their demand could not be requested in full, \
         not {expected} -- demand refused without telling the app is exactly the silent \
         truncation this must never be"
    );
}

#[then(regex = r#"^relay "([^"]+)" is reported as refusing the names NMP gives subscriptions$"#)]
async fn relay_reported_as_rejecting_our_subid_length(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let row = relay_row_matching(w, &relay, |row| row.subid_length_limit.is_some())
        .unwrap_or_else(|| panic!("nothing was ever learned about the names {relay:?} accepts"));
    assert!(
        row.subid_length_rejects_our_ids,
        "{relay:?} accepts names of at most {:?} characters, which is shorter than the \
         64-character names NMP sends, and nothing said so",
        row.subid_length_limit
    );
}

#[then(
    regex = r#"^relay "([^"]+)" is not reported as refusing the names NMP gives subscriptions$"#
)]
async fn relay_not_reported_as_rejecting_our_subid_length(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let row = latest_relay_row(w, &relay);
    assert!(
        !row.subid_length_rejects_our_ids,
        "{relay:?} accepts names of at most {:?} characters, which fits the 64-character \
         names NMP sends, yet it is reported as refusing them",
        row.subid_length_limit
    );
}
