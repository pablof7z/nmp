//! The pre-signed path: bytes and event id preserved exactly.

use cucumber::then;

use nostr::JsonUtil;

use super::route::not_rerouted;

use super::*;

// ---- the pre-signed path -------------------------------------------------

#[then(regex = r#"^the delivered event has id "([^"]+)"$"#)]
async fn delivered_event_has_id(w: &mut NmpWorld, label: String) {
    let expected = w.labelled_id(&label);
    let event = delivered(w).await;
    assert_eq!(
        event.id, expected,
        "a pre-signed event is published byte for byte, so its id cannot move"
    );
}

#[then(regex = r#"^its signature is byte-identical to the one I supplied$"#)]
async fn signature_is_identical(w: &mut NmpWorld) {
    let supplied = w.signed_event();
    let event = delivered(w).await;
    assert_eq!(event.sig, supplied.sig);
    assert_eq!(event.as_json(), supplied.as_json());
}

#[then(regex = r#"^no tag was added, removed or reordered$"#)]
async fn no_tag_was_touched(w: &mut NmpWorld) {
    let supplied = w.signed_event();
    let event = delivered(w).await;
    assert_eq!(rows(&event), rows(&supplied));
}

#[then(regex = r#"^the signer was never asked to sign$"#)]
async fn signer_never_asked(w: &mut NmpWorld) {
    settled(w).await;
    assert_eq!(
        w.signer_ask_count(),
        0,
        "an already-signed event needs no signer, and a refused draft never reaches one"
    );
}

#[then(regex = r#"^the query for that id matches the event that reached "([^"]+)"$"#)]
async fn armed_query_matches(w: &mut NmpWorld, relay: String) {
    let delivered_here = w
        .delivered_event_at(&relay)
        .await
        .unwrap_or_else(|| panic!("nothing reached {relay:?}"));
    let id = delivered_here.id;
    let shown = w.feed_eventually(move |rows, _| rows.iter().any(|row| row.id == id));
    assert!(
        shown,
        "the observation armed on the pre-signed id must match the event that was published"
    );
}

#[then(regex = r#"^the event was not re-signed and not re-routed$"#)]
async fn not_resigned_not_rerouted(w: &mut NmpWorld) {
    signer_never_asked(w).await;
    not_rerouted(w).await;
}

#[then(regex = r#"^the signature still belongs to "([0-9a-fA-F]{64})"$"#)]
async fn signature_belongs_to(w: &mut NmpWorld, person: String) {
    let author = w.pubkey_hex(&person);
    let event = delivered(w).await;
    assert_eq!(event.pubkey.to_hex(), author);
    event.verify().expect("the signature must still verify");
}
