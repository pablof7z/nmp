//! `When` — the NIP-29 `Group` door.
//!
//! Every write below goes through the REAL product door -- INHERENT methods
//! on `nmp_nip29::Group` -- and every read through `group.read(filter)` (one
//! ordinary `LiveQuery`) and the ordinary subscription call. The harness
//! reimplements neither, because both are the thing under test.
//!
//! Every write also freezes an exact decoded `author` (#878): the group never
//! resolves "whoever happens to be active" on its own, so each step resolves
//! the scenario's own logged-in identity through `NmpWorld::me_pubkey` and
//! hands it over explicitly, exactly as an app would.
//!
//! Each step also records what IT named (`GroupCall`), because "I named no
//! relay and no tag on that call" is a claim about the app's words and is
//! unrecoverable from the intent afterwards.

use cucumber::when;

use crate::world::{parse_kind_list, GroupCall, NmpWorld};

/// The kind and the content are the app's; nothing else is.
#[when(regex = r#"^I publish an event of kind (\d+) with content "([^"]+)" through the group$"#)]
async fn publish_kind_through_group(w: &mut NmpWorld, kind: u16, content: String) {
    publish_kind(w, None, kind, content).await;
}

#[when(
    regex = r#"^I publish an event of kind (\d+) with content "([^"]+)" through the group "([^"]+)"$"#
)]
async fn publish_kind_through_named_group(
    w: &mut NmpWorld,
    kind: u16,
    content: String,
    group_id: String,
) {
    publish_kind(w, Some(&group_id), kind, content).await;
}

async fn publish_kind(w: &mut NmpWorld, group_id: Option<&str>, kind: u16, content: String) {
    let builder = nmp::EventBuilder::new(nmp::Kind::from(kind)).content(content);
    let author = w.me_pubkey();
    let call = GroupCall {
        named_kind: true,
        ..GroupCall::default()
    };
    w.group_operation(group_id, call, move |group, engine| {
        group.publish(engine, author, builder)
    })
    .await;
}

/// The draft a `Given` staged, handed over exactly as the app built it.
#[when(regex = r#"^I publish that event through the group$"#)]
async fn publish_staged_draft(w: &mut NmpWorld) {
    let builder = w
        .supplied_draft()
        .cloned()
        .expect("nmp-bdd: no unsigned event has been staged to publish");
    let author = w.me_pubkey();
    let call = GroupCall {
        named_kind: true,
        named_tag: true,
        ..GroupCall::default()
    };
    w.group_operation(None, call, move |group, engine| {
        group.publish(engine, author, builder)
    })
    .await;
}

/// Constructing the identity and stopping. The engine is started first on
/// purpose: an unstarted world contacts nothing for reasons that have nothing
/// to do with the group.
#[when(regex = r#"^I construct the group and do nothing else$"#)]
async fn construct_the_group(w: &mut NmpWorld) {
    w.ensure_started().await;
    let _ = w.group_value(None);
}

#[when(regex = r#"^I publish a join request through the group$"#)]
async fn publish_join_request(w: &mut NmpWorld) {
    join_request(w, None).await;
}

#[when(regex = r#"^I publish a join request with no invite code through the group$"#)]
async fn publish_join_request_no_code(w: &mut NmpWorld) {
    join_request(w, None).await;
}

#[when(regex = r#"^I publish a join request with invite code "([^"]+)" through the group$"#)]
async fn publish_join_request_with_code(w: &mut NmpWorld, code: String) {
    join_request(w, Some(code)).await;
}

async fn join_request(w: &mut NmpWorld, code: Option<String>) {
    let author = w.me_pubkey();
    w.group_operation(None, GroupCall::default(), move |group, engine| {
        group.join_request(engine, author, code.as_deref())
    })
    .await;
}

#[when(regex = r#"^I publish a leave request through the group$"#)]
async fn publish_leave_request(w: &mut NmpWorld) {
    let author = w.me_pubkey();
    w.group_operation(None, GroupCall::default(), move |group, engine| {
        group.leave_request(engine, author)
    })
    .await;
}

#[when(regex = r#"^I add user "([0-9a-fA-F]{64})" to the group$"#)]
async fn add_user(w: &mut NmpWorld, pubkey: String) {
    add_user_with_role(w, pubkey, None).await;
}

#[when(regex = r#"^I add user "([0-9a-fA-F]{64})" to the group with role "([^"]+)"$"#)]
async fn add_user_with_named_role(w: &mut NmpWorld, pubkey: String, role: String) {
    add_user_with_role(w, pubkey, Some(role)).await;
}

async fn add_user_with_role(w: &mut NmpWorld, pubkey: String, role: Option<String>) {
    let pubkey = w.member_pubkey(&pubkey);
    let author = w.me_pubkey();
    w.group_operation(None, GroupCall::default(), move |group, engine| {
        group.add_users(engine, author, [nmp_nip29::GroupUser { pubkey, role }])
    })
    .await;
}

#[when(regex = r#"^I remove user "([0-9a-fA-F]{64})" from the group$"#)]
async fn remove_user(w: &mut NmpWorld, pubkey: String) {
    let pubkey = w.member_pubkey(&pubkey);
    let author = w.me_pubkey();
    w.group_operation(None, GroupCall::default(), move |group, engine| {
        group.remove_users(engine, author, [pubkey])
    })
    .await;
}

#[when(regex = r#"^I edit the group metadata with name "([^"]+)" and about "([^"]+)"$"#)]
async fn edit_metadata_both(w: &mut NmpWorld, name: String, about: String) {
    edit_metadata(w, Some(name), Some(about)).await;
}

#[when(regex = r#"^I edit the group metadata with name "([^"]+)" and nothing else$"#)]
async fn edit_metadata_name_only(w: &mut NmpWorld, name: String) {
    edit_metadata(w, Some(name), None).await;
}

async fn edit_metadata(w: &mut NmpWorld, name: Option<String>, about: Option<String>) {
    let author = w.me_pubkey();
    let edit = nmp_nip29::GroupMetadataEdit {
        name,
        about,
        ..nmp_nip29::GroupMetadataEdit::default()
    };
    w.group_operation(None, GroupCall::default(), move |group, engine| {
        group.edit_metadata(engine, author, edit.clone())
    })
    .await;
}

/// The outline form: every named operation, invoked with the ONE argument
/// each needs and nothing else, so "every operation takes the same path" is
/// asserted over the real set rather than over one representative.
#[when(regex = r#"^I invoke the group operation (.+)$"#)]
async fn invoke_named_operation(w: &mut NmpWorld, operation: String) {
    const SUBJECT: &str = "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad";
    match operation.trim() {
        "join request" => join_request(w, Some("dark-slide-42".to_string())).await,
        "leave request" => {
            let author = w.me_pubkey();
            w.group_operation(None, GroupCall::default(), move |group, engine| {
                group.leave_request(engine, author)
            })
            .await
        }
        "add user" => add_user_with_role(w, SUBJECT.to_string(), None).await,
        "remove user" => {
            let pubkey = w.member_pubkey(SUBJECT);
            let author = w.me_pubkey();
            w.group_operation(None, GroupCall::default(), move |group, engine| {
                group.remove_users(engine, author, [pubkey])
            })
            .await
        }
        "edit metadata" => {
            edit_metadata(w, Some("Photographers".to_string()), None).await;
        }
        other => panic!("nmp-bdd: {other:?} is not a named NIP-29 group operation"),
    }
}

// ---- reads: the group mints a demand, `observe` is the door -------------

#[when(regex = r#"^I observe a live query built from the group's demand for that filter$"#)]
async fn observe_group_demand(w: &mut NmpWorld) {
    let filter = w.last_staged_filter();
    w.observe_group_demand(None, filter).await;
}

#[when(
    regex = r#"^I observe a live query built from the "([^"]+)" group's demand for that filter$"#
)]
async fn observe_named_group_demand(w: &mut NmpWorld, group_id: String) {
    let filter = w.last_staged_filter();
    w.observe_group_demand(Some(&group_id), filter).await;
}

#[when(
    regex = r#"^I observe a second live query built from the same group's demand for a filter selecting (.+)$"#
)]
async fn observe_second_group_demand(w: &mut NmpWorld, kinds: String) {
    w.stage_filter(parse_kind_list(&kinds));
    let filter = w.last_staged_filter();
    w.observe_group_demand(None, filter).await;
}

#[when(regex = r#"^I observe live queries built from the group's demand for all four filters$"#)]
async fn observe_all_staged_filters(w: &mut NmpWorld) {
    let filters = w.staged_filters();
    assert_eq!(
        filters.len(),
        4,
        "this step is the four-simultaneous-queries case; the scenario staged {} filter(s)",
        filters.len()
    );
    for filter in filters {
        w.observe_group_demand(None, filter).await;
    }
}

// ---- surface inspection -------------------------------------------------
//
// Three claims in this feature set are about the SHAPE of the door rather
// than about anything a run produces ("no group write operation accepts a
// relay"). They are answered by reading the door's own source, which is the
// only witness that exists for the absence of a parameter -- and the same
// witness the ownership gate uses.

#[when(regex = r#"^I inspect the group's (read|write|operation) surface$"#)]
async fn inspect_group_surface(w: &mut NmpWorld, which: String) {
    w.inspect_group_surface(&which);
}
