//! `When` — the two payload shapes an app hands the publish door.
//!
//! Its own file, next door to the general `when` catalog, for the reason
//! `then/` is a directory: this family's whole vocabulary ("I compose an
//! event of kind N", "I publish the signed event X as-is") reads on its own
//! only when it has a name, and `features/writes/` is the only feature
//! directory that speaks it.

use std::time::Duration;

use cucumber::gherkin::Step;
use cucumber::when;

use nostr::Tag;

use nmp_grammar::Identity;

use crate::world::{parse_stated_time, NmpWorld};

// ---- composing a builder -------------------------------------------------

#[when(regex = r#"^I compose an event of kind (\d+) saying "([^"]*)" and publish it$"#)]
async fn compose_and_publish(w: &mut NmpWorld, kind: u16, text: String) {
    w.compose_and_publish_event(kind, &text, None, Vec::new())
        .await;
}

/// The import case: an app that states its own `created_at`, which NMP keeps
/// verbatim.
#[when(
    regex = r#"^I compose an event of kind (\d+) saying "([^"]*)" created at "([^"]+)" and publish it$"#
)]
async fn compose_with_created_at_and_publish(
    w: &mut NmpWorld,
    kind: u16,
    text: String,
    at: String,
) {
    w.compose_and_publish_event(kind, &text, Some(parse_stated_time(&at)), Vec::new())
        .await;
}

/// The two-publishes form. The delay is on the STATED clock, never on the
/// wall: what the scenario claims is that two composes of the same words
/// differ by the time NMP stamped them, and a real sleep would prove nothing
/// about a stamp the engine reads from its own clock.
#[when(
    regex = r#"^(\d+) seconds later I compose an event of kind (\d+) saying "([^"]*)" and publish it$"#
)]
async fn seconds_later_compose_and_publish(
    w: &mut NmpWorld,
    seconds: u64,
    kind: u16,
    text: String,
) {
    w.advance_clock(Duration::from_secs(seconds)).await;
    w.compose_and_publish_event(kind, &text, None, Vec::new())
        .await;
}

#[when(
    regex = r#"^(\d+) seconds later I compose an event of kind (\d+) saying "([^"]*)" created at "([^"]+)" and publish it$"#
)]
async fn seconds_later_compose_with_created_at(
    w: &mut NmpWorld,
    seconds: u64,
    kind: u16,
    text: String,
    at: String,
) {
    w.advance_clock(Duration::from_secs(seconds)).await;
    w.compose_and_publish_event(kind, &text, Some(parse_stated_time(&at)), Vec::new())
        .await;
}

/// The tag table. Every cell of a row is one element of one tag, in the order
/// written, with trailing empty cells dropped -- a Gherkin table has to be
/// rectangular, and a two-element tag in a three-column table is what the
/// blank cell means.
#[when(regex = r#"^I compose an event of kind (\d+) saying "([^"]*)" with the tags:$"#)]
async fn compose_with_tags(w: &mut NmpWorld, step: &Step, kind: u16, text: String) {
    let table = step
        .table
        .as_ref()
        .expect("nmp-bdd: this step names a tag table");
    let tags: Vec<Tag> = table
        .rows
        .iter()
        .map(|row| {
            let mut cells: Vec<String> = row.iter().map(|cell| cell.trim().to_string()).collect();
            while cells.last().is_some_and(String::is_empty) {
                cells.pop();
            }
            Tag::parse(cells).expect("nmp-bdd: a tag row is a non-empty list of strings")
        })
        .collect();
    w.stage_composed_event(kind, &text, None, tags);
}

#[when(regex = r#"^I publish it$"#)]
async fn publish_staged(w: &mut NmpWorld) {
    w.publish_staged_event().await;
}

// ---- publishing an already-signed event ---------------------------------

#[when(regex = r#"^I publish the signed event "([0-9a-f]{64})" as-is to "([^"]+)"$"#)]
async fn publish_signed_as_is(w: &mut NmpWorld, label: String, relay: String) {
    w.publish_signed_event(&label, &relay, Identity::Active)
        .await;
}

/// The tampered copy, handed over by the word that names it. Same door, same
/// routing -- what differs is only that the bytes no longer match the
/// signature they arrived with.
#[when(regex = r#"^I publish it as-is to "([^"]+)"$"#)]
async fn publish_altered_as_is(w: &mut NmpWorld, relay: String) {
    let label = w.only_signed_event_label();
    w.publish_signed_event(&label, &relay, Identity::Active)
        .await;
}

#[when(
    regex = r#"^I publish the signed event "([0-9a-f]{64})" to "([^"]+)" naming identity "([0-9a-f]{64})"$"#
)]
async fn publish_signed_naming_identity(
    w: &mut NmpWorld,
    label: String,
    relay: String,
    pubkey: String,
) {
    let key = w.person(&pubkey).public_key();
    w.publish_signed_event(&label, &relay, Identity::Explicit(key))
        .await;
}

// ---- time passing --------------------------------------------------------

/// `And 30 days pass with nothing learned`, `When 40 seconds pass` -- on the
/// STATED clock, and delivered: the engine acts on the new instant rather
/// than merely being told about it. See `world::clock`.
///
/// Both units in one step because `features/diagnostics/stalled-writes.feature`
/// asks the SAME question at both scales on purpose -- forty seconds is
/// discovery in flight and forty days is a recipient who never published a
/// relay list, and NMP is required to treat them identically. Two steps could
/// drift into treating them differently.
#[when(regex = r#"^(\d+) (seconds?|days?) pass(?: with nothing learned)?$"#)]
async fn time_passes(w: &mut NmpWorld, amount: u64, unit: String) {
    let seconds = match unit.trim_end_matches('s') {
        "second" => amount,
        "day" => amount * 86_400,
        other => panic!("nmp-bdd: unsupported elapsed unit {other:?}"),
    };
    w.advance_clock(Duration::from_secs(seconds)).await;
}

// ---- driving the engine on purpose --------------------------------------
//
// Capability #5 of issue #1013. `features/routing/idempotent-resends.feature`
// claims that re-running resolution costs an empty diff, and that claim is
// only falsifiable if the harness can make it re-run -- n times, on demand,
// rather than by waiting for a deadline to elapse on its own.

#[when(regex = r#"^the engine ticks (\d+) times$"#)]
async fn the_engine_ticks(w: &mut NmpWorld, times: usize) {
    w.tick_engine_times(times).await;
}

#[when(regex = r#"^the publishing queue drains (\d+) times with nothing new learned$"#)]
async fn the_queue_drains(w: &mut NmpWorld, times: usize) {
    w.tick_engine_times(times).await;
}

/// The process boundary, said on its own line. `tests/bdd.rs` reads this
/// sentence BEFORE the scenario runs and puts the world on a store that
/// survives it.
#[when(regex = r#"^the process stops(?: with the note undelivered)?$"#)]
async fn the_process_stops(w: &mut NmpWorld) {
    w.stop_process().await;
}
