//! Assertions about the DEFAULT route: which relays an ordinary `Auto` write
//! resolved to, and what the engine said when it resolved to none.
//!
//! Separate from [`super::writes`], which asks where a publish was DELIVERED,
//! and from [`super::routes`], which asks whether resolution is still
//! deciding. This family asks the third question -- what the answer actually
//! WAS -- and it is the only one of the three that can distinguish an outbox
//! that consulted the wrong half of somebody's relay list from one that
//! consulted the right half and could not reach it.
//!
//! Every claim here reads the receipt's own `WriteStatus::Routed` /
//! `AwaitingRoute`, except the two that cannot: "published exactly once" is a
//! count of what a relay ADMITTED, and "no relay outside the ones configured"
//! is read off the engine's planned sessions, because a relay nobody staged
//! has no name for a contact log to be asked about.

use cucumber::then;

use crate::steps::parse_quoted_list;
use crate::world::NmpWorld;

// ---- where the write went ------------------------------------------------

#[then(regex = r#"^the (?:note|event|profile) is routed to exactly (.+)$"#)]
async fn routed_exactly(w: &mut NmpWorld, targets: String) {
    let names = parse_quoted_list(&targets);
    assert!(
        !names.is_empty(),
        "expected quoted relay names in {targets:?}"
    );
    assert!(
        w.routed_exactly(&names),
        "expected the route to be exactly {names:?}; the receipt reported {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the (?:note|event|profile) is routed to ("[^"]+")$"#)]
async fn routed_to(w: &mut NmpWorld, target: String) {
    let names = parse_quoted_list(&target);
    for name in names {
        assert!(
            w.routed_to(&name),
            "expected {name:?} to be among the destinations; the receipt reported {:?}",
            w.receipt_statuses()
        );
    }
}

#[then(regex = r#"^the (?:note|event|profile) is never routed to ("[^"]+")$"#)]
async fn never_routed_to(w: &mut NmpWorld, target: String) {
    for name in parse_quoted_list(&target) {
        assert!(
            w.never_routed_to(&name),
            "an outbox derivation that names {name:?} consulted the wrong set; the receipt \
             reported {:?}",
            w.receipt_statuses()
        );
    }
}

/// The indexers are where the engine ASKS about relay lists, never where it
/// publishes an ordinary event -- "indexers are never a content fallback"
/// cuts this way too.
#[then(regex = r#"^the note is never routed to either indexer$"#)]
async fn never_routed_to_an_indexer(w: &mut NmpWorld) {
    let indexers: Vec<String> = w.indexer_names().to_vec();
    nothing_to_observe!(
        !indexers.is_empty(),
        "this scenario configured no indexer, so there is none for the route to have \
         wrongly named"
    );
    for name in indexers {
        assert!(
            w.never_routed_to(&name),
            "an indexer is a discovery source, never a publishing destination of last \
             resort; the receipt reported {:?}",
            w.receipt_statuses()
        );
    }
}

#[then(regex = r#"^routing is complete$"#)]
async fn routing_is_complete(w: &mut NmpWorld) {
    assert!(
        w.routing_is_complete(),
        "expected routing to retire -- zero unknowns remain, so the answer can never \
         change again; the receipt reported {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^routing is not complete$"#)]
async fn routing_is_not_complete(w: &mut NmpWorld) {
    assert!(
        w.routing_stays_open(),
        "expected routing to stay open while something is still unknown; the receipt \
         reported {:?}",
        w.receipt_statuses()
    );
}

/// A kind:0 must route by exactly the derivation a kind:1 does. Reads the two
/// publishes by ORDER, because both obligations are live at once and "the
/// receipt" cannot name either.
#[then(regex = r#"^the profile and the note are routed to the same relays$"#)]
async fn profile_and_note_route_alike(w: &mut NmpWorld) {
    let profile = w.final_route_at(0);
    let note = w.final_route_at(1);
    nothing_to_observe!(
        !profile.is_empty() || !note.is_empty(),
        "neither publish was ever routed anywhere, so the two answers agree only \
         because there are none"
    );
    assert_eq!(
        profile, note,
        "a kind:0 that reaches the app relays by a kind-specific branch would differ \
         here; the derivation must not look at the kind at all"
    );
}

/// Union, not concatenation: a relay two sources both name is ONE destination
/// and one offer, which is what makes the resolver's output safe to feed a
/// lane keyed by `(intent, relay)`.
#[then(regex = r#"^the note is published to "([^"]+)" exactly once$"#)]
async fn published_exactly_once(w: &mut NmpWorld, name: String) {
    let event = w
        .published_event_id()
        .expect("nmp-bdd: this publish never reported the body it froze");
    // The relay has to have taken it at all before "exactly once" means
    // anything -- zero copies is also "not twice".
    nothing_to_observe!(
        w.wait_for_copy(&name, event).await,
        "{name:?} never received the note at all, so it did not receive it twice either"
    );
    // And then a full settle window, so a SECOND offer has as long to arrive
    // as the first one took.
    w.settle().await;
    let copies = w.copies_admitted(&name, event);
    assert_eq!(
        copies, 1,
        "expected {name:?} to be offered the note once; it admitted {copies} copies"
    );
}

// The idempotency scenarios' spelling of the same count -- `"<relay>" was
// offered the note exactly once` -- is #1018's, in `then::payloads`, and it
// counts every ordinal rather than only one. Nothing is defined for it here:
// two definitions matching one sentence is an AMBIGUOUS match, which cucumber
// refuses outright, so the weaker of the two would not have quietly won -- but
// the scenario would still have failed for a harness reason rather than an
// engine one.

// ---- what must never have been contacted ---------------------------------

/// The three sources and nothing else. The indexers are allowed: asking one
/// where somebody's relay list is happens on the READ plane and never makes
/// it a destination -- which is the claim the sibling step above pins.
#[then(
    regex = r#"^no relay outside the author's, the app's, and the recipients' was ever contacted$"#
)]
async fn no_relay_outside_the_three_sources(w: &mut NmpWorld) {
    let me = w.me();
    let mut allowed = w.write_relay_names_of(&me);
    allowed.extend(w.app_relay_names().iter().cloned());
    allowed.extend(w.indexer_names().iter().cloned());
    for person in w.people_named() {
        allowed.extend(w.read_relay_names_of(&person));
    }
    nothing_to_observe!(
        w.wait_any_relay_contacted().await,
        "no relay in this world was contacted at all, so nothing outside {allowed:?} is \
         unreached only because nothing ever ran"
    );
    let strays: Vec<String> = w
        .relay_names()
        .filter(|name| !allowed.contains(name))
        .filter(|name| w.relay_contacted(name))
        .cloned()
        .collect();
    assert!(
        strays.is_empty(),
        "the outbox default reads only engine-owned directory facts, so {strays:?} could \
         not have come from the author's, the app's or a recipient's list"
    );
}

/// The engine ships no relay list of its own. Read off the PLANNED sessions
/// rather than a contact log, because a relay this world never staged has no
/// name a contact log could be asked about -- and a substituted public relay
/// is exactly a URL nobody staged.
#[then(regex = r#"^no relay outside the ones configured is ever contacted$"#)]
async fn no_unconfigured_relay(w: &mut NmpWorld) {
    let staged: Vec<String> = w.relay_names().cloned().collect();
    nothing_to_observe!(
        w.diagnostics_ran(),
        "the engine never reported a diagnostics snapshot, so it planned nothing that \
         could have been outside {staged:?}"
    );
    let planned = w.planned_relays();
    let strays: Vec<String> = planned
        .into_iter()
        .filter(|name| !staged.contains(name))
        .collect();
    assert!(
        strays.is_empty(),
        "a relay nobody configured is a relay nobody consented to publish through, and \
         the engine planned {strays:?}"
    );
}

#[then(regex = r#"^no relay is ever contacted for the note$"#)]
async fn no_relay_contacted_for_the_note(w: &mut NmpWorld) {
    // A refusal has to have been REPORTED before "and nothing was sent" is a
    // fact about this write rather than about a world that never ran.
    let reasons = w.park_reasons();
    nothing_to_observe!(
        !reasons.is_empty(),
        "the publish never reported a routing park, so it was never refused and this \
         step is asserting about a write that may simply not have run yet"
    );
    let event = w
        .published_event_id()
        .expect("nmp-bdd: this publish never reported the body it froze");
    w.settle().await;
    let holders = w.relays_holding(event);
    assert!(
        holders.is_empty(),
        "an event with no destination must reach none, but {holders:?} hold it"
    );
}

#[then(regex = r#"^the note is routed to no relay$"#)]
async fn routed_to_no_relay(w: &mut NmpWorld) {
    assert!(
        w.routed_nowhere(),
        "expected no destination ever to be named; the receipt reported {:?}",
        w.receipt_statuses()
    );
}

// ---- the refusal, and what it says ---------------------------------------

#[then(regex = r#"^the publish (?:still )?reports that no destination could be determined$"#)]
async fn reports_no_destination(w: &mut NmpWorld) {
    assert!(
        w.park_reason_contains("no destination could be determined"),
        "the app did everything correctly and has no other way to find out its user's \
         message went nowhere; the write reported {:?}",
        w.park_reasons()
    );
}

#[then(regex = r#"^the reason (?:still )?names that my own relay list is absent$"#)]
async fn reason_names_my_absent_relay_list(w: &mut NmpWorld) {
    let me = w.my_pubkey_hex();
    let wanted = format!("author routes are Absent for {me}");
    assert!(
        w.park_reason_contains(&wanted),
        "\"stuck\" and \"stuck because X\" are different messages, and only the second \
         one names a thing to fix -- neutral author routes settled Absent are a final \
         answer, not an Unknown provider need. The write reported {:?}",
        w.park_reasons()
    );
}

#[then(regex = r#"^the reason names that no app relays are configured$"#)]
async fn reason_names_no_app_relays(w: &mut NmpWorld) {
    assert!(
        w.park_reason_contains("no app relays are configured"),
        "every exhausted source is named because configuring any one of them would have \
         produced a route, so the reason doubles as the list of ways to fix it; the \
         write reported {:?}",
        w.park_reasons()
    );
}

#[then(regex = r#"^the publish reports no routing problem$"#)]
async fn reports_no_routing_problem(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the publish reported no status at all, so it is unparked only because nothing ran"
    );
    assert!(
        w.never_parked(),
        "the refusal is about having nothing, never a policy that forbids thin routes; \
         the receipt reported {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the (?:note|write) is never reported as sent$"#)]
async fn never_reported_as_sent(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the publish reported no status at all, so it is unsent only because nothing ran"
    );
    assert!(
        w.never_sent(),
        "an event that reaches nothing must never look like one that was delivered; the \
         receipt reported {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the publish has not failed$"#)]
async fn publish_has_not_failed(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the publish reported no status at all, so it is unfailed only because nothing ran"
    );
    assert!(
        w.never_failed(),
        "not knowing enough yet is a reason to WAIT, never a reason to destroy a durable \
         obligation; the receipt reported {:?}",
        w.receipt_statuses()
    );
}

// ---- the global question: is anything quietly stuck? ---------------------

/// #1025 owns the section and its shape; what this asserts is that the outbox
/// derivation actually populates it. A write with nowhere to go is
/// `Unroutable` -- it is signed and its author can sign, so it is neither
/// unsignable nor undeliverable.
#[then(regex = r#"^diagnostics reports the note among the stalled writes$"#)]
async fn diagnostics_reports_a_stalled_write(w: &mut NmpWorld) {
    let stalled = w.unroutable_writes();
    assert!(
        !stalled.is_empty(),
        "per-receipt reporting answers \"what happened to THIS note\" and needs somebody \
         to be asking; a misconfigured app is the case where EVERY write lands here and \
         no single receipt tells the operator so"
    );
}

#[then(regex = r#"^its stalled entry carries the same reason and how long it has been so$"#)]
async fn stalled_entry_carries_reason_and_age(w: &mut NmpWorld) {
    let reasons = w.park_reasons();
    let published_at = w.last_publish_at();
    let stalled = w.unroutable_writes();
    nothing_to_observe!(
        !stalled.is_empty(),
        "nothing is reported stalled, so there is no entry for this step to read"
    );
    let (detail, since) = stalled
        .into_iter()
        .next()
        .expect("checked non-empty just above");
    assert!(
        reasons.contains(&detail),
        "the row carries the receipt's OWN park reason verbatim (#1025), so an operator \
         holding both never has to decide whether two differently-worded sentences are \
         the same fact; the row said {detail:?} and the receipt said {reasons:?}"
    );
    // "How long it has been so" is `now - stalled_since`, and #1025's
    // `stalled_since` is the ACCEPTANCE instant -- durable, so it survives a
    // restart, at the cost of being an over-estimate for `Undeliverable`. For
    // `Unroutable` the two coincide, because an accepted write is routed
    // immediately. What this pins is that the instant was RECORDED rather than
    // fabricated: it cannot predate the publish that produced it.
    assert!(
        since >= published_at,
        "the row's instant is the moment this obligation was accepted, which cannot \
         predate the publish; the row said {since:?} and the publish went out at \
         {published_at:?}"
    );
}
