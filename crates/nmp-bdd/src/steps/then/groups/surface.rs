//! The door's DECLARED shape -- what no group operation accepts, and what NIP-29 does and does not compose.

use cucumber::then;

use super::*;

// ---- the door's declared shape ------------------------------------------

#[then(regex = r#"^no group write operation accepts a relay$"#)]
async fn no_write_accepts_a_relay(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert_no_parameter(&surface, &["RelayUrl", "relay"], "a relay");
}

#[then(regex = r#"^no group write operation accepts a routing value$"#)]
async fn no_write_accepts_a_route(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert_no_parameter(&surface, &["WriteRouting", "routing"], "a routing value");
}

#[then(regex = r#"^no group write operation accepts an h value$"#)]
async fn no_write_accepts_an_h(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert_no_parameter(&surface, &["group_id", "context_tag", " h:"], "an h value");
}

#[then(regex = r#"^the group id given at construction is the only source of the h tag$"#)]
async fn construction_is_the_only_source_of_h(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert!(
        surface
            .door
            .contains("pub fn new(host: RelayUrl, group_id: impl Into<String>)"),
        "the group id enters at construction and nowhere else"
    );
    assert_no_parameter(&surface, &["group_id"], "an h value");
}

#[then(regex = r#"^it offers operations only for kinds NIP-29 itself defines$"#)]
async fn only_nip29_kinds(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert!(
        !surface.composer_kinds.is_empty(),
        "NOTHING TO OBSERVE -- no composer kind was found at all"
    );
    let alien: Vec<u16> = surface
        .composer_kinds
        .iter()
        .copied()
        .filter(|kind| !(9000..=9022).contains(kind))
        .collect();
    assert!(alien.is_empty(), "NIP-29 defines 9000-9022, not {alien:?}");
}

#[then(regex = r#"^it offers no chat composer$"#)]
async fn no_chat_composer(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert!(!surface.composer_kinds.contains(&9));
    assert!(
        !surface
            .composer_fns
            .iter()
            .any(|f| f.contains("chat") || f.contains("message") || f.contains("reply")),
        "kind 9 chat is nmp-nipc7's; these composers exist: {:?}",
        surface.composer_fns
    );
}

#[then(regex = r#"^it offers no reaction composer$"#)]
async fn no_reaction_composer(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert!(!surface.composer_kinds.contains(&7));
    assert!(
        !surface.composer_fns.iter().any(|f| f.contains("react")),
        "kind 7 reactions are NIP-25's; these composers exist: {:?}",
        surface.composer_fns
    );
}

#[then(
    regex = r#"^an app that wants either builds the event itself and publishes it through the group$"#
)]
async fn an_app_builds_it_itself(w: &mut NmpWorld) {
    let surface = w.group_surface();
    assert!(
        surface
            .write_signatures
            .iter()
            .any(|sig| sig.starts_with("fn publish(") && sig.contains("EventBuilder")),
        "the kind-blind door is what an app uses for a kind NIP-29 does not define; \
         the trait declares {:?}",
        surface.write_signatures
    );
}
