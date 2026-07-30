//! Assertions about WHICH relay was contacted, for what, and in which lane.
//!
//! The claims here are about the routing plan rather than about any one
//! query's rows: an indexer must never be asked for a content atom, a relay
//! outside the plan must never be opened at all, a relay that WAS contacted
//! must be accounted for with a lane, and a subscription already serving
//! someone must not be disturbed by an unrelated write. Two witnesses are
//! used deliberately -- the engine's own diagnostics for what it planned, and
//! the scripted relay's own contact log for what actually reached a socket --
//! so a claim about "never contacted" never rests solely on the thing under
//! test.

use std::time::{Duration, Instant};

use cucumber::then;

use crate::steps::parse_people;
use crate::world::{NmpWorld, EVENTUALLY};

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

/// The optional NIP-65 assembly's exact protocol query. Generic routing has
/// no discovery-kind class and cannot widen these sources to content.
fn is_nip65_kind(k: u16) -> bool {
    k == 10_002
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

#[then(regex = r#"^the subscriptions serving (.+) are untouched$"#)]
async fn subscriptions_untouched(w: &mut NmpWorld, list: String) {
    for person in parse_people(&list) {
        let relays = w.write_relay_of(&person);
        assert!(
            !relays.is_empty(),
            "{person} has no declared write relay to check for untouched-ness"
        );
        for relay in relays {
            let (before, after) = w.contact_counts_since_snapshot(&relay);
            assert!(
                w.relay_untouched_since_snapshot(&relay),
                "expected {person}'s relay {relay:?} to receive no new REQ/EVENT, \
                 but its contact count moved {before} -> {after}; since the \
                 snapshot: {report}",
                report = w.touch_report_since_snapshot(&relay)
            );
        }
    }
}

#[then(regex = r#"^the indexers are asked only for relay lists and profiles$"#)]
async fn indexers_discovery_only(w: &mut NmpWorld) {
    // The structural invariant: an operator-selected NIP-65 source carries
    // only the assembly's exact kind:10002 query, never generic content.
    let names: Vec<String> = w.indexer_names().to_vec();
    let urls: Vec<_> = names.iter().map(|n| w.relay_url(n)).collect();
    // Polled through the predicate rather than read off the first snapshot
    // that mentions an indexer at all: a relay's row appears before its
    // filters do, so a one-shot read of `filters` would make this precondition
    // a race rather than a fact.
    let snapshot = w.diagnostics_matching(|snap| {
        urls.iter().any(|u| {
            snap.relays
                .iter()
                .any(|r| &r.relay == u && !r.filters.is_empty())
        })
    });
    nothing_to_observe!(
        snapshot.is_some(),
        "no indexer ({names:?}) was ever seen carrying a filter, so none of them was \
         asked for anything and discovery-only holds trivially"
    );
    let snapshot = snapshot.expect("checked just above");
    for (name, url) in names.iter().zip(urls.iter()) {
        let Some(relay_diag) = snapshot.relays.iter().find(|r| &r.relay == url) else {
            // This particular indexer was never contacted -- trivially
            // discovery-only (nothing was ever asked of it).
            continue;
        };
        for filter_json in &relay_diag.filters {
            for kind in filter_kinds(filter_json) {
                assert!(
                    is_nip65_kind(kind),
                    "NIP-65 source {name:?} carries a non-NIP-65 filter (kind {kind}): {filter_json}"
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

/// Bounded wait for the engine to have contacted at least one of `names`.
///
/// THE PRECONDITION OF EVERY "was never contacted" ASSERTION, and it has to
/// be a wait rather than a read. The contact log answers about the instant
/// the step runs, and `When my feed of my follows' notes runs to a steady
/// state` returns as soon as the feed's first bounded poll does -- which can
/// be before the engine has opened a single connection. A one-shot read
/// there says, truthfully and uselessly, that no relay outside the plan was
/// contacted, because no relay at all was. Waiting for routing to actually
/// begin is also strictly SAFER for the assertion that follows: it gives a
/// wrongly-contacted relay more time to show up, never less.
async fn some_relay_contacted(w: &NmpWorld, names: &[String]) -> bool {
    let deadline = Instant::now() + EVENTUALLY;
    loop {
        if names.iter().any(|name| w.relay_contacted(name)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[then(regex = r#"^no relay outside the indexers(.*) was ever contacted$"#)]
async fn no_relay_outside_the_plan(w: &mut NmpWorld, extra: String) {
    let mut allowed: Vec<String> = w.indexer_names().to_vec();
    allowed.extend(parse_relay_list_tail(&extra));
    nothing_to_observe!(
        some_relay_contacted(w, &allowed).await,
        "not one relay INSIDE the plan ({allowed:?}) was contacted either, so no \
         routing ever happened to stay inside it"
    );
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
    let others: Vec<String> = w.relay_names().filter(|n| **n != name).cloned().collect();
    nothing_to_observe!(
        some_relay_contacted(w, &others).await,
        "no relay in this world was contacted at all, so {name:?} is unreached only \
         because nothing ever ran"
    );
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
