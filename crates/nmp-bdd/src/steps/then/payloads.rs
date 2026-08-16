//! Assertions about the PAYLOAD a publish carried: what NMP filled in, what
//! the app said and NMP left alone, and what an already-signed event's bytes
//! still were when they reached the far side.
//!
//! A different domain from [`super::writes`], which asks where a publish went
//! and what its receipt said about delivery, and from [`super::identity`],
//! which asks who it published as. Both of those are about the write; this
//! family is about the EVENT -- its kind, its timestamp, its tags, its bytes.
//! Keeping it apart is what lets a reader ask "what did NMP put in the event?"
//! and find one file.
//!
//! Two channels answer everything here, and the split is the same one the
//! identity family draws:
//!
//! - **the relay** answers every claim about the event itself, because an app
//!   pointing at "the published event" means the thing the world received;
//! - **the receipt** answers the claims that exist before there is a published
//!   event at all -- accepted, refused, refused for THIS reason.

use cucumber::then;

use nmp_engine::publish_queue::{RelayState, SigningState, WriteFact, WriteOutcome};
use nostr::JsonUtil;

use crate::world::{format_stated_time, NmpWorld};

// ---- what NMP filled in --------------------------------------------------

#[then(regex = r#"^the published event has kind (\d+)$"#)]
async fn published_event_has_kind(w: &mut NmpWorld, kind: u16) {
    let last = last_index(w);
    nothing_to_observe!(
        w.composed_accepted(last),
        "the write was never even accepted, so no published event exists to have a kind"
    );
    let event = w.composed_event(last);
    assert_eq!(
        event.kind.as_u16(),
        kind,
        "a builder's kind is the one thing NMP may not invent or override"
    );
}

#[then(regex = r#"^the published event carries a created_at, an id and a signature$"#)]
async fn published_event_carries_the_derived_fields(w: &mut NmpWorld) {
    let last = last_index(w);
    nothing_to_observe!(
        w.composed_accepted(last),
        "the write was never even accepted, so nothing was stamped"
    );
    let event = w.composed_event(last);
    assert!(
        event.created_at.as_secs() > 0,
        "an event NMP stamped carries a real created_at, not the epoch"
    );
    event
        .verify()
        .expect("the id and signature NMP derived must verify against the bytes it froze");
}

/// A claim about the VALUE the app handed over, not about the result. A
/// builder has no pubkey, id or signature field to have stated -- that is the
/// type's whole point -- so the only one that could have been stated is the
/// timestamp, and its absence is what this asserts.
#[then(regex = r#"^I never stated my own pubkey, created_at, id or signature$"#)]
async fn i_stated_none_of_them(w: &mut NmpWorld) {
    assert!(
        w.last_builder_stated_no_timestamp(),
        "this scenario is about what NMP fills in when the app said nothing, and the \
         builder it handed over stated a created_at"
    );
}

#[then(regex = r#"^the published event's created_at is "([^"]+)"$"#)]
async fn published_created_at_is(w: &mut NmpWorld, at: String) {
    let last = last_index(w);
    nothing_to_observe!(
        w.composed_accepted(last),
        "the write was never even accepted, so nothing was stamped"
    );
    let event = w.composed_event(last);
    assert_eq!(
        format_stated_time(event.created_at),
        at,
        "acceptance is the moment the body is frozen, so the stamp is the time acceptance \
         happened -- not compose time and not the time the relay took it"
    );
}

// ---- what the app can still say -----------------------------------------

#[then(regex = r#"^the published event carries exactly those tags, in that order, unchanged$"#)]
async fn published_tags_are_exactly_those(w: &mut NmpWorld) {
    let last = last_index(w);
    nothing_to_observe!(
        w.composed_accepted(last),
        "the write was never even accepted, so no published event carries any tags"
    );
    let expected = w.last_builder_tags();
    nothing_to_observe!(
        !expected.is_empty(),
        "the builder under test carried no tags at all, so 'unchanged' is vacuous"
    );
    let event = w.composed_event(last);
    let actual: Vec<Vec<String>> = event.tags.iter().map(|tag| tag.clone().to_vec()).collect();
    assert_eq!(
        actual, expected,
        "arbitrary means arbitrary: not reordered, not normalised, not filtered down to \
         the ones some module claims"
    );
}

#[then(regex = r#"^nothing refused it for being an unrecognised kind$"#)]
async fn nothing_refused_the_kind(w: &mut NmpWorld) {
    let last = last_index(w);
    let mut refusals: Vec<String> = w.publish_refusal().into_iter().collect();
    refusals.extend(
        w.composed_statuses(last)
            .iter()
            .filter_map(|s| match s {
                WriteFact::Signing(SigningState::Refused { reason })
                | WriteFact::Relay {
                    state: RelayState::Rejected { reason },
                    ..
                } => Some(reason.clone()),
                WriteFact::Outcome(WriteOutcome::Refused(reason)) => Some(format!("{reason:?}")),
                _ => None,
            })
            .collect::<Vec<_>>(),
    );
    assert!(
        refusals.is_empty(),
        "a builder holds no whitelist of allowed kinds; a kind nobody wrote a module for \
         is published, not refused -- but this one was refused with {refusals:?}"
    );
}

// ---- two composes of the same words --------------------------------------

#[then(regex = r#"^both events are accepted$"#)]
async fn both_events_accepted(w: &mut NmpWorld) {
    assert_eq!(
        w.composed_count(),
        2,
        "this scenario composes exactly twice"
    );
    for index in 0..2 {
        assert!(
            w.composed_accepted(index),
            "expected publish #{index} to report Accepted; saw {:?}",
            w.composed_statuses(index)
        );
    }
}

#[then(regex = r#"^the two events differ only in their created_at, id and signature$"#)]
async fn the_two_events_differ_only_in_the_stamped_fields(w: &mut NmpWorld) {
    let first = w.composed_event(0);
    let second = w.composed_event(1);
    assert_eq!(first.pubkey, second.pubkey, "same author");
    assert_eq!(first.kind, second.kind, "same kind");
    assert_eq!(first.content, second.content, "same content");
    assert_eq!(first.tags.to_vec(), second.tags.to_vec(), "same tags");
    assert_ne!(
        first.created_at, second.created_at,
        "two composes differ in the time NMP stamped them, and differing is what \
         timestamps are for"
    );
    assert_ne!(first.id, second.id, "a different stamp is a different id");
    assert_ne!(
        first.sig, second.sig,
        "a different id is a different signature"
    );
}

#[then(regex = r#"^nothing reported either one as a duplicate of the other$"#)]
async fn neither_reported_as_a_duplicate(w: &mut NmpWorld) {
    for index in 0..w.composed_count() {
        let duplicates: Vec<String> = w
            .composed_statuses(index)
            .iter()
            .map(|s| format!("{s:?}"))
            .filter(|s| s.to_lowercase().contains("duplicate"))
            .collect();
        assert!(
            duplicates.is_empty(),
            "neither compose is a duplicate of the other and nothing is expected to \
             notice a resemblance, but publish #{index} said {duplicates:?}"
        );
    }
}

#[then(regex = r#"^the two events have the same id$"#)]
async fn the_two_events_have_the_same_id(w: &mut NmpWorld) {
    let first = w
        .composed_event_id(0)
        .expect("the first publish must have frozen a body");
    let second = w
        .composed_event_id(1)
        .expect("the second publish must have frozen a body");
    assert_eq!(
        first, second,
        "byte reproducibility is an app-level property with an app-level means of getting \
         it: state the created_at, and the same logical event is the same event"
    );
}

// ---- an already-signed event ---------------------------------------------

#[then(regex = r#"^"([^"]+)" received exactly the bytes I handed over$"#)]
async fn relay_received_exactly_those_bytes(w: &mut NmpWorld, relay: String) {
    assert!(
        w.relay_received_handed_over_bytes(&relay),
        "a signed payload is carried, not composed: it goes on the wire byte for byte"
    );
}

#[then(regex = r#"^"([^"]+)" received it unchanged$"#)]
async fn relay_received_it_unchanged(w: &mut NmpWorld, relay: String) {
    assert!(
        w.relay_received_handed_over_bytes(&relay),
        "expected {relay:?} to receive the handed-over event unchanged; the receipt showed \
         {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^the event it received still has id "([0-9a-f]{64})"$"#)]
async fn received_event_still_has_id(w: &mut NmpWorld, label: String) {
    let expected = w.signed_event_by_label(&label).id;
    let received = w
        .admitted_event_with_id(expected)
        .expect("the relay must have received the event this scenario published");
    assert_eq!(
        received.id, expected,
        "reordering its tags or restamping it would break the id"
    );
}

#[then(regex = r#"^the event it received is still authored by "([0-9a-f]{64})"$"#)]
async fn received_event_still_authored_by(w: &mut NmpWorld, pubkey: String) {
    let expected = w.person(&pubkey).public_key().to_hex();
    let handed = w.handed_over_event();
    let received = w
        .admitted_event_with_id(handed.id)
        .expect("the relay must have received the event this scenario published");
    assert_eq!(
        received.pubkey.to_hex(),
        expected,
        "re-signing it would make it a different event by a different person"
    );
}

#[then(regex = r#"^its created_at, tags and signature are the ones it arrived with$"#)]
async fn received_event_keeps_its_own_fields(w: &mut NmpWorld) {
    let handed = w.handed_over_event();
    let received = w
        .admitted_event_with_id(handed.id)
        .expect("the relay must have received the event this scenario published");
    assert_eq!(
        received.as_json(),
        handed.as_json(),
        "carried, not composed"
    );
}

#[then(regex = r#"^the write is refused for failing verification$"#)]
async fn refused_for_failing_verification(w: &mut NmpWorld) {
    assert!(
        w.write_refused_before_acceptance(None),
        "verified verbatim means verified, and the refusal must come BEFORE acceptance -- \
         `publish()` itself answers Err; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^nothing was refused for want of a current account$"#)]
async fn nothing_refused_for_want_of_an_account(w: &mut NmpWorld) {
    let refusal = w.write_refusal_reason(None);
    assert!(
        refusal.is_none(),
        "a signed event needs no signing provider, so it needs no current account, so being logged \
         out is not a reason to refuse it -- but it was refused with {refusal:?}"
    );
}

#[then(regex = r#"^the write is refused as a consent and author contradiction$"#)]
async fn refused_as_a_consent_contradiction(w: &mut NmpWorld) {
    assert!(
        w.write_refused_before_acceptance(None),
        "there is no resolution that honours both statements, so it fails closed BEFORE \
         acceptance; saw {:?}",
        w.identity_receipt_statuses(None)
    );
    let reason = w
        .write_refusal_reason(None)
        .expect("a refused publish carries the error it refused with");
    assert!(
        reason.contains("does not match"),
        "the refusal must say which two statements contradict; it said {reason:?}"
    );
}

#[then(regex = r#"^the event was not re-signed as "([0-9a-f]{64})"$"#)]
async fn event_was_not_resigned_as(w: &mut NmpWorld, pubkey: String) {
    assert!(
        w.nothing_was_authored_by(&pubkey),
        "restamping the author would invalidate the signature, so nothing may carry these \
         bytes under {pubkey}"
    );
}

#[then(regex = r#"^the write was not refused$"#)]
async fn the_write_was_not_refused(w: &mut NmpWorld) {
    assert!(
        w.write_never_refused(None),
        "naming no identity means the event's own author, whoever that is -- never a \
         mismatch; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

// ---- how many times one relay was offered one event ----------------------

/// An OFFER, not a stored copy: a relay that already holds an event
/// deduplicates it, so counting what it holds could never tell one send from
/// two. This reads the relay's own record of the EVENT frames it was handed,
/// which is the bandwidth the design bounds.
#[then(regex = r#"^"([^"]+)" was offered the note exactly (once|twice|\d+ times)$"#)]
async fn relay_was_offered_exactly(w: &mut NmpWorld, relay: String, count: String) {
    let expected = match count.as_str() {
        "once" => 1,
        "twice" => 2,
        other => other
            .trim_end_matches(" times")
            .parse()
            .expect("nmp-bdd: an offer count is a number"),
    };
    let id = w
        .last_published_id()
        .expect("the note must have been signed to have been offered to anything");
    nothing_to_observe!(
        w.wait_for_offer(&relay, id).await,
        "{relay:?} was never offered this note at all, so counting its offers would pass \
         however many resolutions ran"
    );
    assert_eq!(
        w.offers_of(&relay, id),
        expected,
        "an acked destination is terminal and untouched by any later resolution, however \
         many times the strategy runs"
    );
}

/// The last publish this scenario composed. Every claim above is about "the
/// published event", which in a one-publish scenario is unambiguous and in a
/// two-publish one is the one the sentence just before it made.
fn last_index(w: &NmpWorld) -> usize {
    w.composed_count()
        .checked_sub(1)
        .expect("nmp-bdd: nothing was composed in this scenario")
}
