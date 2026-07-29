//! The signing boundary: the h row is inside the bytes that were signed.

use cucumber::then;

use super::*;

// ---- signing -------------------------------------------------------------

#[then(regex = r#"^the signer was asked to sign exactly once$"#)]
async fn signer_asked_once(w: &mut NmpWorld) {
    settled(w).await;
    assert_eq!(w.signer_ask_count(), 1);
}

#[then(regex = r#"^the event handed to the signer already carried h "([^"]+)"$"#)]
async fn signer_saw_the_context(w: &mut NmpWorld, group_id: String) {
    let event = delivered(w).await;
    assert_eq!(values_of(&event, "h"), vec![group_id]);
    event
        .verify()
        .expect("the signature must cover the h row, which it only can if it was there first");
}

#[then(
    regex = r#"^(?:no tag was added to the event after it was signed|the signature verifies over those exact bytes)$"#
)]
async fn nothing_added_after_signing(w: &mut NmpWorld) {
    let event = delivered(w).await;
    event
        .verify()
        .expect("a tag added after signing would break the signature");
}

#[then(
    regex = r#"^recomputing the event id over the delivered event reproduces the id it was delivered with$"#
)]
async fn recomputing_the_id_agrees(w: &mut NmpWorld) {
    let event = delivered(w).await;
    assert_eq!(
        event_id_over(&event, event.tags.iter().cloned().collect()),
        event.id
    );
}

#[then(regex = r#"^removing the h tag from the delivered event changes its id$"#)]
async fn removing_h_changes_the_id(w: &mut NmpWorld) {
    let event = delivered(w).await;
    let without: Vec<nostr::Tag> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) != Some("h"))
        .cloned()
        .collect();
    assert_ne!(
        without.len(),
        event.tags.len(),
        "NOTHING TO OBSERVE -- the delivered event carries no h row to remove"
    );
    assert_ne!(
        event_id_over(&event, without),
        event.id,
        "the h row is inside the signed bytes, so removing it must move the id"
    );
}

#[then(regex = r#"^the h tag was present in the bytes that were signed$"#)]
async fn h_was_in_the_signed_bytes(w: &mut NmpWorld) {
    let event = delivered(w).await;
    assert_eq!(values_of(&event, "h").len(), 1);
    event.verify().expect("the signature must cover the h row");
}

#[then(regex = r#"^the failure is reported as a signing failure, not as a routing failure$"#)]
async fn failure_is_a_signing_failure(w: &mut NmpWorld) {
    let reported = w.receipt_eventually(|seen| {
        seen.iter().any(
            |s| matches!(s, WriteStatus::Failed(reason) if reason.to_lowercase().contains("sign")),
        )
    });
    assert!(
        reported,
        "expected a signer refusal on the receipt, saw {:?}",
        w.receipt_statuses()
    );
    assert!(
        !w.receipt_statuses()
            .iter()
            .any(|s| matches!(s, WriteStatus::Routed(relays) if relays.is_empty())),
        "an explicit route resolved fine; the failure is the signer's"
    );
}
