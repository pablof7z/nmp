//! `Given` — the world before the app acts (approach doc §1.2/§2.4): relay
//! topology, configured operator policy, pre-existing protocol state. Every
//! step here only STAGES data on [`NmpWorld`] -- nothing hits a socket until
//! a later step calls `ensure_started` (most directly via the `my feed ...
//! is open` shorthand below).

use cucumber::given;

use crate::steps::{parse_people, parse_quoted_list};
use crate::world::{NmpWorld, ME};

#[given(regex = r#"^only (\d+) indexer relays? (?:is|are) configured$"#)]
async fn only_n_indexers(w: &mut NmpWorld, n: usize) {
    w.configure_n_indexers(n);
}

#[given(regex = r#"^relays? (.+) (?:is|are) configured as indexers?$"#)]
async fn named_indexers(w: &mut NmpWorld, list: String) {
    let names = parse_quoted_list(&list);
    assert!(
        !names.is_empty(),
        "expected at least one quoted relay name in {list:?}"
    );
    w.configure_named_indexers(&names);
}

#[given(regex = r#"^a relay "([^"]+)" exists that nothing references$"#)]
async fn bystander_relay(w: &mut NmpWorld, name: String) {
    w.register_bystander_relay(&name);
}

#[given(regex = r#"^relay "([^"]+)" rejects every event$"#)]
async fn relay_rejects_writes(w: &mut NmpWorld, name: String) {
    w.set_reject_writes(&name);
}

#[given(regex = r#"^relay "([^"]+)" never confirms end of stored events$"#)]
async fn relay_never_confirms_eose(w: &mut NmpWorld, name: String) {
    w.set_reject_queries(&name);
}

/// The relay publishes a real NIP-11 document, served over plain HTTP on its
/// own address, which the engine fetches through its own acquisition path.
/// Nothing about the number is injected past the engine.
#[given(regex = r#"^relay "([^"]+)" allows only (\d+) subscriptions? at a time$"#)]
async fn relay_allows_only_n_subscriptions(w: &mut NmpWorld, name: String, max: u64) {
    w.advertise_subscription_limit(&name, max);
}

#[given(regex = r#"^relay "([^"]+)" publishes nothing about itself$"#)]
async fn relay_publishes_nothing(w: &mut NmpWorld, name: String) {
    w.publish_no_relay_document(&name);
}

#[given(regex = r#"^relay "([^"]+)" accepts subscription names of at most (\d+) characters$"#)]
async fn relay_accepts_subid_length(w: &mut NmpWorld, name: String, max: u64) {
    w.advertise_subid_length(&name, max);
}

#[given(regex = r#"^(\S+)'s relay list names "([^"]+)" as (?:her|his|their) write relay$"#)]
async fn person_write_relay(w: &mut NmpWorld, person: String, relay: String) {
    w.declare_write_relay(&person, &relay);
}

#[given(regex = r#"^my relay list names "([^"]+)" as my write relay$"#)]
async fn my_write_relay(w: &mut NmpWorld, relay: String) {
    w.declare_write_relay(ME, &relay);
}

#[given(regex = r#"^my relay list names (.+) as my write relays$"#)]
async fn my_write_relays(w: &mut NmpWorld, list: String) {
    for relay in parse_quoted_list(&list) {
        w.declare_write_relay(ME, &relay);
    }
}

#[given(regex = r#"^(\S+) follows (.+)$"#)]
async fn person_follows(w: &mut NmpWorld, person: String, list: String) {
    w.stage_follows(&person, &parse_people(&list));
}

#[given(regex = r#"^I am logged in as an account that follows (.+)$"#)]
async fn logged_in_following(w: &mut NmpWorld, list: String) {
    let follows = if list.trim() == "nobody" {
        Vec::new()
    } else {
        parse_people(&list)
    };
    w.log_in_as(ME, &follows);
}

#[given(regex = r#"^I am logged in as my own account$"#)]
async fn logged_in_own_account(w: &mut NmpWorld) {
    w.log_in_as(ME, &[]);
}

#[given(regex = r#"^I am logged in as (\S+)'s account$"#)]
async fn logged_in_as_person(w: &mut NmpWorld, person: String) {
    w.log_in_as(&person, &[]);
}

#[given(regex = r#"^(\S+) has posted a note saying "([^"]+)"$"#)]
async fn person_posted_note(w: &mut NmpWorld, person: String, text: String) {
    w.stage_note(&person, &text);
}

#[given(regex = r#"^(\S+) has posted (\d+) notes?$"#)]
async fn person_posted_n_notes(w: &mut NmpWorld, person: String, n: usize) {
    for i in 1..=n {
        w.stage_note(&person, &format!("note {i} from {person}"));
    }
}

#[given(regex = r#"^I administer (\d+) groups?$"#)]
async fn administer_n_groups(w: &mut NmpWorld, n: usize) {
    w.stage_administered_groups(n);
}

#[given(regex = r#"^I administer no groups$"#)]
async fn administer_no_groups(w: &mut NmpWorld) {
    w.stage_administered_groups(0);
}

#[given(regex = r#"^the group state of every group I administer is open$"#)]
async fn group_state_is_open(w: &mut NmpWorld) {
    w.open_group_state_watch().await;
}

#[given(regex = r#"^relay "([^"]+)" is the relay I watch directly$"#)]
async fn watch_relay(w: &mut NmpWorld, name: String) {
    w.set_watch_relay(&name);
}

#[given(regex = r#"^my feed of my follows' notes is open$"#)]
async fn my_feed_is_open(w: &mut NmpWorld) {
    w.open_my_follows_feed().await;
}

// ---- routing: the two words --------------------------------------------

/// The default in this world, stated out loud because the routing scenarios
/// need it stated: when nothing is delivered to an app relay, it is because
/// no app relay exists, not because the assertion got lucky.
#[given(regex = r#"^no app relays are configured$"#)]
async fn no_app_relays(w: &mut NmpWorld) {
    w.assert_no_app_relays();
}

/// The engine-side twin of `my relay list names ... as my write relays`:
/// the directory is populated so an explicit route has something it could
/// wrongly consult.
#[given(regex = r#"^the directory knows (.+) as my write relays$"#)]
async fn directory_knows_my_write_relays(w: &mut NmpWorld, list: String) {
    for relay in parse_quoted_list(&list) {
        w.declare_write_relay(ME, &relay);
    }
}

/// A note staged as an ALREADY-SIGNED event, kept verbatim so a later step
/// can republish exactly the bytes its author signed.
#[given(regex = r#"^(\S+) has posted a note saying "([^"]+)" signed by \S+$"#)]
async fn person_posted_signed_note(w: &mut NmpWorld, person: String, text: String) {
    w.stage_signed_note(&person, &text);
}

/// A signer for the current account already exists in this world (every
/// scenario that logs in gets one), stated out loud where a scenario's point
/// is that the signer was NOT asked for anything.
#[given(regex = r#"^a signer is registered for the current pubkey$"#)]
async fn signer_is_registered(w: &mut NmpWorld) {
    w.assert_signer_registered();
}
