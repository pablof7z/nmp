//! `When` — one actor does one thing (approach doc §1.2/§2.4): the app
//! opens/closes a feed or publishes; another user posts/updates their own
//! state; the network drops or restores a relay.

use std::time::Duration;

use cucumber::when;

use nmp_grammar::Identity;

use crate::steps::{parse_people, parse_tag};
use crate::world::{NmpWorld, WatchShape};

#[when(regex = r#"^I open a feed of my follows' notes$"#)]
async fn open_feed(w: &mut NmpWorld) {
    w.open_my_follows_feed().await;
}

/// A BOUNDED follows feed. The window belongs to the feed, not to each
/// author in it (#937), so this must reach each relay as ONE request carrying
/// that relay's authors and the feed's own `limit` -- never as one request
/// per author each promising a full page.
#[when(regex = r#"^I open a feed of the latest (\d+) of my follows' notes$"#)]
async fn open_feed_limited(w: &mut NmpWorld, limit: usize) {
    w.open_my_follows_feed_limited(limit).await;
}

#[when(regex = r#"^my feed of my follows' notes runs to a steady state$"#)]
async fn feed_runs_to_steady_state(w: &mut NmpWorld) {
    w.open_my_follows_feed().await;
    // "Steady state" for a headless world with no further stimulus simply
    // means: give every already-staged relay's backlog time to arrive and
    // settle. `feed_eventually` with an always-true predicate still drains
    // whatever arrives within the bounded window before returning.
    w.feed_eventually(|_, _| true);
}

#[when(regex = r#"^I publish a new follow list with (.+)$"#)]
async fn publish_new_follow_list(w: &mut NmpWorld, list: String) {
    w.publish_new_follow_list(&parse_people(&list)).await;
}

#[when(regex = r#"^I publish a note saying "([^"]+)"$"#)]
async fn publish_note(w: &mut NmpWorld, text: String) {
    w.publish_note(&text).await;
}

#[when(regex = r#"^I switch to (\S+)'s account$"#)]
async fn switch_account(w: &mut NmpWorld, person: String) {
    w.switch_account(&person).await;
}

#[when(regex = r#"^I switch to a new account that follows (.+)$"#)]
async fn switch_to_new_account(w: &mut NmpWorld, list: String) {
    w.switch_to_new_account_following(&parse_people(&list))
        .await;
}

#[when(regex = r#"^(\S+) posts a note saying "([^"]+)"$"#)]
async fn person_posts_note(w: &mut NmpWorld, person: String, text: String) {
    w.person_posts_note_live(&person, &text).await;
}

#[when(regex = r#"^relay "([^"]+)" drops the connection$"#)]
async fn relay_drops(w: &mut NmpWorld, name: String) {
    w.drop_relay_connection(&name).await;
}

#[when(regex = r#"^relay "([^"]+)" comes back$"#)]
async fn relay_comes_back(w: &mut NmpWorld, name: String) {
    w.relay_comes_back(&name).await;
}

// ---- watching one relay directly ---------------------------------------
//
// The subject of `features/routing/subscription-collapse.feature`. These read
// lower than the feed steps above on purpose -- the contract they serve is
// about what NMP puts on a relay socket -- but they are still framed as
// things a person does ("I watch for", "I stop watching"), never as calls
// ("subscribe with filter X").

#[when(regex = r#"^I watch for notes tagged "([a-zA-Z])" as "([^"]+)"$"#)]
async fn watch_tag_value(w: &mut NmpWorld, tag: String, value: String) {
    w.watch_tag_value(parse_tag(&tag), &value).await;
}

#[when(regex = r#"^(\d+)ms later I watch for notes tagged "([a-zA-Z])" as "([^"]+)"$"#)]
async fn watch_tag_value_after(w: &mut NmpWorld, delay_ms: u64, tag: String, value: String) {
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    w.watch_tag_value(parse_tag(&tag), &value).await;
}

#[when(regex = r#"^I watch for notes tagged "([a-zA-Z])" as (\d+) different values$"#)]
async fn watch_n_tag_values(w: &mut NmpWorld, tag: String, n: usize) {
    w.watch_n_tag_values(parse_tag(&tag), n).await;
}

/// A watch for the LATEST n notes under a tag value. The `limit` is not
/// decoration: a relay-side limit caps the RESULT COUNT rather than the
/// predicate, so two limited watches for different values cannot be unioned
/// into one without under-fetching. The author-axis twin of this step
/// (`I watch for the latest N notes from <person>`) is what the injectivity
/// scenario uses.
#[when(regex = r#"^I watch for the latest (\d+) notes tagged "([a-zA-Z])" as "([^"]+)"$"#)]
async fn watch_tag_value_limited(w: &mut NmpWorld, limit: usize, tag: String, value: String) {
    w.watch_tag_value_shaped(
        parse_tag(&tag),
        &value,
        WatchShape {
            limit: Some(limit),
            ..WatchShape::default()
        },
    )
    .await;
}

/// The delayed form of the limited watch above -- demand arriving at an
/// already-saturated relay some time after the first subscriptions are live,
/// which is what a subscription limit has to stay quiet under.
#[when(
    regex = r#"^(\d+)ms later I watch for the latest (\d+) notes tagged "([a-zA-Z])" as "([^"]+)"$"#
)]
async fn watch_tag_value_limited_after(
    w: &mut NmpWorld,
    delay_ms: u64,
    limit: usize,
    tag: String,
    value: String,
) {
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    w.watch_tag_value_shaped(
        parse_tag(&tag),
        &value,
        WatchShape {
            limit: Some(limit),
            ..WatchShape::default()
        },
    )
    .await;
}

/// A watch narrowed to a time window. `since` is a co-pinned bound, not a
/// value list, so two windows never union -- the scenarios using this step
/// assert that two tag watches under DIFFERENT windows stay two
/// subscriptions.
///
/// The day count is turned into a `since` timestamp here rather than in the
/// scenario, so the spec reads in days and the wire sees seconds.
#[when(regex = r#"^I watch for notes tagged "([a-zA-Z])" as "([^"]+)" from the last (\d+) days?$"#)]
async fn watch_tag_value_windowed(w: &mut NmpWorld, tag: String, value: String, days: u64) {
    w.watch_tag_value_shaped(
        parse_tag(&tag),
        &value,
        WatchShape {
            since: Some(days_ago(days)),
            ..WatchShape::default()
        },
    )
    .await;
}

/// A fixed epoch minus `days`, so the same scenario run twice produces the
/// same filter and a scenario that asks for two different windows really does
/// get two different `since` values.
fn days_ago(days: u64) -> u64 {
    const EPOCH: u64 = 1_750_000_000;
    EPOCH - days * 86_400
}

#[when(regex = r#"^I stop watching notes tagged "([a-zA-Z])" as "([^"]+)"$"#)]
async fn stop_watching_tag_value(w: &mut NmpWorld, tag: String, value: String) {
    w.stop_watching_tag_value(parse_tag(&tag), &value).await;
}

#[when(regex = r#"^I watch for notes from (\S+)$"#)]
async fn watch_author(w: &mut NmpWorld, person: String) {
    w.watch_author(&person, None).await;
}

#[when(regex = r#"^I watch for the latest (\d+) notes from (\S+)$"#)]
async fn watch_author_limited(w: &mut NmpWorld, limit: usize, person: String) {
    w.watch_author(&person, Some(limit)).await;
}

#[when(regex = r#"^I stop watching notes from (\S+)$"#)]
async fn stop_watching_author(w: &mut NmpWorld, person: String) {
    w.stop_watching_author(&person).await;
}

#[when(regex = r#"^I open the group state of every group I administer$"#)]
async fn open_group_state(w: &mut NmpWorld) {
    w.open_group_state_watch().await;
}

#[when(regex = r#"^I am made an admin of one more group$"#)]
async fn made_admin_of_one_more_group(w: &mut NmpWorld) {
    w.made_admin_of_one_more_group().await;
}

// ---- routing: the two words --------------------------------------------
//
// The subject of `features/routing/auto-and-explicit.feature`. "Let NMP
// figure out the routing" and "to exactly <relays>" are the only two things
// a scenario may say, because they are the only two things an app may say.

#[when(regex = r#"^I publish a note saying "([^"]+)" and let NMP figure out the routing$"#)]
async fn publish_note_auto(w: &mut NmpWorld, text: String) {
    w.publish_note(&text).await;
}

#[when(regex = r#"^I publish a note saying "([^"]+)" to exactly (.+)$"#)]
async fn publish_note_explicit(w: &mut NmpWorld, text: String, targets: String) {
    w.publish_note_to_exactly(&text, &parse_relay_targets(&targets))
        .await;
}

#[when(regex = r#"^I publish (\S+)'s signed note unchanged to exactly (.+)$"#)]
async fn republish_signed_note(w: &mut NmpWorld, _person: String, targets: String) {
    let text = w
        .only_staged_signed_note_text()
        .expect("nmp-bdd: republishing needs exactly one note staged as already-signed");
    w.republish_signed_note_to_exactly(&text, &parse_relay_targets(&targets))
        .await;
}

/// `"a"`, `"a" and "b"`, or the literal `no relays`. The empty case is a
/// real request the engine must refuse, not a request the harness declines
/// to make.
fn parse_relay_targets(raw: &str) -> Vec<String> {
    if raw.trim().trim_end_matches('.') == "no relays" {
        return Vec::new();
    }
    let names = crate::steps::parse_quoted_list(raw);
    assert!(
        !names.is_empty(),
        "expected quoted relay names (or the words \"no relays\") in {raw:?}"
    );
    names
}

// ---- identity: composing and publishing as somebody ---------------------
//
// The subject of `features/identity/`. "Naming no identity" and "naming
// identity <hex>" are the only two things a scenario may say, because they
// are the only two things an app may say.

#[when(
    regex = r#"^I compose an event of kind (\d+) saying "([^"]+)" and publish it naming no identity$"#
)]
async fn compose_and_publish_as_active(w: &mut NmpWorld, kind: u16, text: String) {
    w.publish_composed_event(kind, &text, Identity::Active)
        .await;
}

#[when(
    regex = r#"^I compose an event of kind (\d+) saying "([^"]+)" and publish it naming identity "([0-9a-f]{64})"$"#
)]
async fn compose_and_publish_as_identity(
    w: &mut NmpWorld,
    kind: u16,
    text: String,
    pubkey: String,
) {
    let key = w.person(&pubkey).public_key();
    w.publish_composed_event(kind, &text, Identity::Explicit(key))
        .await;
}

/// The intended path for an app that holds a display form: the decode already
/// happened, at the app's own boundary, and what reaches the write plane is a
/// key like any other.
#[when(
    regex = r#"^I compose an event of kind (\d+) saying "([^"]+)" and publish it naming that identity$"#
)]
async fn compose_and_publish_as_decoded_identity(w: &mut NmpWorld, kind: u16, text: String) {
    let key = w.decoded_identity();
    w.publish_composed_event(kind, &text, Identity::Explicit(key))
        .await;
}

/// The refusal is STRUCTURAL rather than a message the engine sends back:
/// the identity a write names is a public key, and a bech32 string is not
/// one. Nothing is published, so there is no receipt for this to fail on.
#[when(
    regex = r#"^I compose an event of kind (\d+) saying "([^"]+)" and publish it naming as identity the npub form of "([0-9a-f]{64})"$"#
)]
async fn compose_and_publish_naming_an_npub(
    w: &mut NmpWorld,
    _kind: u16,
    _text: String,
    pubkey: String,
) {
    w.ensure_started().await;
    w.refuse_bech32_identity(&pubkey);
}

#[when(regex = r#"^I switch the active account to "([0-9a-f]{64})"$"#)]
async fn switch_active_identity(w: &mut NmpWorld, pubkey: String) {
    w.switch_active_identity(&pubkey).await;
}

/// Also a `Then` (see `then::identity`): a scenario may either assert the
/// acceptance or simply wait for it before doing the next thing.
#[when(regex = r#"^the write reports accepted$"#)]
async fn write_reports_accepted_when(w: &mut NmpWorld) {
    assert!(
        w.write_reported_accepted(None),
        "expected the write to report Accepted; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[when(regex = r#"^the receipt reports it awaiting a signer for "([0-9a-f]{64})"$"#)]
async fn receipt_reports_awaiting_when(w: &mut NmpWorld, pubkey: String) {
    assert!(
        w.write_awaiting_signer_for(&pubkey, None),
        "expected the receipt to park awaiting a signer for {pubkey}; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[when(regex = r#"^the write reports accepted and the process stops immediately$"#)]
async fn write_accepted_then_process_stops(w: &mut NmpWorld) {
    assert!(
        w.write_reported_accepted(None),
        "expected the write to report Accepted before the process stopped; saw {:?}",
        w.identity_receipt_statuses(None)
    );
}

#[when(regex = r#"^I reconstruct the engine from the same durable store$"#)]
async fn reconstruct_engine(w: &mut NmpWorld) {
    let active = w.active_identity_label();
    w.restart_engine(active).await;
}

#[when(
    regex = r#"^I reconstruct the engine from the same durable store with "([0-9a-f]{64})" active$"#
)]
async fn reconstruct_engine_with_active(w: &mut NmpWorld, pubkey: String) {
    w.person(&pubkey);
    w.restart_engine(Some(pubkey)).await;
}

#[when(regex = r#"^the podcast identity's signer answers$"#)]
async fn podcast_signer_answers(w: &mut NmpWorld) {
    let label = w.podcast_identity();
    w.release_signer(&label);
}

#[when(regex = r#"^the first account's signer answers$"#)]
async fn first_accounts_signer_answers(w: &mut NmpWorld) {
    let label = w.first_identity();
    w.release_signer(&label);
}

/// A signing capability for exactly that key arriving after the write was
/// accepted and parked. What the park waits on is a capability for one
/// pubkey; which transport carries it is not something the write observes.
#[when(regex = r#"^a NIP-46 signer for "([0-9a-f]{64})" attaches(?: \d+ seconds later)?$"#)]
async fn nip46_signer_attaches(w: &mut NmpWorld, pubkey: String) {
    w.attach_signer_for(&pubkey).await;
}

#[when(regex = r#"^I cancel that write$"#)]
async fn cancel_that_write(w: &mut NmpWorld) {
    w.cancel_last_write();
}

#[when(regex = r#"^the app decodes it to a public key$"#)]
async fn app_decodes_pasted_npub(w: &mut NmpWorld) {
    w.decode_pasted_npub();
}
