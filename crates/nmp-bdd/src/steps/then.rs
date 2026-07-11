//! `Then` — an observable outcome, always one of the four channels
//! (approach doc §1.3): rows on a feed, receipt states, diagnostics facts,
//! acquisition-evidence facts. Every assertion below reads ONLY through
//! `NmpWorld`'s public observers (`feed_*`/`receipt_*`/`diagnostics_*`/
//! `relay_contacted`/`relay_untouched_since_snapshot`) -- never anything
//! engine-internal.

use cucumber::then;

use nmp_engine::outbox::WriteStatus;

use crate::steps::parse_people;
use crate::world::NmpWorld;

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
