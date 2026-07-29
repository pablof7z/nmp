//! `When` — replacing a whole-value event, and the other device that moves
//! the winner while you are doing it.
//!
//! Its own file next to `when::writes` for the reason `then/` is a directory:
//! this family's vocabulary ("naming X as the version it replaces") is
//! `features/writes/replaceable-edits.feature`'s alone.

use cucumber::{given, when};

use nmp_grammar::Identity;

use crate::world::{parse_stated_time, NmpWorld};

// ---- what the store already holds ---------------------------------------
//
// Stated as `Given`s because they are the world before the app acts, even
// though each one really does publish: a winner that the engine did not put
// there is not a winner a compare-and-swap can meaningfully be checked
// against. See `world::replaceable` for the two harness decisions this makes.

#[given(regex = r#"^my contact list "([0-9a-f]{64})" created at "([^"]+)" is the stored winner$"#)]
async fn my_contact_list_is_the_winner(w: &mut NmpWorld, label: String, at: String) {
    let me = w.current_identity();
    w.stage_stored_winner(&me, &label, parse_stated_time(&at))
        .await;
}

#[given(regex = r#"^another device replaced it with "([0-9a-f]{64})" created at "([^"]+)"$"#)]
async fn another_device_replaced_it(w: &mut NmpWorld, label: String, at: String) {
    let me = w.current_identity();
    w.stage_stored_winner(&me, &label, parse_stated_time(&at))
        .await;
}

/// The same move, made while my own replacement is composed and not yet
/// published. A `When` rather than a `Given` because the ORDER is the point:
/// the app's read was correct when it happened, and the winner moved
/// afterwards.
#[when(regex = r#"^another device replaces it with "([0-9a-f]{64})" before my write is accepted$"#)]
async fn another_device_replaces_it_mid_flight(w: &mut NmpWorld, label: String) {
    let me = w.current_identity();
    let at = w
        .stated_clock()
        .expect("nmp-bdd: this scenario states the device clock before anything moves");
    w.stage_stored_winner(&me, &label, at).await;
}

#[given(regex = r#"^"([0-9a-f]{64})"'s contact list "([0-9a-f]{64})" is stored locally$"#)]
async fn foreign_contact_list_is_stored(w: &mut NmpWorld, owner: String, label: String) {
    w.observe_foreign_contact_list(&owner, &label).await;
}

#[given(
    regex = r#"^that identity's contact list "([0-9a-f]{64})" created at "([^"]+)" is its stored winner$"#
)]
async fn that_identitys_contact_list_is_its_winner(w: &mut NmpWorld, label: String, at: String) {
    let owner = w.podcast_identity();
    w.stage_stored_winner(&owner, &label, parse_stated_time(&at))
        .await;
}

// ---- replacing it --------------------------------------------------------

#[when(
    regex = r#"^I (?:re-read the stored winner and )?publish a replacement(?: contact list)? naming "([0-9a-f]{64})" as the version it replaces$"#
)]
async fn publish_replacement(w: &mut NmpWorld, base: String) {
    w.publish_replacement(Identity::Active, &base, None).await;
}

/// The foot-gun, deliberately left loaded: a caller-stated timestamp is kept
/// verbatim, including one that regresses below the winner and loses.
#[when(
    regex = r#"^I publish a replacement contact list created at "([^"]+)" naming "([0-9a-f]{64})" as the version it replaces$"#
)]
async fn publish_replacement_with_created_at(w: &mut NmpWorld, at: String, base: String) {
    w.publish_replacement(Identity::Active, &base, Some(parse_stated_time(&at)))
        .await;
}

/// Which coordinate gets checked is decided by the same identity resolution
/// that decides the author.
#[when(
    regex = r#"^I publish a replacement contact list naming identity "([0-9a-f]{64})" and "([0-9a-f]{64})" as the version it replaces$"#
)]
async fn publish_replacement_naming_identity(w: &mut NmpWorld, pubkey: String, base: String) {
    let key = w.person(&pubkey).public_key();
    w.publish_replacement(Identity::Explicit(key), &base, None)
        .await;
}

#[when(
    regex = r#"^I read the stored winner and compose a replacement naming "([0-9a-f]{64})" as the version it replaces$"#
)]
async fn compose_replacement(w: &mut NmpWorld, base: String) {
    let me = w.current_identity();
    let winner = w.stored_winner_of(&me);
    assert_eq!(
        winner,
        Some(w.id_of(&base)),
        "nmp-bdd: this scenario's app READ the winner, so the version it composes against \
         has to be the one that was really there"
    );
    w.stage_replacement(Identity::Active, &base, None);
}

#[when(regex = r#"^I publish that replacement$"#)]
async fn publish_staged_replacement(w: &mut NmpWorld) {
    w.publish_staged_replacement().await;
}
