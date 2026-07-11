//! `When` — one actor does one thing (approach doc §1.2/§2.4): the app
//! opens/closes a feed or publishes; another user posts/updates their own
//! state; the network drops or restores a relay.

use cucumber::when;

use crate::steps::parse_people;
use crate::world::NmpWorld;

#[when(regex = r#"^I open a feed of my follows' notes$"#)]
async fn open_feed(w: &mut NmpWorld) {
    w.open_my_follows_feed().await;
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
    w.drop_relay_connection(&name);
}

#[when(regex = r#"^relay "([^"]+)" comes back$"#)]
async fn relay_comes_back(w: &mut NmpWorld, name: String) {
    w.relay_comes_back(&name).await;
}
