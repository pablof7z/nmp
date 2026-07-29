//! Where a group write was routed, and what its receipt said about it.

use cucumber::then;

use super::*;

// ---- the route: minted by the group, spelled by nobody --------------------

#[then(regex = r#"^the write's routing is explicit over exactly "([^"]+)"$"#)]
async fn routing_is_explicit_over(w: &mut NmpWorld, relay: String) {
    let url = w.relay_url(&relay);
    let routed = w.receipt_eventually(|seen| {
        seen.iter().any(
            |s| matches!(s, WriteStatus::Routed { relays, .. } if relays.len() == 1 && relays.contains(&url)),
        )
    });
    assert!(
        routed,
        "expected the write to resolve to exactly {relay:?}; receipt showed {:?}",
        w.receipt_statuses()
    );
    assert!(
        !w.last_publish_named_no_relay(),
        "a group write is an explicit route, never Auto"
    );
}

#[then(regex = r#"^the group minted that routing from the host it was constructed with$"#)]
async fn group_minted_the_routing(w: &mut NmpWorld) {
    let host = w.group_host_name(None);
    let url = w.relay_url(&host);
    let routed = w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Routed { relays, .. } if relays.contains(&url)))
    });
    assert!(
        routed,
        "the resolved route must be the group's own construction host {host:?}; \
         receipt showed {:?}",
        w.receipt_statuses()
    );
}

#[then(
    regex = r#"^(?:the app contributed no relay to that routing|the app supplied no relay anywhere in that run|I named no relay(?: and no tag)? on that call)$"#
)]
async fn the_app_named_no_relay(w: &mut NmpWorld) {
    let call = w.group_call();
    assert!(
        !call.named_relay,
        "the step under test handed the group door a relay; no group operation accepts one"
    );
    let surface = w.group_surface();
    assert_no_parameter(&surface, &["RelayUrl", "relay"], "a relay");
}

#[then(regex = r#"^I named no tag name on that call$"#)]
async fn i_named_no_tag(w: &mut NmpWorld) {
    assert!(
        !w.group_call().named_tag,
        "the step under test named a tag; NIP-29's own operations name their own schema"
    );
}

#[then(regex = r#"^I named no kind number on that call$"#)]
async fn i_named_no_kind(w: &mut NmpWorld) {
    assert!(
        !w.group_call().named_kind,
        "the step under test named a kind number; a named operation carries its own"
    );
}

#[then(regex = r#"^the write was not re-routed to any other relay$"#)]
pub(super) async fn not_rerouted(w: &mut NmpWorld) {
    settled(w).await;
    let host_name = w.group_host_name(None);
    let host = w.relay_url(&host_name);
    let elsewhere: Vec<String> = w
        .receipt_statuses()
        .iter()
        .filter_map(|s| match s {
            WriteStatus::Routed { relays, .. } => Some(relays.clone()),
            _ => None,
        })
        .flatten()
        .filter(|url| *url != host)
        .map(|url| url.to_string())
        .collect();
    assert!(
        elsewhere.is_empty(),
        "an Explicit route has no widen path, but the receipt named {elsewhere:?}"
    );
}

#[then(regex = r#"^(?:no other relay was tried|the operation was not retried anywhere else)$"#)]
async fn nothing_tried_elsewhere(w: &mut NmpWorld) {
    not_rerouted(w).await;
}

#[then(
    regex = r#"^(?:the write consulted no relay list of mine|the write never waited on a relay list|no relay list of mine was read for that write)$"#
)]
async fn no_relay_list_consulted(w: &mut NmpWorld) {
    settled(w).await;
    let mine: Vec<String> = w
        .write_relay_of(crate::world::ME)
        .into_iter()
        .filter(|relay| w.relay_contacted(relay))
        .collect();
    assert!(
        mine.is_empty(),
        "a group write resolves without the directory, but my own write relays {mine:?} \
         were contacted"
    );
    assert!(
        !w.last_publish_named_no_relay(),
        "an Auto route is the one that WOULD consult a relay list; this write must not be one"
    );
}

#[then(regex = r#"^the write was not reported as unroutable$"#)]
async fn not_reported_unroutable(w: &mut NmpWorld) {
    let statuses = w.receipt_statuses();
    let failed: Vec<&String> = statuses
        .iter()
        .filter_map(|s| match s {
            WriteStatus::Failed(reason) => Some(reason),
            _ => None,
        })
        .collect();
    assert!(
        failed.is_empty(),
        "an explicit route needs no relay list to resolve, but the write failed: {failed:?}"
    );
}

#[then(regex = r#"^diagnostics show the write on an explicit route$"#)]
async fn diagnostics_show_explicit_route(w: &mut NmpWorld) {
    let host = w.group_host_name(None);
    routing_is_explicit_over(w, host).await;
}

#[then(regex = r#"^diagnostics show no outbox resolution for that write$"#)]
async fn diagnostics_show_no_outbox(w: &mut NmpWorld) {
    no_relay_list_consulted(w).await;
}

#[then(regex = r#"^a group write cannot be redirected to a relay other than its host$"#)]
async fn cannot_be_redirected(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert_no_parameter(&surface, &["RelayUrl", "WriteRouting", "relay"], "a relay");
}

// ---- the receipt ---------------------------------------------------------

#[then(regex = r#"^the receipt reports the event acked by "([^"]+)"$"#)]
async fn receipt_acked_by(w: &mut NmpWorld, relay: String) {
    let url = w.relay_url(&relay);
    assert!(
        w.receipt_eventually(|seen| seen
            .iter()
            .any(|s| matches!(s, WriteStatus::Acked(u) if *u == url))),
        "expected the receipt to report acked by {relay:?}; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the receipt reports the event rejected by "([^"]+)"$"#)]
async fn receipt_rejected_by(w: &mut NmpWorld, relay: String) {
    let url = w.relay_url(&relay);
    assert!(
        w.receipt_eventually(|seen| seen
            .iter()
            .any(|s| matches!(s, WriteStatus::Rejected(u, _) if *u == url))),
        "expected the receipt to report rejected by {relay:?}; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the receipt names no relay other than "([^"]+)"$"#)]
async fn receipt_names_only(w: &mut NmpWorld, relay: String) {
    settled(w).await;
    let url = w.relay_url(&relay);
    let others: Vec<String> = w
        .receipt_statuses()
        .iter()
        .flat_map(named_relays)
        .filter(|named| *named != url)
        .map(|named| named.to_string())
        .collect();
    assert!(
        others.is_empty(),
        "the receipt of a group write names only its host, but also named {others:?}"
    );
}

/// Every relay a receipt status names. Written as an explicit match rather
/// than a `Debug` string scan so a new `WriteStatus` variant carrying a relay
/// cannot slip past this assertion silently.
fn named_relays(status: &WriteStatus) -> Vec<nostr::RelayUrl> {
    match status {
        WriteStatus::Routed { relays, .. } => relays.iter().cloned().collect(),
        WriteStatus::AwaitingRelay { relay }
        | WriteStatus::AwaitingAuth { relay }
        | WriteStatus::RetryEligible { relay, .. }
        | WriteStatus::HandoffAmbiguous { relay, .. }
        | WriteStatus::Sent { relay, .. }
        | WriteStatus::Acked(relay)
        | WriteStatus::Rejected(relay, _)
        | WriteStatus::GaveUp(relay)
        | WriteStatus::PersistenceBlocked(relay)
        | WriteStatus::RoutePersistenceBlocked(relay)
        | WriteStatus::OutcomeUnknown(relay) => vec![relay.clone()],
        WriteStatus::Accepted
        | WriteStatus::Cancelled
        // A retired obligation names no relay: supersession happens at
        // acceptance, before any route is resolved, and is precisely the
        // case where nothing was ever sent anywhere.
        | WriteStatus::Superseded
        | WriteStatus::AwaitingCapability { .. }
        // A routing park names an AUTHOR it is waiting on, never a relay:
        // the whole point of the state is that no destination exists yet.
        | WriteStatus::AwaitingRoute { .. }
        | WriteStatus::Signed(_)
        | WriteStatus::ReplaceableConflict { .. }
        | WriteStatus::Failed(_) => Vec::new(),
    }
}

#[then(regex = r#"^the receipt carries the host's own rejection message$"#)]
async fn receipt_carries_host_message(w: &mut NmpWorld) {
    let messages = rejection_messages(w);
    assert!(
        messages.iter().any(|message| !message.trim().is_empty()),
        "expected the host's own words on the receipt, got {messages:?}"
    );
}

#[then(regex = r#"^the receipt carries the message "([^"]+)"$"#)]
async fn receipt_carries_message(w: &mut NmpWorld, expected: String) {
    let messages = rejection_messages(w);
    assert!(
        messages.iter().any(|message| message == &expected),
        "expected the receipt to carry {expected:?} verbatim, got {messages:?}"
    );
}

fn rejection_messages(w: &mut NmpWorld) -> Vec<String> {
    w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Rejected(_, _)))
    });
    w.receipt_statuses()
        .iter()
        .filter_map(|s| match s {
            WriteStatus::Rejected(_, message) => Some(message.clone()),
            _ => None,
        })
        .collect()
}

#[then(regex = r#"^(?:the removal is never reported as accepted)$"#)]
async fn removal_never_accepted(w: &mut NmpWorld) {
    settled(w).await;
    assert!(
        !w.receipt_statuses()
            .iter()
            .any(|s| matches!(s, WriteStatus::Acked(_))),
        "a refused moderation action must never report a host ack; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the failure is reported as a rejection by the host$"#)]
async fn failure_is_a_host_rejection(w: &mut NmpWorld) {
    let statuses = {
        rejection_messages(w);
        w.receipt_statuses()
    };
    assert!(
        statuses
            .iter()
            .any(|s| matches!(s, WriteStatus::Rejected(_, _))),
        "expected a per-relay Rejected, saw {statuses:?}"
    );
}

#[then(regex = r#"^the failure is not reported as a routing failure$"#)]
async fn failure_is_not_a_routing_failure(w: &mut NmpWorld) {
    not_reported_unroutable(w).await;
}

#[then(regex = r#"^NMP made no claim of its own about my permissions in the group$"#)]
async fn no_permission_claim_of_its_own(w: &mut NmpWorld) {
    let invented: Vec<String> = w
        .receipt_statuses()
        .iter()
        .filter_map(|s| match s {
            WriteStatus::Failed(reason) => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert!(
        invented.is_empty(),
        "membership is the host's to decide; NMP said {invented:?} on its own account"
    );
}

#[then(regex = r#"^a receipt exists for it addressed by its event id$"#)]
async fn receipt_addressed_by_event_id(w: &mut NmpWorld) {
    let event = delivered(w).await;
    let signed = w.published_event_id();
    assert_eq!(
        signed,
        Some(event.id),
        "the receipt's frozen id must be the id that reached the host"
    );
}

#[then(regex = r#"^the receipt is addressed by the same id "([^"]+)"$"#)]
async fn receipt_addressed_by_label(w: &mut NmpWorld, label: String) {
    let expected = w.labelled_id(&label);
    assert_eq!(
        w.published_event_id(),
        Some(expected),
        "a rejected pre-signed publication keeps the id it was signed with"
    );
}
