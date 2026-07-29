//! The rows NIP-29's own named operations carry.

use cucumber::then;

use super::*;

// ---- NIP-29's own operations: the rows each one carries ------------------
//
// One assertion family per tag NIP-29 defines, all reading the SAME delivered
// event the routing claims above read. An app never spells these rows, so
// these are the only place their shape is checked end to end.

#[then(regex = r#"^the published event carries a code tag with value "([^"]+)"$"#)]
async fn published_carries_code(w: &mut NmpWorld, value: String) {
    let event = delivered(w).await;
    assert_eq!(values_of(&event, "code"), vec![value]);
}

#[then(regex = r#"^the published event carries no code tag$"#)]
async fn published_carries_no_code(w: &mut NmpWorld) {
    let event = delivered(w).await;
    assert!(
        values_of(&event, "code").is_empty(),
        "an absent invite code emits no code row at all; rows were {:?}",
        rows(&event)
    );
}

#[then(regex = r#"^the published event carries no empty tag$"#)]
async fn published_carries_no_empty_tag(w: &mut NmpWorld) {
    let event = delivered(w).await;
    let empty: Vec<Vec<String>> = rows(&event)
        .into_iter()
        .filter(|row| row.is_empty() || row.iter().any(String::is_empty))
        .collect();
    assert!(
        empty.is_empty(),
        "an omitted field emits no row, never an empty one; found {empty:?}"
    );
}

#[then(regex = r#"^the published event carries a p tag naming "([0-9a-fA-F]{64})"$"#)]
async fn published_carries_p(w: &mut NmpWorld, person: String) {
    let expected = w.pubkey_hex(&person);
    let event = delivered(w).await;
    assert_eq!(
        values_of(&event, "p"),
        vec![expected],
        "rows were {:?}",
        rows(&event)
    );
}

#[then(
    regex = r#"^the published event carries a p tag naming "([0-9a-fA-F]{64})" with role "([^"]+)"$"#
)]
async fn published_carries_p_with_role(w: &mut NmpWorld, person: String, role: String) {
    let expected = w.pubkey_hex(&person);
    let event = delivered(w).await;
    let row = rows(&event)
        .into_iter()
        .find(|row| row.first().map(String::as_str) == Some("p"))
        .unwrap_or_else(|| panic!("no p row at all; rows were {:?}", rows(&event)));
    assert_eq!(row, vec!["p".to_string(), expected, role]);
}

#[then(regex = r#"^the published event carries a name tag with value "([^"]+)"$"#)]
async fn published_carries_name(w: &mut NmpWorld, value: String) {
    let event = delivered(w).await;
    assert_eq!(values_of(&event, "name"), vec![value]);
}

#[then(regex = r#"^the published event carries an about tag with value "([^"]+)"$"#)]
async fn published_carries_about(w: &mut NmpWorld, value: String) {
    let event = delivered(w).await;
    assert_eq!(values_of(&event, "about"), vec![value]);
}

#[then(regex = r#"^the published event carries no about tag$"#)]
async fn published_carries_no_about(w: &mut NmpWorld) {
    let event = delivered(w).await;
    assert!(
        values_of(&event, "about").is_empty(),
        "an omitted field is left untouched, never cleared; rows were {:?}",
        rows(&event)
    );
}
