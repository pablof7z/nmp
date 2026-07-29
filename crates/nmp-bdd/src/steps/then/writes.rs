//! Assertions about a PUBLISH: where it was routed, what its receipt said,
//! and what came back out the other side.
//!
//! This is the write plane, and it is a different domain from
//! [`super::routing`], which is the READ plane -- which relay was asked for
//! which kinds, in which lane. Both talk about relays; only one of them is
//! about an event this app sent. Keeping them apart is what lets a reader ask
//! "what happens when I publish?" and find one file.
//!
//! Four claims live here, and #1006's own section banners are kept because
//! they name them exactly:
//!
//! - the two-word routing surface (`Auto` vs `Explicit`) and what each did;
//! - the empty route, refused at the acceptance door;
//! - routing being independent of authorship -- a republished event's bytes
//!   are the bytes its author signed;
//! - what the removal of the private-route vocabulary must NOT have taken
//!   with it.
//!
//! Delivery is read off the RECEIPT rather than a harness-side mailbox
//! throughout: `WriteStatus::Acked(url)` is the relay itself confirming it
//! took the event, which is the only delivery fact an app ever gets.

use cucumber::then;

use nmp::mechanism::outbox::WriteStatus;
use nostr::JsonUtil;

use crate::world::{NmpWorld, ME};

// ---- the receipt's own status stream -------------------------------------

#[then(regex = r#"^the receipt first reports only accepted -- never sent$"#)]
async fn receipt_first_accepted(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the receipt reported no status at all, so it has no FIRST status to be wrong"
    );
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

/// The refusal, attributed to the relay that made it. Its sibling below
/// asserts the WORDS; this one asserts only that this destination -- and not
/// some other -- is the one that said no.
#[then(regex = r#"^the receipt reports "([^"]+)" rejected the note$"#)]
async fn receipt_reports_relay_rejected(w: &mut NmpWorld, relay_name: String) {
    let relay_url = w.relay_url(&relay_name);
    let rejected = w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Rejected(url, _) if *url == relay_url))
    });
    assert!(
        rejected,
        "expected the receipt to report {relay_name:?} as the destination that refused; saw {:?}",
        w.receipt_statuses()
    );
}

/// Verbatim, prefix included. "blocked: not admitted" is actionable and
/// "failed" is not, and NMP has no business paraphrasing a message it did
/// not write.
#[then(regex = r#"^the reason is the relay's own words "([^"]+)"$"#)]
async fn reason_is_the_relays_own_words(w: &mut NmpWorld, message: String) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| seen
            .iter()
            .any(|s| matches!(s, WriteStatus::Rejected(_, _)))),
        "no destination refused this write at all, so there are no words to have been kept"
    );
    let statuses = w.receipt_statuses();
    let said: Vec<&String> = statuses
        .iter()
        .filter_map(|s| match s {
            WriteStatus::Rejected(_, reason) => Some(reason),
            _ => None,
        })
        .collect();
    assert!(
        said.iter().any(|reason| reason.as_str() == message),
        "expected the relay's own sentence {message:?} to reach the receipt unchanged; saw {said:?}"
    );
}

// ---- one publish retiring another ---------------------------------------
//
// These are the only steps that name a publish by ORDER instead of by
// recency. A scenario about a newer replaceable write retiring an older one
// has TWO live obligations at once and has to say something different about
// each, which "the receipt" cannot express.

fn receipt_ordinal(name: &str) -> usize {
    match name {
        "first" => 0,
        "second" => 1,
        other => panic!("unsupported receipt ordinal {other:?}"),
    }
}

#[then(regex = r#"^the (first|second) receipt reports waiting for "([^"]+)"$"#)]
async fn numbered_receipt_waiting(w: &mut NmpWorld, ordinal: String, relay_name: String) {
    let relay_url = w.relay_url(&relay_name);
    let waiting = w.receipt_eventually_at(receipt_ordinal(&ordinal), |seen| {
        seen.iter().any(
            |status| matches!(status, WriteStatus::AwaitingRelay { relay } if *relay == relay_url),
        )
    });
    assert!(
        waiting,
        "expected the {ordinal} receipt to wait for {relay_name:?}"
    );
}

#[then(regex = r#"^the (first|second) receipt reports superseded by the newer replaceable write$"#)]
async fn numbered_receipt_superseded(w: &mut NmpWorld, ordinal: String) {
    let superseded = w.receipt_eventually_at(receipt_ordinal(&ordinal), |seen| {
        seen.iter()
            .any(|status| matches!(status, WriteStatus::Superseded))
    });
    assert!(
        superseded,
        "expected the {ordinal} receipt to terminate as superseded"
    );
}

#[then(regex = r#"^the (first|second) receipt reports acked by "([^"]+)"$"#)]
async fn numbered_receipt_acked(w: &mut NmpWorld, ordinal: String, relay_name: String) {
    let relay_url = w.relay_url(&relay_name);
    let acked = w.receipt_eventually_at(receipt_ordinal(&ordinal), |seen| {
        seen.iter()
            .any(|status| matches!(status, WriteStatus::Acked(relay) if *relay == relay_url))
    });
    assert!(
        acked,
        "expected the {ordinal} receipt to be acked by {relay_name:?}"
    );
}

// ---- routing: the two words ---------------------------------------------
//
// "Delivered to <relay>" is read off the RECEIPT, not off a harness-side
// mailbox: `WriteStatus::Acked(url)` is the relay itself confirming it took
// the event, which is the only delivery fact an app ever gets.

/// `Then the note is delivered to "a"` / `... to "a" and "b"`, and the
/// routing-plane spelling `... is published to "a"`. One assertion for both:
/// "published" and "delivered" name the same observable — the relay itself
/// confirming it took the event.
#[then(regex = r#"^the (?:note|event|relay list) is (?:delivered|published) to (.+)$"#)]
async fn delivered_to(w: &mut NmpWorld, targets: String) {
    let names = crate::steps::parse_quoted_list(&targets);
    assert!(
        !names.is_empty(),
        "expected quoted relay names in {targets:?}"
    );
    for name in names {
        let url = w.relay_url(&name);
        let acked = w.receipt_eventually(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Acked(u) if *u == url))
        });
        assert!(
            acked,
            "expected the write to be acked by {name:?}; receipt showed {:?}",
            w.receipt_statuses()
        );
    }
}

#[then(regex = r#"^"([^"]+)" was never contacted$"#)]
async fn relay_never_contacted(w: &mut NmpWorld, name: String) {
    nothing_to_observe!(
        w.any_relay_contacted(),
        "no relay in this world was contacted at all, so {name:?} is unreached only \
         because nothing ever ran"
    );
    assert!(
        !w.relay_contacted(&name),
        "expected relay {name:?} to never be contacted"
    );
}

/// The all-quoted form. Its sibling above starts with the unquoted words
/// "the indexers" and owns every phrase that names the configured indexer
/// set; this one owns the phrases that name relays and nothing else, so the
/// two regexes cannot both match the same sentence.
#[then(regex = r#"^no relay outside ("[^"]+".*) was ever contacted$"#)]
async fn no_relay_outside(w: &mut NmpWorld, targets: String) {
    let allowed = crate::steps::parse_quoted_list(&targets);
    nothing_to_observe!(
        w.any_relay_contacted(),
        "no relay in this world was contacted at all, so nothing outside {allowed:?} is \
         unreached only because nothing ever ran"
    );
    let strays: Vec<String> = w
        .relay_names()
        .filter(|n| !allowed.contains(n))
        .filter(|n| w.relay_contacted(n))
        .cloned()
        .collect();
    assert!(
        strays.is_empty(),
        "expected only {allowed:?} to be contacted, but {strays:?} also were"
    );
}

/// The app handed over a routing value and nothing else. There is no relay
/// argument on the publish door for it to have filled in, which is exactly
/// what "the app named no relay" means -- so what this checks is that the
/// route the engine executed came from the directory, not from the caller.
#[then(regex = r#"^the app named no relay anywhere in that publish$"#)]
async fn app_named_no_relay(w: &mut NmpWorld) {
    assert!(
        w.last_publish_named_no_relay(),
        "the publish under test carried an explicit relay set; this scenario is about \
         the route NMP derived on its own"
    );
}

#[then(regex = r#"^exactly one receipt exists for that publish$"#)]
async fn exactly_one_receipt(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the publish reported no status at all, so there is no receipt to count"
    );
    assert_eq!(
        w.receipt_count(),
        1,
        "one publish is one obligation and one receipt stream"
    );
}

#[then(regex = r#"^the receipt reports exactly one destination$"#)]
async fn receipt_reports_one_destination(w: &mut NmpWorld) {
    let routed_once = w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Routed { relays, .. } if relays.len() == 1))
    });
    assert!(
        routed_once,
        "expected the receipt to report exactly one destination; saw {:?}",
        w.receipt_statuses()
    );
}

// ---- the empty route ----------------------------------------------------

#[then(regex = r#"^the publish is refused before anything is accepted$"#)]
async fn refused_before_acceptance(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the publish reported no status at all, so it was neither refused nor accepted"
    );
    let statuses = w.receipt_statuses();
    assert!(
        matches!(statuses.first(), Some(WriteStatus::Failed(_))),
        "expected Failed to be the FIRST and only status, never Accepted; saw {statuses:?}"
    );
    assert!(
        !statuses.iter().any(|s| matches!(s, WriteStatus::Accepted)),
        "a refused publish must never report Accepted; saw {statuses:?}"
    );
}

#[then(regex = r#"^no receipt is created$"#)]
async fn no_receipt_is_created(w: &mut NmpWorld) {
    let statuses = w.receipt_statuses();
    assert!(
        !statuses.iter().any(|s| matches!(s, WriteStatus::Accepted)),
        "a refused publish allocates no durable receipt; saw {statuses:?}"
    );
}

#[then(regex = r#"^nothing is written to the journal$"#)]
async fn nothing_written_to_journal(w: &mut NmpWorld) {
    let statuses = w.receipt_statuses();
    assert!(
        !statuses.iter().any(|s| matches!(s, WriteStatus::Accepted)),
        "acceptance IS the journal write, and it must not have happened; saw {statuses:?}"
    );
}

#[then(regex = r#"^no relay is contacted$"#)]
async fn no_relay_is_contacted(w: &mut NmpWorld) {
    let contacted: Vec<String> = w
        .relay_names()
        .filter(|n| w.relay_contacted(n))
        .cloned()
        .collect();
    assert!(
        contacted.is_empty(),
        "expected no relay to be contacted, but {contacted:?} were"
    );
}

/// Read off the signer itself, not off the receipt: `WriteStatus::Signed`
/// is a lifecycle beat the engine emits for an already-signed payload too,
/// so it says nothing about whether a signer was asked.
#[then(regex = r#"^no signer was asked for anything$"#)]
async fn no_signer_was_asked(w: &mut NmpWorld) {
    // Let anything the publish was going to do actually happen first.
    w.receipt_never(|_| false);
    let asked = w.signer_ask_count();
    assert_eq!(
        asked, 0,
        "expected the signer never to be asked, but it was asked {asked} time(s)"
    );
}

// ---- routing is independent of authorship -------------------------------

#[then(regex = r#"^"([^"]+)" received the note with (\S+)'s signature untouched$"#)]
async fn received_with_signature_untouched(w: &mut NmpWorld, name: String, person: String) {
    let url = w.relay_url(&name);
    let acked = w.receipt_eventually(|seen| {
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Acked(u) if *u == url))
    });
    assert!(acked, "expected {name:?} to accept the republished event");

    let event = w
        .republished_event()
        .cloned()
        .expect("this scenario republishes an already-signed event");
    let author = w.pubkey_hex(&person);
    assert_eq!(
        event.pubkey.to_hex(),
        author,
        "the republished event must still be signed by {person}"
    );
    event
        .verify()
        .expect("the republished signature must still verify");
}

#[then(regex = r#"^the note's event id is the one (\S+) signed$"#)]
async fn event_id_is_the_signed_one(w: &mut NmpWorld, person: String) {
    let event = w
        .republished_event()
        .cloned()
        .expect("this scenario republishes an already-signed event");
    let expected = w
        .staged_signed_event_of(&person)
        .expect("the note this scenario republishes was staged as signed");
    assert_eq!(
        event.id, expected.id,
        "republishing must not recompute an id -- the engine never re-signs"
    );
}

#[then(regex = r#"^nothing identifying me appears anywhere in the payload$"#)]
async fn nothing_identifying_me_in_payload(w: &mut NmpWorld) {
    let event = w
        .republished_event()
        .cloned()
        .expect("this scenario republishes an already-signed event");
    let me = w.pubkey_hex(ME);
    let json = event.as_json();
    assert!(
        !json.contains(&me),
        "the publishing user's identity must not appear in someone else's event: {json}"
    );
}

// ---- what the removals must not have taken with them --------------------

/// Fail-closed transferred; the privacy FRAMING did not. A group host and an
/// archive relay are public targets, and a journal row describing that write
/// as "private" would be lying.
#[then(regex = r#"^nothing describes that write as private$"#)]
async fn nothing_describes_the_write_as_private(w: &mut NmpWorld) {
    let described: Vec<String> = w
        .receipt_statuses()
        .iter()
        .map(|s| format!("{s:?}"))
        .filter(|s| s.to_lowercase().contains("private"))
        .collect();
    assert!(
        described.is_empty(),
        "an exact route is not a privacy claim, but the receipt said: {described:?}"
    );
}
