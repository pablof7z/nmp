//! `Given` — the OUTBOX plane's world: the two operator relay sets, the two
//! halves of one person's relay list, whether the discovery sources have
//! finished looking, and a publish that already happened.
//!
//! Its own file because everything here is staged by a rule no other family
//! shares. The operator sets belong to nobody in particular, so they cannot
//! be phrased as anybody's relay list. A recipient is reached at the READ
//! half and an author writes from the WRITE half, so the same sentence about
//! "a relay list" means a different set depending on which role the identity
//! plays. And whether the indexers have finished looking is not a fact about
//! a person at all -- it is the difference between an absence and an
//! ignorance, which is the distinction the whole outbox arm turns on.

use cucumber::given;

use crate::steps::{parse_people, parse_quoted_list};
use crate::world::{NmpWorld, ME};

// ---- routing: the two operator relay sets --------------------------------
//
// The subject of `features/routing/outbox-app-relays.feature` and
// `outbox-fallback-coverage.feature`. Neither set belongs to a person, which
// is why they are staged apart from every `<person>'s relay list ...` step:
// app relays reach every kind of every author always, and fallback relays top
// up a p-tagged RECIPIENT below the coverage minimum -- unless an app relay
// suppressed the top-up entirely.

#[given(regex = r#"^app relays (.+) (?:is|are) configured$"#)]
async fn app_relays_configured(w: &mut NmpWorld, list: String) {
    let names = parse_quoted_list(&list);
    assert!(
        !names.is_empty(),
        "expected at least one quoted relay name in {list:?}"
    );
    w.configure_app_relays(&names);
}

#[given(regex = r#"^fallback relays (.+) (?:is|are) configured$"#)]
async fn fallback_relays_configured(w: &mut NmpWorld, list: String) {
    let names = parse_quoted_list(&list);
    assert!(
        !names.is_empty(),
        "expected at least one quoted relay name in {list:?}"
    );
    w.configure_fallback_relays(&names);
}

#[given(regex = r#"^no fallback relays are configured$"#)]
async fn no_fallback_relays(w: &mut NmpWorld) {
    w.no_fallback_relays();
}

// ---- routing: the two halves of one relay list ---------------------------
//
// NIP-65 is two sets, and an outbox derivation reads a DIFFERENT one
// depending on whether the identity is the author or an addressee. Every
// spelling below exists because some scenario turns on the difference: the
// author's read-marked entry that must not be published to, the recipient's
// unmarked entry that is both halves, and the list that exists while naming
// nothing on the half being asked for.

#[given(regex = r#"^my relay list also names "([^"]+)" as a read-marked relay$"#)]
async fn my_extra_read_relay(w: &mut NmpWorld, relay: String) {
    w.declare_read_relay(ME, &relay);
}

#[given(regex = r#"^my relay list also names (.+) as write relays$"#)]
async fn my_extra_write_relays(w: &mut NmpWorld, list: String) {
    for relay in parse_quoted_list(&list) {
        w.declare_write_relay(ME, &relay);
    }
}

/// "only" is a REPLACEMENT: kind:10002 is replaceable, so a scenario
/// narrowing what its Background stated is describing the list it actually
/// has, not adding to one.
#[given(regex = r#"^my relay list names only (.+) as my write relays?$"#)]
async fn my_relay_list_is_only(w: &mut NmpWorld, list: String) {
    let names = parse_quoted_list(&list);
    assert!(
        !names.is_empty(),
        "expected at least one quoted relay name in {list:?}"
    );
    w.replace_my_relay_list(&names, &[]);
}

#[given(regex = r#"^my relay list names only "([^"]+)" as a read-marked relay$"#)]
async fn my_relay_list_is_only_read_marked(w: &mut NmpWorld, relay: String) {
    w.replace_my_relay_list(&[], &[relay]);
}

#[given(regex = r#"^my relay list declares no write relays$"#)]
async fn my_relay_list_declares_no_write_relays(w: &mut NmpWorld) {
    w.declare_no_write_relays(ME);
}

#[given(regex = r#"^(\S+)'s relay list names (.+) as (?:her|his|their) read relays$"#)]
async fn person_read_relays(w: &mut NmpWorld, person: String, list: String) {
    let names = parse_quoted_list(&list);
    assert!(
        !names.is_empty(),
        "expected at least one quoted relay name in {list:?}"
    );
    for relay in names {
        w.declare_read_relay(&person, &relay);
    }
}

/// "his ONE read relay" is the coverage case stated out loud: a list with a
/// single entry is below the 2-relay minimum, which is what arms the
/// per-recipient top-up.
#[given(regex = r#"^(\S+)'s relay list names "([^"]+)" as (?:her|his|their) one read relay$"#)]
async fn person_single_read_relay(w: &mut NmpWorld, person: String, relay: String) {
    w.declare_read_relay(&person, &relay);
}

#[given(regex = r#"^(\S+)'s relay list names "([^"]+)" without marking it read or write$"#)]
async fn person_unmarked_relay(w: &mut NmpWorld, person: String, relay: String) {
    w.declare_unmarked_relay(&person, &relay);
}

// ---- routing: whether the sources have finished looking ------------------

/// Discovery ran to completion and produced nothing for them. Staged as a
/// well-behaved indexer set plus a person with no list, because that is what
/// settled absence IS -- the engine derives it from a real end-of-stored-
/// events on a real subscription, never from anything injected here.
#[given(
    regex = r#"^the indexers have finished their stored events without a relay list for (.+)$"#
)]
async fn indexers_finished_without(w: &mut NmpWorld, who: String) {
    w.indexers_finished_without_a_list_for(&subject(&who));
}

/// The complement: one configured indexer withholds its end-of-stored-events,
/// so nothing settles while the other still answers -- which is what lets a
/// relay list ARRIVE later in the same scenario.
#[given(regex = r#"^the indexers have not yet finished their stored events for (.+)$"#)]
async fn indexers_have_not_finished(w: &mut NmpWorld, who: String) {
    w.indexers_have_not_finished(&subject(&who));
}

/// `my own account` is how these scenarios name the logged-in user where the
/// same sentence otherwise names somebody else. One spelling, one keypair.
fn subject(who: &str) -> String {
    match who.trim() {
        "my own account" | "me" | "myself" => ME.to_string(),
        other => other.to_string(),
    }
}

// ---- routing: a publish that already happened ----------------------------

/// The past-tense form: this write went out during setup, and the scenario is
/// about what became of it afterwards.
///
/// Nothing here decides the STORE. #1018's before-hook reads the whole
/// scenario and gives a durable one to any scenario whose own sentences say it
/// crosses a process boundary, which is a better answer than any single step
/// could give: the requirement is stated in the `.feature`, so the hook and
/// the reader are looking at the same words.
#[given(regex = r#"^I published a note saying "([^"]+)"$"#)]
async fn i_published_a_note(w: &mut NmpWorld, text: String) {
    w.publish_note(&text).await;
}

#[given(regex = r#"^I published a note saying "([^"]+)" that p-tags (.+)$"#)]
async fn i_published_a_note_p_tagging(w: &mut NmpWorld, text: String, people: String) {
    w.publish_note_mentioning(&text, &parse_people(&people))
        .await;
}

#[given(regex = r#"^relay "([^"]+)" holds a kind (\d+) event with h "([^"]+)" saying "([^"]+)"$"#)]
async fn relay_holds_group_event(
    w: &mut NmpWorld,
    relay: String,
    kind: u16,
    group_id: String,
    text: String,
) {
    w.seed_group_event(&relay, kind, &group_id, &text).await;
}
