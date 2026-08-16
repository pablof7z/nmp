//! Assertions about the per-relay subscription budget (#931): what a relay
//! says it can hold, how much it is actually being asked to hold, and what
//! happened to demand that did not fit.
//!
//! Its own family rather than part of `wire` because the subject is the
//! RELAY'S OWN DECLARED LIMITS and NMP's response to them, not the shape of
//! the requests. That makes three witnesses load-bearing at once and none of
//! them interchangeable: the NIP-11 document the relay served (read back
//! through diagnostics), the live subscription count on the socket, and the
//! acquisition evidence the affected WATCH was given. The last one is the
//! whole point -- refusing demand without telling the app is precisely the
//! silent truncation these scenarios exist to forbid -- so a "nothing was
//! refused" claim checks all three rather than the operator-facing count
//! alone.

use cucumber::then;

use crate::world::NmpWorld;

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
    nothing_to_observe!(
        !record.reqs.is_empty(),
        "relay {relay:?} received no REQ at all, so it holds nothing for want of an \
         engine rather than for want of a merge"
    );
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
    nothing_to_observe!(
        !live.is_empty(),
        "relay {relay:?} is holding no live subscription at all, and nothing is under \
         every bound"
    );
    assert!(
        live.len() <= bound,
        "{relay:?} is holding {} live subscriptions, over the {bound} it allows: {live:?}",
        live.len()
    );
}

#[then(regex = r#"^nothing I asked for was refused for want of a subscription$"#)]
async fn nothing_refused_for_want_of_a_subscription(w: &mut NmpWorld) {
    w.wire_settled().await;
    // The non-emptiness is POLLED (in the predicate), not read off whatever
    // snapshot happens to arrive first: the earliest snapshot legitimately
    // predates every relay row, and a one-shot read of it would report "there
    // was nothing to observe" about a scenario that was merely early.
    let snapshot = w.diagnostics_matching(|snap| !snap.relays.is_empty());
    nothing_to_observe!(
        snapshot.is_some(),
        "diagnostics never knew of a single relay, so nothing was ever asked and \
         nothing could have been refused"
    );
    let snapshot = snapshot.expect("checked just above");
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
