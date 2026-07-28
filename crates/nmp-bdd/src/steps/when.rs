//! `When` — one actor does one thing (approach doc §1.2/§2.4): the app
//! opens/closes a feed or publishes; another user posts/updates their own
//! state; the network drops or restores a relay.

use std::time::Duration;

use cucumber::when;

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
