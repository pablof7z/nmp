//! The one read door: an app-chosen selection, pinned and h-scoped.

use crate::world::observe::branch_sources;
use cucumber::then;

use crate::world::parse_kind_list;

use super::*;

// ---- reads: one door, app-chosen kinds, pinned host ----------------------

#[then(regex = r#"^a subscription is returned by the same observe call every other read uses$"#)]
async fn subscription_from_the_one_door(w: &mut NmpWorld) {
    let host = w.group_host_name(None);
    let asked = w
        .wire_record_when(&host, |record| !record.reqs.is_empty())
        .await;
    assert!(
        !asked.reqs.is_empty(),
        "the group's demand must have reached the host through the ordinary read door"
    );
    let surface = w.group_surface();
    assert_no_read_door(&surface);
}

#[then(regex = r#"^(?:the|each) request is pinned to "([^"]+)"$"#)]
async fn request_is_pinned_to(w: &mut NmpWorld, relay: String) {
    let reqs = w
        .wire_record_when(&relay, |record| {
            record.reqs.iter().any(|req| req.names_tag('h'))
        })
        .await;
    assert!(
        reqs.reqs.iter().any(|req| req.names_tag('h')),
        "no group request reached {relay:?} at all"
    );
    let elsewhere: Vec<String> = w
        .relay_names()
        .filter(|name| **name != relay)
        .filter(|name| !w.group_requests(name).is_empty())
        .cloned()
        .collect();
    assert!(
        elsewhere.is_empty(),
        "a pinned group read reaches only its host, but {elsewhere:?} were asked too"
    );
}

#[then(regex = r#"^(?:the|each) request is scoped to h "([^"]+)"$"#)]
async fn request_is_scoped_to_h(w: &mut NmpWorld, group_id: String) {
    let host = w.group_host_name(None);
    let record = w
        .wire_record_when(&host, |record| {
            record.reqs.iter().any(|req| req.names_tag('h'))
        })
        .await;
    let scoped: Vec<_> = record
        .reqs
        .iter()
        .filter(|req| req.names_tag('h'))
        .collect();
    assert!(!scoped.is_empty(), "no group request reached {host:?}");
    for req in scoped {
        assert!(
            req.tag_values('h').contains(&group_id),
            "every group request carries its own group id; this one asked for {:?}",
            req.tag_values('h')
        );
    }
}

#[then(regex = r#"^the request selects exactly (.+)$"#)]
async fn request_selects_exactly(w: &mut NmpWorld, kinds: String) {
    let wanted = parse_kind_list(&kinds);
    let host = w.group_host_name(None);
    let record = w
        .wire_record_when(&host, |record| {
            record
                .reqs
                .iter()
                .any(|req| req.names_tag('h') && req.kinds() == wanted)
        })
        .await;
    let asked: Vec<_> = record
        .reqs
        .iter()
        .filter(|req| req.names_tag('h'))
        .map(|req| req.kinds())
        .collect();
    assert!(
        asked.contains(&wanted),
        "the app asked for {wanted:?}; the wire asked for {asked:?}"
    );
}

#[then(regex = r#"^no relay outside "([^"]+)" was asked$"#)]
async fn no_relay_outside_was_asked(w: &mut NmpWorld, relay: String) {
    w.wire_settled().await;
    let strays: Vec<String> = w
        .relay_names()
        .filter(|name| **name != relay)
        .filter(|name| !w.wire_record(name).reqs.is_empty())
        .cloned()
        .collect();
    assert!(
        strays.is_empty(),
        "these relays were also asked: {strays:?}"
    );
}

#[then(regex = r#"^the group contributed no kind of its own to the request$"#)]
async fn group_contributed_no_kind(w: &mut NmpWorld) {
    let wanted = w
        .last_staged_filter()
        .kinds
        .expect("the app supplied kinds");
    let host = w.group_host_name(None);
    let record = w
        .wire_record_when(&host, |record| {
            record
                .reqs
                .iter()
                .any(|req| req.names_tag('h') && req.kinds() == wanted)
        })
        .await;
    for req in record.reqs.iter().filter(|req| req.names_tag('h')) {
        let extra: Vec<u16> = req.kinds().difference(&wanted).copied().collect();
        assert!(
            extra.is_empty(),
            "the group adds no kind of its own, but the wire also asked for {extra:?}"
        );
    }
}

/// FOUR OBSERVATIONS, not four REQs.
///
/// The count is app-level on purpose. NMP deliberately collapses demands that
/// share a relay and a tag scope into one wire subscription -- that is the
/// whole subject of `features/routing/subscription-collapse.feature` -- so
/// asserting four REQs here would assert the opposite of a shipped contract.
/// What this scenario is about is that ONE group value serves four
/// simultaneous, independent observations, which is exactly what is counted.
#[then(regex = r#"^four independent subscriptions exist at once$"#)]
async fn four_subscriptions_at_once(w: &mut NmpWorld) {
    assert_eq!(
        w.open_group_observations(),
        4,
        "one group serves four simultaneous observations"
    );
    let host = w.group_host_name(None);
    let record = w
        .wire_record_when(&host, |record| !record.reqs.is_empty())
        .await;
    assert!(
        !record.reqs.is_empty(),
        "all four must actually have reached the host"
    );
}

#[then(regex = r#"^the group exposes no observe operation of its own$"#)]
async fn no_observe_of_its_own(w: &mut NmpWorld) {
    assert_no_read_door(&w.group_surface());
}

#[then(regex = r#"^the group exposes no stream, channel or callback of its own$"#)]
async fn no_stream_of_its_own(w: &mut NmpWorld) {
    let surface = w.group_surface();
    // #1033 merged the pure door and its engine binding into one file, so
    // `door` now also imports and aliases `FifoReceiver` -- the SAME ordinary
    // publish stream every other write already returns (the door's own doc
    // comment: "the SAME stream every other publish returns, drained the
    // same way"). That is reuse, not a group-shaped stream of its own, so
    // both the import and the `pub type GroupReceipts = FifoReceiver<..>`
    // alias are excused from the scan below; everything else must still name
    // none of these.
    let lines: Vec<&str> = surface
        .door
        .lines()
        .filter(|line| !line.contains("FifoReceiver"))
        .collect();
    for forbidden in ["Receiver", "Sender", "Subscription", "Fn(", "callback"] {
        let offending: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| line.contains(forbidden))
            .collect();
        assert!(
            offending.is_empty(),
            "the group mints values, never delivery: its source names {forbidden:?} in \
             {offending:?}"
        );
    }
}

#[then(regex = r#"^every group read in the surface passes through the same observe call$"#)]
async fn every_group_read_uses_observe(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert!(
        surface.door.contains("pub fn read("),
        "the group's read contribution is a LiveQuery, taken through the one observe door"
    );
    assert_no_read_door(&surface);
}

#[then(regex = r#"^the query shows only "([^"]+)"$"#)]
async fn query_shows_only(w: &mut NmpWorld, text: String) {
    let wanted = text.clone();
    let shown = w.feed_eventually(move |rows, _| rows.iter().any(|e| e.content == wanted));
    assert!(shown, "expected the query to show {text:?}");
    let others = w.feed_never(move |rows| rows.iter().any(|e| e.content != text));
    assert!(
        others,
        "the `#h` scoping separates two groups on one host; another group's row appeared"
    );
}

#[then(regex = r#"^the query shows no events$"#)]
async fn query_shows_no_events(w: &mut NmpWorld) {
    assert!(
        w.feed_never(|rows| !rows.is_empty()),
        "an unreachable host has nothing to show"
    );
}

#[then(regex = r#"^diagnostics attribute "([^"]+)" to the query's own pinned source$"#)]
async fn diagnostics_attribute_pinned_source(w: &mut NmpWorld, relay: String) {
    let url = w.relay_url(&relay);
    let attributed = w
        .diagnostics_matching(|snapshot| snapshot.relays.iter().any(|r| r.relay == url))
        .is_some();
    assert!(
        attributed,
        "expected {relay:?} to appear in diagnostics as this query's source"
    );
}

#[then(regex = r#"^diagnostics attribute it to no relay-list or operator-configured fact$"#)]
async fn diagnostics_attribute_no_directory_fact(w: &mut NmpWorld) {
    let indexers: Vec<String> = w.indexer_names().to_vec();
    assert!(
        indexers.is_empty(),
        "a query-declared pinning consults no operator-configured indexer, but this world \
         configured {indexers:?}"
    );
    let mine: Vec<String> = w
        .write_relay_of(crate::world::ME)
        .into_iter()
        .filter(|relay| w.relay_contacted(relay))
        .collect();
    assert!(
        mine.is_empty(),
        "a relay-list relay was consulted: {mine:?}"
    );
}

#[then(regex = r#"^per-source acquisition evidence is reported for "([^"]+)"$"#)]
async fn per_source_evidence_for(w: &mut NmpWorld, relay: String) {
    let url = w.relay_url(&relay);
    let reported = w.feed_eventually(move |_, evidence| {
        branch_sources(evidence).any(|source| source.relay.to_string() == url.to_string())
    });
    assert!(
        reported,
        "expected per-source acquisition evidence naming {relay:?}"
    );
}

#[then(regex = r#"^the acquisition evidence reports the host as unreachable$"#)]
async fn evidence_reports_unreachable(w: &mut NmpWorld) {
    let reported = w.feed_eventually(|_, evidence| {
        branch_sources(evidence).next().is_some()
            && branch_sources(evidence).all(|source| source.reconciled_through.is_none())
    });
    assert!(
        reported,
        "an unreachable host must be reported as an unproven source, not as an empty group"
    );
}

#[then(regex = r#"^the pinned set was never widened$"#)]
async fn pinned_set_never_widened(w: &mut NmpWorld) {
    w.wire_settled().await;
    let host = w.group_host_name(None);
    let strays: Vec<String> = w
        .relay_names()
        .filter(|name| **name != host)
        .filter(|name| !w.group_requests(name).is_empty())
        .cloned()
        .collect();
    assert!(
        strays.is_empty(),
        "a pinned group read is never widened by what the engine learns, but {strays:?} \
         were asked"
    );
}
