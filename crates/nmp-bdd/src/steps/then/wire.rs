//! Assertions about the REQ/CLOSE frames NMP actually put on a relay socket
//! -- a FIFTH observable channel, added for
//! `features/routing/subscription-collapse.feature`.
//!
//! It is deliberately NOT read out of engine diagnostics: the contract here is
//! "what did the relay receive", and a spec about wire economy must not take
//! the thing under test as its witness. The record comes from the world's own
//! scripted relay (`relays::WireRecord`) instead.
//!
//! Every step below settles the wire first (`wire_settled`; see the world's
//! `WIRE_QUIET`): each of these assertions is a COUNT, and a count read while
//! demand is still recompiling measures when it was taken, not the plan.
//!
//! The claims come in two axes -- tag values and authors -- and that pairing
//! is the point: the author axis already aggregated before any of this
//! existed, so it is the control every tag-axis claim is measured against.
//! Both axes are here rather than in two files precisely so the pairs stay
//! adjacent and a rule that fixes one while breaking the other is visible.

use std::collections::BTreeSet;

use cucumber::then;

use crate::steps::parse_tag;
use crate::world::NmpWorld;
use nmp_test_support::relays::{WireRecord, WireReq};

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
    nothing_to_observe!(
        !record.reqs.is_empty(),
        "relay {relay:?} received no REQ at all, so it is serving the #{tag} watches \
         with no subscriptions for want of an engine, not for want of a merge"
    );
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
    nothing_to_observe!(
        !ids.is_empty(),
        "relay {relay:?} is holding no live #{tag} subscription at all, and nothing is \
         comfortably under any bound"
    );
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
    nothing_to_observe!(
        !record.reqs_naming_tag(tag).is_empty() && !record.reqs_naming_authors().is_empty(),
        "relay {relay:?} received {} REQ(s) naming #{tag} and {} naming authors -- both \
         axes must actually reach the wire before their non-merger means anything",
        record.reqs_naming_tag(tag).len(),
        record.reqs_naming_authors().len()
    );
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
    nothing_to_observe!(!wanted.is_empty(), "no #{tag} value is being watched");
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
    nothing_to_observe!(
        !wanted.is_empty(),
        "no #{tag} value is being watched, so every one of them is covered vacuously"
    );
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
    let ids = record.live_subscription_ids_naming_tag(tag);
    nothing_to_observe!(
        !ids.is_empty(),
        "relay {relay:?} is holding no live #{tag} subscription at all, so none of them \
         carries too many values"
    );
    for id in ids {
        let carried = live_tag_values(&record, &id, tag).len();
        assert!(
            carried <= bound,
            "a subscription on {relay:?} carries {carried} #{tag} values, over the bound of {bound}"
        );
    }
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
    let ids = record.subscription_ids_naming_tag(tag);
    nothing_to_observe!(
        !ids.is_empty(),
        "relay {relay:?} never had a #{tag} subscription opened on it at all, so none \
         could have been retired instead of replaced"
    );
    let closed = closes_among(&record, &ids);
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
    let ids = record.subscription_ids_naming_authors();
    nothing_to_observe!(
        !ids.is_empty(),
        "relay {relay:?} never had an author subscription opened on it at all, so none \
         could have been retired instead of replaced"
    );
    let closed = closes_among(&record, &ids);
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
    nothing_to_observe!(
        !record.reqs.is_empty(),
        "relay {relay:?} received no REQ at all, so nothing was ever opened that could \
         be stopped and reopened"
    );
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
    nothing_to_observe!(
        !record.reqs.is_empty(),
        "relay {relay:?} received no REQ at all, so nothing could have been re-sent"
    );
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
    nothing_to_observe!(
        !record.reqs_naming_tag(left).is_empty() && !record.reqs_naming_tag(right).is_empty(),
        "relay {relay:?} received {} REQ(s) naming #{left} and {} naming #{right} -- both \
         tag names must actually reach the wire before their non-merger means anything",
        record.reqs_naming_tag(left).len(),
        record.reqs_naming_tag(right).len()
    );
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
    nothing_to_observe!(
        !record.reqs.is_empty(),
        "relay {relay:?} received no REQ at all, so no resolved set could have widened \
         into 'send me everything'"
    );
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
    nothing_to_observe!(
        !record.reqs.is_empty(),
        "relay {relay:?} received no REQ at all, so it is serving the author watches \
         with no subscriptions for want of an engine, not for want of a merge"
    );
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
    nothing_to_observe!(!wanted.is_empty(), "no author is being watched");
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

/// The bounded feed's window survived the per-relay join intact (#937).
///
/// Asserts on EVERY live request rather than on their sum: the failure this
/// guards against is a filter that kept a per-author page after the authors
/// were joined, and one such filter is enough. An UNBOUNDED request fails
/// here too, deliberately -- dropping the `limit` during the join would
/// substitute the relay's own undocumented default and make under-fetch
/// unobservable, which is a worse outcome than fetching too much.
#[then(regex = r#"^every request on relay "([^"]+)" asks for at most (\d+) notes$"#)]
async fn every_request_asks_for_at_most(w: &mut NmpWorld, relay: String, most: u64) {
    w.wire_settled().await;
    let record = w.wire_record(&relay);
    let live = record.live_subscription_ids();
    nothing_to_observe!(
        !live.is_empty(),
        "relay {relay:?} is holding no live subscription to check"
    );
    for id in &live {
        let req = record
            .latest_req_on(id)
            .unwrap_or_else(|| panic!("live subscription {id:?} has no REQ on {relay:?}"));
        match req.max_limit() {
            Some(limit) => assert!(
                limit <= most,
                "subscription {id:?} on {relay:?} asks for {limit} notes, more than the {most} \
                 the feed asked for -- the window was multiplied during the per-relay join"
            ),
            None => panic!(
                "subscription {id:?} on {relay:?} carries no `limit` at all; the feed asked for \
                 at most {most}, and an unbounded filter makes under-fetch unobservable"
            ),
        }
    }
}

#[then(regex = r#"^every author I watch is covered by some subscription on relay "([^"]+)"$"#)]
async fn every_author_is_covered(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let wanted = w.watched_authors();
    nothing_to_observe!(
        !wanted.is_empty(),
        "no author is being watched, so every one of them is covered vacuously"
    );
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
