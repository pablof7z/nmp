//! What the door refuses, and how a caller error differs from a relay rejection.

use cucumber::then;

use super::*;

// ---- refusals ------------------------------------------------------------

#[then(regex = r#"^the publication is refused with a typed missing-group-context error$"#)]
async fn refused_missing_context(w: &mut NmpWorld) {
    assert_refusal(w, "MissingContext");
}

#[then(regex = r#"^the publication is refused with a typed mismatched-group-context error$"#)]
async fn refused_mismatched_context(w: &mut NmpWorld) {
    assert_refusal(w, "MismatchedContext");
}

#[then(regex = r#"^the publication is refused with a typed ambiguous-group-context error$"#)]
async fn refused_ambiguous_context(w: &mut NmpWorld) {
    assert_refusal(w, "AmbiguousContext");
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

#[then(regex = r#"^the publication is refused with a typed caller-supplied-previous error$"#)]
async fn refused_caller_supplied_previous(w: &mut NmpWorld) {
    assert_refusal(w, "CallerSuppliedTimeline");
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

#[then(regex = r#"^the error names the previous tag$"#)]
async fn error_names_the_previous_tag(w: &mut NmpWorld) {
    let said = w
        .group_refusal()
        .expect("expected a typed refusal")
        .to_string();
    assert!(
        said.contains("'previous'"),
        "the refusal must name the tag: {said}"
    );
}

#[then(regex = r#"^the error names both "([^"]+)" and "([^"]+)"$"#)]
async fn error_names_both(w: &mut NmpWorld, found: String, expected: String) {
    let said = w
        .group_refusal()
        .expect("expected a typed refusal")
        .to_string();
    assert!(
        said.contains(&found) && said.contains(&expected),
        "the refusal must name the group it found AND the one it was published through: {said}"
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

#[then(regex = r#"^no h tag was appended to it$"#)]
async fn no_h_was_appended(w: &mut NmpWorld) {
    let supplied = w.signed_event();
    assert!(
        !supplied
            .tags
            .iter()
            .any(|tag| tag.as_slice().first().map(String::as_str) == Some("h")),
        "the refused event must be exactly as it arrived"
    );
}

#[then(regex = r#"^its id was never recomputed$"#)]
async fn id_never_recomputed(w: &mut NmpWorld) {
    let event = w.signed_event();
    assert_eq!(
        event_id_over(&event, event.tags.iter().cloned().collect()),
        event.id,
        "the refused event still hashes to the id its author signed"
    );
}

#[then(regex = r#"^neither tag was stripped from the event I supplied$"#)]
async fn neither_tag_stripped(w: &mut NmpWorld) {
    let supplied = w
        .supplied_draft()
        .cloned()
        .expect("this scenario supplies its own draft");
    let names: Vec<String> = supplied
        .tags
        .iter()
        .filter_map(|tag| tag.as_slice().first().cloned())
        .collect();
    assert!(
        names.iter().any(|n| n == "h") && names.iter().any(|n| n == "previous"),
        "the door refuses; it never trims. The draft still carries both, got {names:?}"
    );
}

#[then(regex = r#"^the delivered event carries no previous tag$"#)]
async fn delivered_carries_no_previous(w: &mut NmpWorld) {
    let event = delivered(w).await;
    assert!(
        values_of(&event, "previous").is_empty(),
        "the group never mints a previous row"
    );
}

#[then(regex = r#"^no surface anywhere can mint a previous tag for a group publication$"#)]
async fn nothing_can_mint_previous(w: &mut NmpWorld) {
    let (passed, said) = w.gate_outcome_now();
    assert!(
        passed,
        "the ownership gate is what forbids a caller-mintable previous authority: {said}"
    );
}
