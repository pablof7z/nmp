//! Assertions about WHO a write published as, and what happened when the key
//! it named could not currently sign.
//!
//! A different domain from [`super::writes`], which is about where a write
//! went and what its receipt said about delivery. Both read the receipt
//! stream; only one of them is about authorship. Keeping them apart is what
//! lets a reader ask "who signed this?" and find one file.
//!
//! Two channels carry every claim here, and the split is deliberate:
//!
//! - **the relay** answers "the published event is authored by X" and
//!   "<relay> received it". An app pointing at "the published event" means
//!   the thing the world actually received, so that is what these read.
//! - **the receipt** answers everything about the write before it is a
//!   published event at all: refused, accepted, parked awaiting a named key,
//!   cancelled. Those facts exist only there.
//!
//! Plus one that is neither: WHICH signer was asked. A signature in the bytes
//! cannot say it (an already-signed payload has one without any local signer
//! being approached), so a per-key ask counter is the fact, and
//! `world::identity` owns it.

use cucumber::then;

use crate::world::NmpWorld;

// ---- who published it ---------------------------------------------------

#[then(regex = r#"^the published event is authored by "([0-9a-f]{64})"$"#)]
async fn published_event_authored_by(w: &mut NmpWorld, pubkey: String) {
    nothing_to_observe!(
        w.write_reported_accepted(None),
        "the write was never even accepted, so no published event exists to have an author"
    );
    assert!(
        w.published_event_authored_by(&pubkey, None),
        "expected the published event to be authored by {pubkey}; the receipt showed {:?}",
        w.identity_receipt_statuses(None)
    );
}

/// The two-publishes form: a scenario that resolved "whoever is active"
/// twice names each write by what it said.
#[then(regex = r#"^"([^"]+)" is authored by "([0-9a-f]{64})"$"#)]
async fn named_write_authored_by(w: &mut NmpWorld, text: String, pubkey: String) {
    nothing_to_observe!(
        w.write_reported_accepted(Some(&text)),
        "the write saying {text:?} was never accepted, so it has no author to be wrong"
    );
    assert!(
        w.published_event_authored_by(&pubkey, Some(&text)),
        "expected {text:?} to be authored by {pubkey}; its receipt showed {:?}",
        w.identity_receipt_statuses(Some(&text))
    );
}

/// Read off the signer itself, not off the receipt: `SigningState::Signed` is
/// a lifecycle beat the engine emits for an already-signed payload too, so it
/// says nothing about whether a local capability was approached.
#[then(regex = r#"^it was signed by that account's signer$"#)]
async fn signed_by_that_accounts_signer(w: &mut NmpWorld) {
    let label = w.current_identity();
    assert!(
        w.signer_was_asked_for(&label),
        "expected {label}'s own signer to have been asked to sign"
    );
}

#[then(regex = r#"^it was signed by the podcast identity's signer$"#)]
async fn signed_by_the_podcast_signer(w: &mut NmpWorld) {
    let label = w.podcast_identity();
    assert!(
        w.signer_was_asked_for(&label),
        "expected the podcast identity's own signer to have been asked to sign"
    );
}

#[then(regex = r#"^"([0-9a-f]{64})" is still the active account$"#)]
async fn still_the_active_account(w: &mut NmpWorld, pubkey: String) {
    assert!(
        w.active_identity_is(&pubkey),
        "publishing as one identity must not re-root the engine onto it"
    );
}

// ---- the pin ------------------------------------------------------------

/// The identity was resolved once, at acceptance, and frozen. Whether the
/// scenario reached here through a switch or a restart, what it is asking is
/// the same: the write is STILL about that one key.
#[then(regex = r#"^the pending write still awaits "([0-9a-f]{64})"$"#)]
async fn pending_write_still_awaits(w: &mut NmpWorld, pubkey: String) {
    assert!(
        w.write_still_pinned_to(&pubkey),
        "expected the accepted write to still target {pubkey}; its receipt showed {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^the write is still awaiting a signer for "([0-9a-f]{64})"$"#)]
async fn still_awaiting_signer_for(w: &mut NmpWorld, pubkey: String) {
    assert!(
        w.write_awaiting_signer_for(&pubkey, None),
        "expected the write to still be parked awaiting a signer for {pubkey}; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^neither "([0-9a-f]{64})" nor "([0-9a-f]{64})" is asked to sign it$"#)]
async fn neither_is_asked_to_sign(w: &mut NmpWorld, first: String, second: String) {
    for pubkey in [first, second] {
        let asked = w.signer_ask_count_for(&pubkey);
        assert_eq!(
            asked, 0,
            "expected {pubkey}'s signer never to be asked, but it was asked {asked} time(s)"
        );
    }
}

#[then(regex = r#"^"([0-9a-f]{64})" is never asked to sign it$"#)]
async fn never_asked_to_sign(w: &mut NmpWorld, pubkey: String) {
    let asked = w.signer_ask_count_for(&pubkey);
    assert_eq!(
        asked, 0,
        "expected {pubkey}'s signer never to be asked, but it was asked {asked} time(s)"
    );
}

/// An event id commits to author, content and timestamp together, so an
/// unchanged id IS an unchanged body.
#[then(regex = r#"^its frozen body is byte-for-byte what it was before the restart$"#)]
async fn frozen_body_unchanged(w: &mut NmpWorld) {
    assert!(
        w.frozen_body_unchanged_across_restart(),
        "the restart re-froze the write's body; a decided identity must be reloaded, \
         never re-resolved"
    );
}

// ---- failing closed -----------------------------------------------------

#[then(regex = r#"^the write is refused for having no identity to publish as$"#)]
async fn refused_for_having_no_identity(w: &mut NmpWorld) {
    assert!(
        w.write_refused_before_acceptance(None),
        "expected the publish door itself to refuse, never to take custody; saw {:?}",
        w.identity_receipt_statuses(None)
    );
    let reason = w
        .write_refusal_reason(None)
        .expect("a refused publish carries the error it refused with");
    assert!(
        reason.contains("active account"),
        "the refusal must say WHICH instruction could not resolve; it said {reason:?}"
    );
}

#[then(regex = r#"^it never reports accepted$"#)]
async fn never_reports_accepted(w: &mut NmpWorld) {
    assert!(
        w.write_never_reported_accepted(None),
        "a refused publish must never be taken into custody; the door answered Ok and the \
         receipt showed {:?}",
        w.identity_receipt_statuses(None)
    );
}

/// Acceptance IS the journal write, and the id a receipt stream carries
/// before it is a pre-acceptance correlation id, not a durable write id.
#[then(regex = r#"^no journal row was written and no write id was allocated$"#)]
async fn nothing_journaled(w: &mut NmpWorld) {
    assert!(
        w.nothing_was_journaled(None),
        "acceptance is the journal write, and it must not have happened; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

/// Not a parsing failure -- a boundary rule. The string is a perfectly valid
/// npub for an identity that really is registered here, and the field still
/// takes a public key and nothing else.
#[then(regex = r#"^the write is refused for not being given a public key$"#)]
async fn refused_for_not_being_a_public_key(w: &mut NmpWorld) {
    assert!(
        w.identity_refusal().is_some(),
        "expected a display form to be refused where a public key belongs"
    );
}

// ---- the park -----------------------------------------------------------

#[then(regex = r#"^the write reports accepted$"#)]
async fn write_reports_accepted(w: &mut NmpWorld) {
    assert!(
        w.write_reported_accepted(None),
        "expected the write to report Accepted; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^the receipt reports it awaiting a signer for "([0-9a-f]{64})"$"#)]
async fn receipt_reports_awaiting_signer(w: &mut NmpWorld, pubkey: String) {
    assert!(
        w.write_awaiting_signer_for(&pubkey, None),
        "expected the receipt to report AwaitingSigner for {pubkey}; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

/// The failure this rules out is a write that looks live and is actually
/// stuck: the key being waited on is ON the receipt, so an app renders
/// "waiting for your podcast signer" rather than inferring a stall.
#[then(regex = r#"^the receipt names "([0-9a-f]{64})" as the key it is waiting for$"#)]
async fn receipt_names_the_awaited_key(w: &mut NmpWorld, pubkey: String) {
    assert!(
        w.write_awaiting_signer_for(&pubkey, None),
        "expected the park to name {pubkey}; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^the receipt can be reattached by its stable id$"#)]
async fn receipt_reattaches_by_id(w: &mut NmpWorld) {
    assert!(
        w.receipt_reattaches_by_id(),
        "a parked write is a decision the app owns, so its receipt must reattach by id"
    );
}

#[then(regex = r#"^the write is never refused$"#)]
async fn write_is_never_refused(w: &mut NmpWorld) {
    assert!(
        w.write_never_refused(None),
        "waiting for a capability is not an error; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^the write is signed by that signer$"#)]
async fn write_is_signed_by_that_signer(w: &mut NmpWorld) {
    assert!(
        w.write_was_signed(None),
        "expected the parked write to resume and sign; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^the write is reported cancelled$"#)]
async fn write_is_reported_cancelled(w: &mut NmpWorld) {
    assert!(
        w.write_reported_cancelled(None),
        "expected the cancelled write to report Cancelled; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[then(regex = r#"^nothing is signed$"#)]
async fn nothing_is_signed(w: &mut NmpWorld) {
    assert!(
        w.nothing_was_signed(None),
        "a cancelled write must never sign, however late a capability turns up; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

// ---- what reached the relay ---------------------------------------------

#[then(regex = r#"^"([^"]+)" received it$"#)]
async fn relay_received_it(w: &mut NmpWorld, relay: String) {
    assert!(
        w.relay_received_the_write(&relay),
        "expected {relay:?} to receive the published event; the receipt showed {:?}",
        w.identity_receipt_statuses(None)
    );
}

/// Costs its full window: the claim is that nothing arrives, not that nothing
/// has arrived yet.
#[then(regex = r#"^"([^"]+)" received nothing(?: yet)?$"#)]
async fn relay_received_nothing(w: &mut NmpWorld, relay: String) {
    assert!(
        w.relay_received_nothing(&relay),
        "expected {relay:?} to receive nothing at all; the receipt showed {:?}",
        w.identity_receipt_statuses(None)
    );
}
