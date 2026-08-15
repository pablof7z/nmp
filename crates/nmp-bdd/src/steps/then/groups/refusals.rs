//! What the door refuses, and how a caller error differs from a relay rejection.

use cucumber::then;

use super::*;

// ---- refusals ------------------------------------------------------------

#[then(regex = r#"^the signer was never asked to sign$"#)]
async fn signer_never_asked(w: &mut NmpWorld) {
    settled(w).await;
    assert_eq!(
        w.signer_ask_count(),
        0,
        "a draft the door refused never reaches a signer"
    );
}

#[then(regex = r#"^the publication is refused with a typed caller-supplied-h error$"#)]
async fn refused_caller_supplied_h(w: &mut NmpWorld) {
    assert_refusal(w, "CallerSuppliedContext");
}

/// Asked of the DOOR, not of a run: this scenario publishes nothing, and the
/// claim is that the refusal is unconditional. So the door itself is handed an
/// `h`-carrying draft here and must refuse it.
#[then(regex = r#"^an event that arrives carrying its own h is refused$"#)]
async fn an_h_carrying_event_is_refused(w: &mut NmpWorld) {
    let refusal = w.door_refuses_a_caller_supplied_context().await;
    assert!(
        format!("{refusal:?}").starts_with("CallerSuppliedContext"),
        "the door must refuse a caller's own h row, but said {refusal:?}"
    );
}

#[then(regex = r#"^the publication is refused with a typed error$"#)]
async fn refused_with_a_typed_error(w: &mut NmpWorld) {
    assert!(
        w.group_refusal().is_some(),
        "expected a typed refusal at the door; the publication was accepted instead"
    );
}

fn assert_refusal(w: &mut NmpWorld, variant: &str) {
    let refusal = w
        .group_refusal()
        .expect("expected a typed refusal at the door; the publication was accepted instead");
    let named = format!("{refusal:?}");
    assert!(
        named.starts_with(variant),
        "expected a {variant} refusal, got {named}"
    );
}

#[then(regex = r#"^the error names the h tag$"#)]
async fn error_names_the_h_tag(w: &mut NmpWorld) {
    let said = w
        .group_refusal()
        .expect("expected a typed refusal")
        .to_string();
    assert!(
        said.contains("'h'"),
        "the refusal must name the tag: {said}"
    );
}

#[then(regex = r#"^the refusal is the same error as for a matching h$"#)]
async fn same_refusal_as_matching_h(w: &mut NmpWorld) {
    assert_refusal(w, "CallerSuppliedContext");
}

#[then(regex = r#"^(?:no write intent was accepted|no receipt was created for it)$"#)]
async fn no_intent_accepted(w: &mut NmpWorld) {
    assert_eq!(
        w.receipt_count(),
        0,
        "a refusal at the door never reaches the publish door, so no receipt exists"
    );
}

#[then(regex = r#"^the refusal is reported as a caller error, not as a relay rejection$"#)]
async fn refusal_is_a_caller_error(w: &mut NmpWorld) {
    assert!(
        w.group_refusal().is_some(),
        "expected a typed refusal at the door"
    );
    assert_eq!(
        w.receipt_count(),
        0,
        "a caller error has no receipt to carry a relay's rejection"
    );
}
