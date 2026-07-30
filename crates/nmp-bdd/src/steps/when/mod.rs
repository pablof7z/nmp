//! `When` — one actor does one thing (approach doc §1.2/§2.4): the app
//! opens/closes a feed or publishes; another user posts/updates their own
//! state; the network drops or restores a relay.

use std::time::Duration;

use cucumber::when;

use nmp_grammar::Identity;

use crate::steps::{parse_people, parse_tag};
use crate::world::{NmpWorld, WatchShape, ME};

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

/// One literal author's notes, pinned to every named relay. This spelling is
/// intentionally stronger than a follows feed: source-provenance scenarios
/// must mechanically contact both relays instead of relying on the bounded
/// outbox solver to choose every candidate.
#[when(regex = r#"^I read (\S+)'s notes from relays (.+)$"#)]
async fn read_authored_notes_from_relays(w: &mut NmpWorld, person: String, list: String) {
    let relays = crate::steps::parse_quoted_list(&list);
    assert!(
        relays.len() > 1,
        "expected more than one quoted relay name in {list:?}"
    );
    w.open_authored_notes_from_relays(&person, &relays).await;
}

/// One read, pinned to several hosts, of one group's metadata coordinate.
/// The plural relay list is the point: divergence is only observable when a
/// single query reaches both hosts.
#[when(regex = r#"^I read the metadata for group "([^"]+)" from relays (.+)$"#)]
async fn read_group_metadata_from_relays(w: &mut NmpWorld, group_id: String, list: String) {
    let relays = crate::steps::parse_quoted_list(&list);
    assert!(
        !relays.is_empty(),
        "expected at least one quoted relay name in {list:?}"
    );
    w.open_group_metadata_feed(&group_id, &relays).await;
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

#[when(regex = r#"^I publish kind (\d+) with d tag "([^"]*)" saying "([^"]+)"$"#)]
async fn publish_replaceable(w: &mut NmpWorld, kind: u16, d: String, text: String) {
    w.publish_replaceable(kind, &d, &text).await;
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

// ---- routing as a lifecycle ---------------------------------------------
//
// A mention is what makes the outbox fan-out -- and therefore its unknowns --
// reachable from a scenario at all: an event with no p-tags has exactly one
// contributing author.

#[when(regex = r#"^I publish a note(?: saying "([^"]*)")? mentioning (.+)$"#)]
async fn publish_note_mentioning(w: &mut NmpWorld, text: String, people: String) {
    let text = if text.is_empty() {
        format!("mentioning {people}")
    } else {
        text
    };
    w.publish_note_mentioning(&text, &parse_people(&people))
        .await;
}

/// A real kind:10002 landing at the indexers, where the engine's own
/// discovery is already looking. Nothing is injected into the directory: the
/// event goes on a relay and comes back through ordinary ingestion, which is
/// the only way this proves a parked route wakes on what the READ path
/// learned.
#[when(regex = r#"^my relay list arrives naming "([^"]+)" as my write relay$"#)]
async fn my_relay_list_arrives(w: &mut NmpWorld, relay: String) {
    w.relay_list_arrives(ME, &[relay], &[]).await;
}

#[when(regex = r#"^(\S+)'s relay list arrives naming "([^"]+)" as (?:her|his|their) read relay$"#)]
async fn person_relay_list_arrives(w: &mut NmpWorld, person: String, relay: String) {
    w.relay_list_arrives(&person, &[], &[relay]).await;
}

// ---- routing: the outbox default ----------------------------------------
//
// The subject of `features/routing/outbox-*.feature`. "p-tags" rather than
// "mentioning" because these scenarios are about the TAG: a recipient is
// reached at the inbox their relay list names, and which tag put them in the
// event is exactly what decides that.

#[when(regex = r#"^I publish a note saying "([^"]+)" that p-tags (.+)$"#)]
async fn publish_note_p_tagging(w: &mut NmpWorld, text: String, people: String) {
    w.publish_note_mentioning(&text, &parse_people(&people))
        .await;
}

/// An ordinary kind:0 through the ordinary door, saying nothing about relays
/// -- the whole point of the app-relay scenarios being that a profile is not
/// a special case.
#[when(regex = r#"^I publish my profile$"#)]
async fn publish_profile(w: &mut NmpWorld) {
    w.publish_profile().await;
}

#[when(regex = r#"^I publish a kind (\d+) event$"#)]
async fn publish_kind(w: &mut NmpWorld, kind: u16) {
    w.publish_kind(kind).await;
}

#[when(regex = r#"^my relay list arrives naming (.+) as my write relays$"#)]
async fn my_relay_list_arrives_plural(w: &mut NmpWorld, list: String) {
    let names = crate::steps::parse_quoted_list(&list);
    assert!(!names.is_empty(), "expected quoted relay names in {list:?}");
    w.relay_list_arrives(ME, &names, &[]).await;
}

/// The withholding source starts answering. A real relay really does reach
/// end of stored events, on the subscription the engine already had open, and
/// the absence settles off that -- nothing is injected into the directory.
#[when(regex = r#"^the indexers finish their stored events without a relay list for (?:\S+)$"#)]
async fn indexers_finish_stored_events(w: &mut NmpWorld) {
    w.indexers_finish_stored_events().await;
}

#[when(regex = r#"^the indexers deliver (\S+)'s relay list and confirm end of stored events$"#)]
async fn indexers_deliver_relay_list(w: &mut NmpWorld, person: String) {
    let relays = w.read_relay_names_of(&person);
    assert!(
        !relays.is_empty(),
        "nmp-bdd: {person}'s relay list must name a read relay for the indexers to deliver"
    );
    w.relay_list_arrives(&person, &[], &relays).await;
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

#[when(regex = r#"^the process stops immediately$"#)]
async fn process_stops_immediately(w: &mut NmpWorld) {
    w.stop_process().await;
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

#[when(regex = r#"^I reattach to the receipt by its stable id$"#)]
async fn reattach_receipt_by_stable_id(w: &mut NmpWorld) {
    assert!(
        w.restarted_receipt_is_reattached(),
        "the reconstructed engine did not reattach the durable receipt by its stable id"
    );
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

// ---- the global stalled-write list -------------------------------------

#[when(regex = r#"^I publish that note$"#)]
async fn publish_that_note(w: &mut NmpWorld) {
    w.publish_told_note().await;
}

#[when(regex = r#"^I read diagnostics$"#)]
async fn read_diagnostics(w: &mut NmpWorld) {
    w.ensure_started().await;
    w.read_stalled_writes();
}

/// Reading a mirror, repeatedly and on purpose. If reading retried, an app
/// that polled would publish differently from one that did not.
#[when(regex = r#"^I read diagnostics (\d+) times$"#)]
async fn read_diagnostics_n_times(w: &mut NmpWorld, times: usize) {
    w.ensure_started().await;
    w.read_diagnostics_repeatedly(times);
}

#[when(regex = r#"^I cancel that write$"#)]
async fn cancel_that_write(w: &mut NmpWorld) {
    w.cancel_last_write();
}

#[when(regex = r#"^the app decodes it to a public key$"#)]
async fn app_decodes_pasted_npub(w: &mut NmpWorld) {
    w.decode_pasted_npub();
}

// The group family lives next door for the same reason `then/` is a directory:
// this catalog is shared by every feature, and one family's whole vocabulary is
// readable on its own only when it has a name. See `when::groups`.
mod groups;
mod replaceable;
mod writes;
