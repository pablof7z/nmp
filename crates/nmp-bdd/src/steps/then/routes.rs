//! Assertions about ROUTING as a lifecycle: whether the strategy is still
//! deciding, what it says it is waiting for, and whether it can ever change
//! its mind again.
//!
//! This is a different question from [`super::writes`]'s "where did the
//! publish go", and keeping it apart is the point: routed and published are
//! separate axes, and a suite that read one off the other could not tell a
//! misconfigured indexer set from a slow relay. Every assertion here reads
//! `WriteFact::Destinations { relays, complete }` -- the one fact an app
//! actually has about where a write is going and whether resolution can
//! still change its mind -- and never a harness-side view of engine
//! internals.

use cucumber::then;

use nmp::mechanism::publish_queue::WriteFact;

use crate::world::NmpWorld;

/// True once the receipt has said routing is COMPLETE -- nothing left to
/// learn, so the strategy can never produce a different answer.
fn routing_completed(w: &mut NmpWorld) -> bool {
    w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteFact::Destinations { complete: true, .. }))
    })
}

/// The receipt's latest routing verdict: `Some(true)` once it reported a
/// complete route, `Some(false)` while it is still deciding, `None` if it has
/// said nothing about routing at all.
fn latest_completeness(w: &mut NmpWorld) -> Option<bool> {
    w.receipt_statuses().iter().rev().find_map(|s| match s {
        WriteFact::Destinations { complete, .. } => Some(*complete),
        _ => None,
    })
}

// ---- still deciding ------------------------------------------------------

#[then(regex = r#"^the receipt reports it is still determining destinations$"#)]
async fn still_determining(w: &mut NmpWorld) {
    let determining = w.receipt_eventually(|seen| {
        seen.iter().any(|s| {
            matches!(
                s,
                WriteFact::Destinations {
                    complete: false,
                    ..
                }
            )
        })
    });
    assert!(
        determining,
        "expected the receipt to say routing is still open -- either parked with nothing \
         resolved, or resolved so far but incomplete; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^routing for the note is not complete$"#)]
async fn routing_not_complete(w: &mut NmpWorld) {
    // The negative form has to cost its budget: "it never completed" is only
    // meaningful once the world has had as long to complete it as any
    // positive assertion would allow.
    let completed = w.receipt_never(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteFact::Destinations { complete: true, .. }))
    });
    assert!(
        completed,
        "expected routing to stay open while something is still unknown; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the receipt never reported routing complete$"#)]
async fn never_reported_complete(w: &mut NmpWorld) {
    routing_not_complete(w).await;
}

#[then(regex = r#"^the receipt reports no destinations yet$"#)]
async fn no_destinations_yet(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the publish reported no status at all, so it has no destinations either way"
    );
    let statuses = w.receipt_statuses();
    assert!(
        !statuses
            .iter()
            .any(|s| matches!(s, WriteFact::Destinations { relays, .. } if !relays.is_empty())),
        "expected no destination to have been named yet; saw {statuses:?}"
    );
}

// ---- what the park says --------------------------------------------------

/// The park AND its reason: an open destination picture that names at least
/// one author whose routes it is still waiting on.
///
/// The reason is the whole point of this step, and it is deliberately a
/// stronger claim than `the receipt reports it is still determining
/// destinations` -- which asserts only `complete: false`. A park that named
/// nobody would satisfy that sentence and fail this one. Before #1236 it
/// could not: the reason was computed in the engine and never left it, so
/// this step could prove the park existed and nothing else, which is a step
/// that cannot fail for the reason it exists.
#[then(
    regex = r#"^the receipt says (?:why it is still determining destinations|why it cannot settle)$"#
)]
async fn park_carries_a_reason(w: &mut NmpWorld) {
    let reasoned = w.park_names_an_author();
    assert!(
        reasoned,
        "a park that says only THAT it is stuck is not actionable: the open destination \
         picture must name the authors whose routes it is waiting on; saw {:?}",
        w.routing_facts_reported()
    );
}

/// Parked on a NAMED author: the person the scenario staged as unlooked-up
/// is the person the receipt says it is waiting for.
///
/// Asserting only that resolution is incomplete would re-prove the preceding
/// `routing for the note is not complete` step and pass no matter whose
/// lookup was outstanding.
#[then(regex = r#"^the note stays parked awaiting (\S+)'s relay list$"#)]
async fn parked_awaiting_person(w: &mut NmpWorld, person: String) {
    let awaited = w.person(&person).public_key();
    assert!(
        w.park_awaits(awaited),
        "expected the receipt's park to name {person} as the route it is waiting on; \
         saw {:?}",
        w.routing_facts_reported()
    );
}

/// "is not" and "is never" are the same claim here — the negative form costs
/// its full budget either way — and both spellings appear in the catalog, so
/// one step owns both rather than letting the unmatched one skip silently.
#[then(regex = r#"^the (?:note|write) is (?:not|never) reported as failed$"#)]
async fn not_reported_failed(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the publish reported no status at all, so it is unfailed only because nothing ran"
    );
    let never_failed = w.never_failed();
    assert!(
        never_failed,
        "not knowing enough yet is a reason to WAIT, never a reason to destroy a durable \
         obligation; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the publish is accepted$"#)]
async fn publish_is_accepted(w: &mut NmpWorld) {
    let accepted = w.publish_was_accepted();
    assert!(
        accepted,
        "a write the engine cannot route YET is still a well-formed obligation, and \
         acceptance is what makes it durable; saw {:?}",
        w.receipt_statuses()
    );
}

/// The destination is read off `Routed`, which is the routing answer, never
/// off a delivery fact: "we know exactly where this goes" and "it has gone
/// there" are separate axes, and this step owns only the first.
#[then(regex = r#"^the write was routed to "([^"]+)"$"#)]
async fn write_was_routed_to(w: &mut NmpWorld, url: String) {
    let url = nmp_router::RelayUrl::parse(&url).expect("nmp-bdd: a scenario names a real URL");
    let routed = w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteFact::Destinations { relays, .. } if relays.contains(&url)))
    });
    assert!(
        routed,
        "expected the write to be routed to {url}; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the receipt does not report a failure$"#)]
async fn receipt_reports_no_failure(w: &mut NmpWorld) {
    not_reported_failed(w).await;
}

#[then(regex = r#"^the write is still held, not dropped$"#)]
async fn write_still_held(w: &mut NmpWorld) {
    not_reported_failed(w).await;
    assert_eq!(
        w.receipt_count(),
        1,
        "the obligation must still be the one the app is holding"
    );
}

// ---- nothing left to learn -----------------------------------------------

#[then(regex = r#"^the receipt reports routing complete$"#)]
async fn reports_routing_complete(w: &mut NmpWorld) {
    assert!(
        routing_completed(w),
        "expected routing to retire -- zero unknowns remain, so the answer can never \
         change again; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^routing for the note is complete(?: immediately)?$"#)]
async fn routing_for_note_complete(w: &mut NmpWorld) {
    reports_routing_complete(w).await;
}

/// Retirement is knowledge exhaustion, and the observable form of "the entry
/// is consumed" is that the receipt reported a COMPLETE route and never went
/// back to determining afterwards.
#[then(regex = r#"^the routing entry is consumed$"#)]
async fn routing_entry_consumed(w: &mut NmpWorld) {
    assert!(
        routing_completed(w),
        "an entry with unknowns left cannot be consumed; saw {:?}",
        w.receipt_statuses()
    );
    assert_eq!(
        latest_completeness(w),
        Some(true),
        "a consumed entry never returns to determining destinations; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the routing entry is not consumed$"#)]
async fn routing_entry_not_consumed(w: &mut NmpWorld) {
    routing_not_complete(w).await;
}

/// A park names nobody now, so "parked waiting on Bob SPECIFICALLY" has no
/// receipt spelling. The claim that survives is the stronger-in-one-direction
/// one: this write was never parked with nothing resolved at all, which a
/// write genuinely waiting on Bob could not satisfy.
#[then(regex = r#"^the note is never parked waiting on (\S+)$"#)]
async fn never_parked_on(w: &mut NmpWorld, person: String) {
    let parked = w.never_parked();
    assert!(
        parked,
        "a relay list that declares NO relays is knowledge, not ignorance -- nothing waits \
         on {person}; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^nothing is left parked on (\S+) or (\S+)$"#)]
async fn nothing_parked_on_either(w: &mut NmpWorld, first: String, _second: String) {
    never_parked_on(w, first).await;
}

// ---- what an unknown author must NOT cause -------------------------------

#[then(regex = r#"^no relay is contacted on (\S+)'s behalf$"#)]
async fn no_relay_contacted_for(w: &mut NmpWorld, person: String) {
    let named = w.read_relay_names_of(&person);
    nothing_to_observe!(
        w.any_relay_contacted(),
        "no relay in this world was contacted at all, so {person}'s are unreached only \
         because nothing ever ran"
    );
    for name in named {
        assert!(
            !w.relay_contacted(&name),
            "expected nothing to be sent to {name:?} on {person}'s behalf"
        );
    }
}

#[then(regex = r#"^no relays are guessed for (\S+)$"#)]
async fn no_relays_guessed_for(w: &mut NmpWorld, person: String) {
    no_relay_contacted_for(w, person).await;
}

/// The whole point of the outline this serves: the RELAY SET is IDENTICAL in
/// both examples — an author with no relay list contributes nothing whether
/// that is because we know they have none or because we have not finished
/// looking — and only the completeness differs. So this asserts the set, not
/// the absence of contact: reading it off a contact counter would pass on an
/// empty world, and reading it off completeness would assert the very thing
/// the two examples disagree about.
#[then(regex = r#"^no relays are known for (\S+) either way$"#)]
async fn no_relays_known_either_way(w: &mut NmpWorld, _person: String) {
    let me = w.me();
    let mine: std::collections::BTreeSet<_> = w
        .write_relay_names_of(&me)
        .iter()
        .map(|name| w.relay_url(name))
        .collect();
    nothing_to_observe!(
        !mine.is_empty(),
        "the author staged no write relay, so an outbox derivation has nothing to name \
         with or without the mentioned author"
    );
    let only_mine = w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteFact::Destinations { relays, .. } if *relays == mine))
    });
    assert!(
        only_mine,
        "expected the route to name exactly the author's own write relays and nothing \
         else; saw {:?}",
        w.receipt_statuses()
    );
}
