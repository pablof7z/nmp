//! The group privileges no kind -- and the gate that keeps it that way.

use cucumber::then;

use super::*;

// ---- kind blindness ------------------------------------------------------

#[then(regex = r#"^the group read the kind at no point in that publication$"#)]
async fn group_never_read_the_kind(w: &mut NmpWorld) {
    let door = w.group_surface().door;
    let offenders: Vec<&str> = door
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("Kind") || line.contains("kind"))
        .collect();
    assert!(
        offenders.is_empty(),
        "the group door names no kind at all, but its source says: {offenders:?}"
    );
}

#[then(regex = r#"^the group contributed no part of the kind 9 schema$"#)]
async fn no_part_of_the_chat_schema(w: &mut NmpWorld) {
    let event = delivered(w).await;
    let supplied = w.group_call();
    assert!(
        supplied.named_kind,
        "NOTHING TO OBSERVE -- the scenario did not name the kind, so nothing was left \
         for the group to have contributed to it"
    );
    let contributed: Vec<Vec<String>> = rows(&event)
        .into_iter()
        .filter(|row| row.first().map(String::as_str) != Some("h"))
        .collect();
    assert!(
        contributed.is_empty(),
        "the group's whole contribution is the h row, but it also added {contributed:?}"
    );
}

#[then(regex = r#"^the group exposes no composer for kind 9$"#)]
async fn no_composer_for_chat(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert!(
        !surface.composer_kinds.contains(&9),
        "kind 9 chat is nmp-nipc7's; the NIP-29 composers bind {:?}",
        surface.composer_kinds
    );
}

#[then(
    regex = r#"^the delivered event differs from the one I supplied only by an appended h tag$"#
)]
async fn differs_only_by_appended_h(w: &mut NmpWorld) {
    let supplied = w
        .supplied_draft()
        .cloned()
        .expect("this scenario supplies its own draft");
    let event = delivered(w).await;
    let mut expected: Vec<Vec<String>> = supplied
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect();
    expected.push(vec!["h".to_string(), w.group_host_group_id()]);
    assert_eq!(
        rows(&event),
        expected,
        "the group appends one h row to the end and changes nothing else"
    );
}

#[then(regex = r#"^its kind, content and created_at survive unchanged$"#)]
async fn kind_content_created_at_survive(w: &mut NmpWorld) {
    let supplied = w
        .supplied_draft()
        .cloned()
        .expect("this scenario supplies its own draft");
    let event = delivered(w).await;
    assert_eq!(event.kind, supplied.kind);
    assert_eq!(event.content, supplied.content);
    assert_eq!(
        Some(event.created_at),
        supplied.created_at,
        "an app-chosen created_at is kept verbatim, never restamped"
    );
}

#[then(regex = r#"^every tag I supplied survives unchanged and in the order I gave it$"#)]
async fn supplied_tags_survive_in_order(w: &mut NmpWorld) {
    let supplied = w
        .supplied_draft()
        .cloned()
        .expect("this scenario supplies its own draft");
    assert!(
        !supplied.tags.is_empty(),
        "NOTHING TO OBSERVE -- the scenario supplied no tag, so order is preserved vacuously"
    );
    let event = delivered(w).await;
    let delivered_rows = rows(&event);
    let supplied_rows: Vec<Vec<String>> = supplied
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect();
    assert_eq!(
        delivered_rows[..supplied_rows.len()],
        supplied_rows[..],
        "the caller's own rows come first, in the caller's own order"
    );
}

#[then(regex = r#"^the publication was not refused for being an unrecognised kind$"#)]
async fn not_refused_for_the_kind(w: &mut NmpWorld) {
    assert!(
        w.group_refusal().is_none(),
        "an unfamiliar kind is published, not questioned; the door refused it: {:?}",
        w.group_refusal()
    );
}
