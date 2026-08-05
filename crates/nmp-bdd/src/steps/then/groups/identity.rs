//! What a group promises as an IDENTITY: construction costs nothing, a write needs no read, and one value serves a whole room.

use crate::world::acquisition::{branch_shortfall, branch_sources};
use cucumber::then;

use super::publish_queue::published_delivered_to;

use super::*;

// ---- the identity: construction is free, reads are optional --------------

#[then(regex = r#"^no relay received a connection$"#)]
async fn no_relay_received_a_connection(w: &mut NmpWorld) {
    w.wire_settled().await;
    let contacted: Vec<String> = w
        .relay_names()
        .filter(|name| w.relay_contacted(name))
        .cloned()
        .collect();
    assert!(
        contacted.is_empty(),
        "constructing a group contacts nothing, but {contacted:?} were contacted"
    );
}

#[then(regex = r#"^no query was sent to "([^"]+)"$"#)]
async fn no_query_was_sent_to(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let reqs = w.wire_record(&relay).reqs;
    assert!(
        reqs.is_empty(),
        "expected no REQ at {relay:?}, saw {} of them",
        reqs.len()
    );
}

#[then(
    regex = r#"^(?:no subscription exists|no subscription existed at any point during that publication)$"#
)]
async fn no_subscription_exists(w: &mut NmpWorld) {
    w.wire_settled().await;
    let host = w.group_host_name(None);
    let reqs = w.wire_record(&host).reqs;
    assert!(
        reqs.is_empty(),
        "this scenario publishes with no read at all, but {} REQ(s) reached {host:?}",
        reqs.len()
    );
}

#[then(regex = r#"^the publication did not require a read to succeed first$"#)]
async fn publication_needed_no_read(w: &mut NmpWorld) {
    let host = w.group_host_name(None);
    published_delivered_to(w, host).await;
}

#[then(regex = r#"^the query reports the refused read as a source fact$"#)]
async fn query_reports_the_refusal(w: &mut NmpWorld) {
    let reported = w.feed_eventually(|_, evidence| {
        branch_sources(evidence).next().is_some()
            && (branch_sources(evidence).any(|source| source.reconciled_through.is_none())
                || branch_shortfall(evidence).next().is_some())
    });
    assert!(
        reported,
        "a host that refused the read must appear as an unproven source, not as silence"
    );
}

#[then(regex = r#"^the query does not report the group as empty$"#)]
async fn query_does_not_claim_empty(w: &mut NmpWorld) {
    query_reports_the_refusal(w).await;
}

#[then(
    regex = r#"^(?:all four operations used the same group instance|the same group instance minted all four)$"#
)]
async fn one_group_instance(w: &mut NmpWorld) {
    assert_eq!(
        w.group_build_count(None),
        1,
        "one room is one group value; it was constructed more than once"
    );
}

#[then(regex = r#"^no group (?:had|needed) to be reconstructed between them$"#)]
async fn no_group_reconstruction(w: &mut NmpWorld) {
    one_group_instance(w).await;
}

#[then(regex = r#"^every one of them named "([^"]+)" without the app supplying it$"#)]
async fn every_one_named_the_host(w: &mut NmpWorld, relay: String) {
    settled(w).await;
    assert_eq!(
        w.group_host_name(None),
        relay,
        "the group's own construction host must be the relay every operation named"
    );
    assert!(
        w.relay_contacted(&relay),
        "NOTHING TO OBSERVE -- {relay:?} was never contacted, so nothing named it"
    );
    assert!(
        !w.group_call().named_relay,
        "the app must not have supplied that relay"
    );
}
