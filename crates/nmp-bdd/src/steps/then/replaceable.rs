//! Assertions about a whole-value REPLACEMENT: which version the store holds,
//! what the conflict named when the winner had moved, and what timestamp the
//! acceptance transaction decided on.
//!
//! A different domain from [`super::payloads`], which is about the event a
//! publish carried, and from [`super::writes`], which is about where it went.
//! Everything here is about a ROW that existed before this write did.
//!
//! The winner is read through an ordinary subscription (see
//! `world::replaceable`), never out of the store directly: what an app can
//! see is what the specification is allowed to claim.

use cucumber::then;

use crate::world::{format_stated_time, NmpWorld};

// ---- the precondition ----------------------------------------------------

/// Also read by `features/diagnostics/stalled-writes.feature`, whose subject
/// is a write nothing can deliver rather than a replacement: "the obligation
/// was accepted" is the same observable in both, and a `WriteStatus::Accepted`
/// is what both mean. Narrowing this to something replaceable-specific would
/// silently break that feature, so it stays the plain acceptance fact.
#[then(regex = r#"^the write is accepted$"#)]
async fn the_write_is_accepted(w: &mut NmpWorld) {
    assert!(
        w.replacement_accepted(),
        "expected the replacement to be accepted; saw {:?}",
        w.receipt_statuses()
    );
}

#[then(regex = r#"^the write is refused with a replaceable conflict$"#)]
async fn refused_with_a_replaceable_conflict(w: &mut NmpWorld) {
    nothing_to_observe!(
        w.receipt_eventually(|seen| !seen.is_empty()),
        "the publish reported no status at all, so it was neither refused nor accepted"
    );
    assert!(
        w.replacement_conflicted(),
        "a stale base is refused with a typed conflict and never silently applied on top; \
         saw {:?}",
        w.receipt_statuses()
    );
}

#[then(
    regex = r#"^the conflict names "([0-9a-f]{64})" as expected and "([0-9a-f]{64})" as actual$"#
)]
async fn the_conflict_names(w: &mut NmpWorld, expected: String, actual: String) {
    assert!(
        w.conflict_names(&expected, &actual),
        "the refusal has to say what was expected and what is actually there, or the app \
         has nothing to re-read against; saw {:?}",
        w.receipt_statuses()
    );
}

// ---- the row -------------------------------------------------------------

#[then(regex = r#"^the replacement is the stored winner$"#)]
async fn the_replacement_is_the_winner(w: &mut NmpWorld) {
    let me = w.current_identity();
    assert!(
        w.replacement_is_the_winner(&me),
        "an accepted replacement is the value at that coordinate; saw {:?}",
        w.stored_winner_of(&me)
    );
}

#[then(regex = r#"^the replacement is the stored winner for "([0-9a-f]{64})"$"#)]
async fn the_replacement_is_the_winner_for(w: &mut NmpWorld, pubkey: String) {
    assert!(
        w.replacement_is_the_winner(&pubkey),
        "the coordinate CAS-ed is the one the write publishes as; saw {:?}",
        w.stored_winner_of(&pubkey)
    );
}

#[then(regex = r#"^the stored winner is still "([0-9a-f]{64})"$"#)]
async fn the_stored_winner_is_still(w: &mut NmpWorld, label: String) {
    let me = w.current_identity();
    assert!(
        w.stored_winner_is(&me, &label),
        "a refused replacement changes nothing; saw {:?}",
        w.stored_winner_of(&me)
    );
}

#[then(regex = r#"^my own contact list is still "([0-9a-f]{64})"$"#)]
async fn my_own_contact_list_is_still(w: &mut NmpWorld, label: String) {
    let me = w.current_identity();
    assert!(
        w.stored_winner_is(&me, &label),
        "publishing as one identity must not CAS against another's row; saw {:?}",
        w.stored_winner_of(&me)
    );
}

#[then(regex = r#"^"([0-9a-f]{64})"'s contact list is unchanged$"#)]
async fn foreign_contact_list_unchanged(w: &mut NmpWorld, pubkey: String) {
    let label = w.only_foreign_contact_list_label();
    assert!(
        w.stored_winner_is(&pubkey, &label),
        "another author's event is never the winner at MY coordinate, so nothing this \
         write did may have touched theirs; saw {:?}",
        w.stored_winner_of(&pubkey)
    );
}

#[then(regex = r#"^nothing was journaled and no event id was allocated$"#)]
async fn nothing_journaled_and_no_id(w: &mut NmpWorld) {
    let statuses = w.receipt_statuses();
    assert!(
        !statuses
            .iter()
            .any(|s| matches!(s, nmp::mechanism::publish_queue::WriteStatus::Accepted)),
        "the precondition is checked BEFORE an intent or receipt id is allocated; saw \
         {statuses:?}"
    );
}

// ---- the stamp -----------------------------------------------------------

#[then(regex = r#"^the replacement's created_at is "([^"]+)"$"#)]
async fn the_replacements_created_at_is(w: &mut NmpWorld, at: String) {
    let created_at = w
        .replacement_created_at()
        .expect("the replacement must have been accepted to have a created_at");
    assert_eq!(
        format_stated_time(created_at),
        at,
        "the stamp is decided inside the acceptance transaction, against the row the \
         precondition is holding"
    );
}

#[then(regex = r#"^the replacement's created_at is greater than "([0-9a-f]{64})"'s$"#)]
async fn the_replacements_created_at_is_greater(w: &mut NmpWorld, label: String) {
    let base = w
        .created_at_of(&label)
        .expect("the version this replaces must be known to compare against");
    let created_at = w
        .replacement_created_at()
        .expect("the replacement must have been accepted to have a created_at");
    assert!(
        created_at > base,
        "a stale base cannot produce a stale stamp: {created_at:?} must be after {base:?}"
    );
}

#[then(regex = r#"^nothing restamped it to "([^"]+)"$"#)]
async fn nothing_restamped_it(w: &mut NmpWorld, at: String) {
    let created_at = w
        .replacement_created_at()
        .expect("the replacement must have been accepted to have a created_at");
    assert_ne!(
        format_stated_time(created_at),
        at,
        "present-then-changed is the one thing a stated field may never be, even when \
         keeping it loses the race"
    );
}

// ---- what reached the relay ---------------------------------------------

#[then(regex = r#"^"([^"]+)" received the replacement$"#)]
async fn relay_received_the_replacement(w: &mut NmpWorld, relay: String) {
    let id = w
        .replacement_id()
        .expect("the replacement must have been accepted to have reached anything");
    assert!(
        w.await_admitted_event_at(&relay, id).is_some(),
        "an accepted replacement is routed like any other write; the receipt showed {:?}",
        w.receipt_statuses()
    );
}
