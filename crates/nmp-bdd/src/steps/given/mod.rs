//! `Given` — the world before the app acts (approach doc §1.2/§2.4): relay
//! topology, configured operator policy, pre-existing protocol state. Every
//! step here only STAGES data on [`NmpWorld`] -- nothing hits a socket until
//! a later step calls `ensure_started` (most directly via the `my feed ...
//! is open` shorthand below).

use cucumber::gherkin::Step;
use cucumber::given;

use crate::steps::{parse_people, parse_quoted_list};
use crate::world::{parse_kind_list, NmpWorld, ME};

#[given(regex = r#"^(?:only )?(\d+|an) indexer relays? (?:is|are) configured$"#)]
async fn n_indexers(w: &mut NmpWorld, n: String) {
    let n = if n == "an" {
        1
    } else {
        n.parse().expect("nmp-bdd: an indexer count is a number")
    };
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

/// The bare form, and the form that writes the relay's own refusal. NMP has
/// no business paraphrasing a message it did not write, so a scenario that
/// cares about the words supplies them and they reach the receipt verbatim.
#[given(regex = r#"^relay "([^"]+)" rejects every event(?: with "([^"]+)")?$"#)]
async fn relay_rejects_writes(w: &mut NmpWorld, name: String, message: String) {
    let message = (!message.is_empty()).then_some(message);
    w.set_reject_writes(&name, message.as_deref());
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

/// The same declaration as a TABLE. Four destinations read as a list rather
/// than as a sentence, and a scenario whose subject is "four independent
/// fates" is unreadable written the other way.
#[given(regex = r#"^my relay list names these as my write relays:$"#)]
async fn my_write_relays_table(w: &mut NmpWorld, step: &Step) {
    let table = step
        .table
        .as_ref()
        .expect("nmp-bdd: this step is written with a table of relay URLs");
    assert!(
        !table.rows.is_empty(),
        "nmp-bdd: an empty relay table declares nothing"
    );
    for row in &table.rows {
        let relay = row
            .first()
            .expect("nmp-bdd: each row names one relay")
            .trim();
        w.declare_write_relay(ME, relay);
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

/// The name may be an ordinary one (`Alice`) or a key
/// (`"4c26...81f5"`) -- `features/writes/` names its people by the key that
/// signed, and the quotes it writes them in are punctuation, not part of the
/// name. Trimming them here is what makes one label name one keypair across
/// both spellings in the same scenario.
#[given(regex = r#"^(\S+) has posted a note saying "([^"]+)"$"#)]
async fn person_posted_note(w: &mut NmpWorld, person: String, text: String) {
    w.stage_note(person.trim_matches('"'), &text);
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
    w.no_app_relays();
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
    w.stage_note(&person, &text);
}

// ---- the clock ----------------------------------------------------------
//
// The subject of `features/writes/`: an acceptance stamp is whatever the
// reducer's clock said, so a spec that names an instant needs one it can
// state. See `world::clock`.

#[given(regex = r#"^my device clock reads "([^"]+)"$"#)]
async fn device_clock_reads(w: &mut NmpWorld, at: String) {
    w.set_device_clock(&at).await;
}

// ---- an already-signed event a scenario names ---------------------------

/// BINDS the scenario's id word to the event a `Given` staged. See
/// `NmpWorld::bind_signed_event_label` for why a `.feature` cannot spell a
/// real event id and why a binding proves exactly what the scenario claims.
#[given(regex = r#"^that note is the signed event "([0-9a-f]{64})"$"#)]
async fn note_is_the_signed_event(w: &mut NmpWorld, label: String) {
    w.bind_signed_event_label(&label);
}

/// A real forgery: the bytes change and the signature is left alone, which is
/// exactly the payload the acceptance boundary has to catch.
#[given(regex = r#"^the signed event "([0-9a-f]{64})" has had one byte of its content altered$"#)]
async fn signed_event_altered(w: &mut NmpWorld, label: String) {
    w.tamper_signed_event(&label);
}

/// Every `features/writes/` Background states its account by key, because
/// that feature is written for a reader who cares which key signed.
#[given(regex = r#"^I am logged in as the account with pubkey "([0-9a-f]{64})"$"#)]
async fn logged_in_as_pubkey(w: &mut NmpWorld, pubkey: String) {
    w.log_in_as_identity(&pubkey);
}

// ---- routing: three-valued knowledge ------------------------------------
//
// The three values are staged three different ways, and the difference is the
// whole point: a relay list NAMING relays is `Known`, one declaring NONE is
// still `Known` (a fact, just an empty one), and one never ingested is
// `Unknown` -- which, until the indexers finish looking, keeps a write parked.

// `my relay list has never been fetched` is defined once, alongside the
// group staging that first needed it (`NmpWorld::forget_my_relay_list`): it
// UNSTAGES rather than asserts, which is the only spelling that works for
// both a scenario whose Background staged one and a cold-start scenario
// whose Background did not.

/// Every spelling of "we have nothing for them", including the plural form
/// the three-mention case needs. It takes a LIST rather than one name because
/// a scenario that names two unresolved people in one clause and silently
/// matched no step at all would skip — and a skipped `Given` leaves the
/// scenario reading exactly like one that proved something.
#[given(
    regex = r#"^(?:(\S+)'s relay list has never been fetched|no relay list for (.+?) (?:has ever been ingested|exists))$"#
)]
async fn person_relay_list_never_fetched(w: &mut NmpWorld, single: String, list: String) {
    let people = if single.is_empty() {
        parse_people(&list)
    } else {
        vec![single]
    };
    assert!(
        !people.is_empty(),
        "expected at least one person in this step"
    );
    for person in people {
        w.person(&person);
        w.assert_relay_list_never_fetched(&person);
    }
}

#[given(regex = r#"^(\S+)'s relay list names "([^"]+)" as (?:her|his|their) read relay$"#)]
async fn person_read_relay(w: &mut NmpWorld, person: String, relay: String) {
    w.declare_read_relay(&person, &relay);
}

#[given(regex = r#"^(\S+)'s relay list is ingested and names no relays at all$"#)]
async fn person_declares_no_relays(w: &mut NmpWorld, person: String) {
    w.declare_no_relays(&person);
}

/// The indexers have NOT finished looking, so nothing can settle and every
/// unknown stays unknown. Staged as relay behaviour rather than as an
/// injected fact: a relay that never says end-of-stored-events is exactly
/// what "we have not finished looking" IS on the wire.
#[given(
    regex = r#"^the indexers have not(?: yet)? confirmed end of stored events for (\S+)'s relay list$"#
)]
async fn indexers_have_not_confirmed(w: &mut NmpWorld, person: String) {
    w.person(&person);
    w.indexers_never_confirm_end_of_stored_events();
}

/// The complement, stated out loud where a scenario turns on it: a
/// well-behaved relay answers end-of-stored-events, which is the ONLY thing
/// that turns "we have not looked" into "we looked and there is nothing".
#[given(
    regex = r#"^the indexers have (?:already )?confirmed end of stored events for (\S+)'s relay list$"#
)]
async fn indexers_have_confirmed(w: &mut NmpWorld, person: String) {
    w.person(&person);
    w.assert_indexers_confirm_end_of_stored_events();
}

#[given(regex = r#"^no indexer relays are configured$"#)]
async fn no_indexers_configured(w: &mut NmpWorld) {
    w.assert_no_indexers();
}

/// A signer for the current account already exists in this world (every
/// scenario that logs in gets one), stated out loud where a scenario's point
/// is that the signer was NOT asked for anything.
#[given(regex = r#"^a signer is registered for the current pubkey$"#)]
async fn signer_is_registered(w: &mut NmpWorld) {
    w.assert_signer_registered();
}

// ---- identity: who exists, and who can sign -----------------------------
//
// The subject of `features/identity/`. These name accounts by PUBKEY rather
// than by a person's name, because that feature is written for a reader who
// cares which key signed. Each hex string is an ordinary fixture-person label
// (`NmpWorld::person`), so one hex names one keypair for the whole scenario.

#[given(
    regex = r#"^the account with pubkey "([0-9a-f]{64})" is registered with a working signer$"#
)]
async fn account_registered_with_signer(w: &mut NmpWorld, pubkey: String) {
    w.register_identity_with_signer(&pubkey);
}

#[given(regex = r#"^my podcast identity "([0-9a-f]{64})" is registered with a working signer$"#)]
async fn podcast_identity_registered(w: &mut NmpWorld, pubkey: String) {
    w.register_podcast_identity(&pubkey);
}

/// The keypair exists and can be named; nothing in the world can sign for it.
/// That gap is the entire subject of `awaiting-signer.feature`.
#[given(regex = r#"^no signer is registered for "([0-9a-f]{64})"$"#)]
async fn no_signer_registered_for(w: &mut NmpWorld, pubkey: String) {
    w.register_identity_without_signer(&pubkey);
}

#[given(regex = r#"^"([0-9a-f]{64})" is the active account$"#)]
async fn identity_is_active(w: &mut NmpWorld, pubkey: String) {
    w.activate_identity(&pubkey).await;
}

#[given(regex = r#"^no account is active$"#)]
async fn no_account_is_active(w: &mut NmpWorld) {
    w.no_account_is_active();
}

#[given(regex = r#"^the podcast identity's signer is slow to answer$"#)]
async fn podcast_signer_is_slow(w: &mut NmpWorld) {
    let label = w.podcast_identity();
    w.signer_is_slow(&label);
}

#[given(regex = r#"^that account's signer is slow to answer$"#)]
async fn that_accounts_signer_is_slow(w: &mut NmpWorld) {
    let label = w.current_identity();
    w.signer_is_slow(&label);
}

#[given(regex = r#"^that account's signer is offline$"#)]
async fn that_accounts_signer_is_offline(w: &mut NmpWorld) {
    let label = w.current_identity();
    w.signer_is_offline(&label);
}

/// The display form really is one: the bech32 rendering of the key this world
/// minted for that label, arriving where display forms actually arrive.
#[given(regex = r#"^the user pasted the npub form of "([0-9a-f]{64})" into the identity picker$"#)]
async fn user_pasted_npub(w: &mut NmpWorld, pubkey: String) {
    w.paste_npub_of(&pubkey);
}

// ---- NIP-29 groups (features/groups/) ----------------------------------
//
// A group is `(host, group_id)` and nothing else, so every `Given` here is
// either that identity, the app-supplied read selection the group refuses to
// invent for it, or the draft/signed event a later `When` hands the door.

/// `Given the group "photographers" hosted by relay "wss://..."` -- and the
/// `also hosted by` form, which is the same staging said twice about one
/// relay (two groups on ONE host is the case `#h` scoping has to separate).
#[given(regex = r#"^the group "([^"]+)" (?:also )?hosted by relay "([^"]+)"$"#)]
async fn stage_group(w: &mut NmpWorld, group_id: String, relay: String) {
    w.stage_group(&group_id, &relay);
}

/// The account named by its own key material. See `NmpWorld::log_in_as_key`
/// for why the keypair is derived from the hex rather than minted.
#[given(regex = r#"^I am logged in as "([0-9a-fA-F]{64})"$"#)]
async fn logged_in_as_key(w: &mut NmpWorld, secret_hex: String) {
    w.log_in_as_key(&secret_hex);
}

#[given(regex = r#"^"([0-9a-fA-F]{64})" names "([^"]+)" as their write relay$"#)]
async fn key_write_relay(w: &mut NmpWorld, secret_hex: String, relay: String) {
    w.declare_write_relay(&secret_hex, &relay);
}

// ---- the global stalled-write list -------------------------------------

/// The destination is a LITERAL URL this world deliberately never starts --
/// see `world::stalled::told_to_publish_to` for why registering it as an
/// ordinary scripted relay would delete the case.
#[given(regex = r#"^I am told to publish a note to exactly "([^"]+)"$"#)]
async fn told_to_publish_to(w: &mut NmpWorld, url: String) {
    w.told_to_publish_to(&url);
}

/// Accepted, signed, and routed, so whatever fails next is a DELIVERY
/// failure and never a signing or routing one.
#[given(regex = r#"^a note saying "([^"]+)" was published and signed$"#)]
async fn note_published_and_signed(w: &mut NmpWorld, text: String) {
    w.publish_and_await_signature(&text).await;
}

#[given(regex = r#"^my relay list has never been fetched$"#)]
async fn relay_list_never_fetched(w: &mut NmpWorld) {
    w.forget_my_relay_list();
}

/// The read half of "I can write into a group whose content I cannot read":
/// the host answers the query with a refusal, which is what a NIP-29 relay
/// does to a non-member.
#[given(regex = r#"^relay "([^"]+)" refuses my reads until I am a member$"#)]
async fn relay_refuses_my_reads(w: &mut NmpWorld, relay: String) {
    w.set_reject_queries(&relay);
}

/// Both spellings of the same staged fact: the relay is bound (so it has a
/// real URL and a real port nobody else can take) and then severed, so a
/// connection attempt is REFUSED rather than quietly succeeding against a
/// relay that answers nothing.
#[given(regex = r#"^relay "([^"]+)" cannot (?:connect|be connected to)$"#)]
async fn relay_cannot_connect(w: &mut NmpWorld, relay: String) {
    w.set_unreachable(&relay);
}

#[given(regex = r#"^relay "([^"]+)" rejects kind (\d+) with "([^"]+)"$"#)]
async fn relay_rejects_kind(w: &mut NmpWorld, relay: String, kind: u16, message: String) {
    w.set_reject_kind(&relay, kind, &message);
}

#[given(regex = r#"^signing fails for this account$"#)]
async fn signing_fails(w: &mut NmpWorld) {
    w.fail_signing();
}

/// Stated out loud where a scenario's point is that a WRITE needed no read
/// first. Nothing to stage: it is the world's default, and asserting it here
/// keeps a later "no subscription existed" from being vacuously true because
/// an earlier step quietly opened one.
#[given(regex = r#"^I have never observed anything from this group$"#)]
async fn never_observed_this_group(w: &mut NmpWorld) {
    w.assert_no_group_observation();
}

/// The host decides who may moderate; NMP holds no opinion to state. Nothing
/// is staged because there is nothing in NMP for this to configure -- which
/// is exactly the claim the paired `Then` makes.
#[given(regex = r#"^I am not an admin of "([^"]+)"$"#)]
async fn not_an_admin(w: &mut NmpWorld, group_id: String) {
    w.assert_no_permission_claim(&group_id);
}

/// A relay the engine could plausibly widen a group read to, present so that
/// "the pinned set was never widened" has something to have been widened to.
#[given(regex = r#"^the engine later learns of relay "([^"]+)" for this group's members$"#)]
async fn engine_learns_of_gossip_relay(w: &mut NmpWorld, relay: String) {
    w.declare_write_relay("group-member", &relay);
}

/// The APP's own kind selection. The group imposes no catalogue, so every
/// read scenario has to say which kinds it wants.
#[given(regex = r#"^an? (?:chat |activity |reactions |membership )?filter selecting (.+)$"#)]
async fn stage_filter(w: &mut NmpWorld, kinds: String) {
    w.stage_filter(parse_kind_list(&kinds));
}

#[given(regex = r#"^an unsigned event of kind (\d+) with content "([^"]+)"$"#)]
async fn stage_draft(w: &mut NmpWorld, kind: u16, content: String) {
    w.stage_draft(kind, &content);
}

#[given(regex = r#"^that event carries the tags "([^"]+)"="([^"]+)" and "([^"]+)"="([^"]+)"$"#)]
async fn draft_carries_tags(w: &mut NmpWorld, a: String, av: String, b: String, bv: String) {
    w.draft_add_tag(&a, &av);
    w.draft_add_tag(&b, &bv);
}

#[given(regex = r#"^that event carries a created_at the app chose$"#)]
async fn draft_carries_created_at(w: &mut NmpWorld) {
    w.draft_chooses_created_at();
}

#[given(regex = r#"^that event (?:already )?carries an h tag with value "([^"]+)"$"#)]
async fn draft_carries_h(w: &mut NmpWorld, value: String) {
    w.draft_add_tag("h", &value);
}

#[given(regex = r#"^that event (?:already )?carries a previous tag$"#)]
async fn draft_carries_previous(w: &mut NmpWorld) {
    w.draft_add_tag("previous", "deadbeef");
}

#[given(
    regex = r#"^an event signed earlier by "([0-9a-fA-F]{64})" of kind (\d+) with content "([^"]+)"$"#
)]
async fn stage_signed_event(w: &mut NmpWorld, author: String, kind: u16, content: String) {
    w.stage_signed_event(&author, kind, &content);
}

#[given(regex = r#"^that signed event carries an h tag with value "([^"]+)"$"#)]
async fn signed_event_carries_h(w: &mut NmpWorld, value: String) {
    w.signed_event_add_tag("h", &value);
}

#[given(regex = r#"^that signed event carries h tags with values "([^"]+)" and "([^"]+)"$"#)]
async fn signed_event_carries_two_h(w: &mut NmpWorld, first: String, second: String) {
    w.signed_event_add_tag("h", &first);
    w.signed_event_add_tag("h", &second);
}

/// Stated out loud, and staged as nothing: the event is built from the parts
/// above and no `h` is among them.
#[given(regex = r#"^that signed event carries no h tag$"#)]
async fn signed_event_carries_no_h(w: &mut NmpWorld) {
    w.assert_signed_event_has_no_context();
}

/// BINDS the scenario's id word to the id the event actually got. See
/// `NmpWorld::id_labels` for why a real id cannot be written in a `.feature`
/// and why a binding proves exactly what the scenario claims.
#[given(regex = r#"^that signed event has id "([^"]+)"$"#)]
async fn signed_event_has_id(w: &mut NmpWorld, label: String) {
    w.bind_id_label(&label);
}

/// The pre-signed path's whole point: the id exists BEFORE publication, so an
/// observation can already be armed on it.
#[given(regex = r#"^I am observing a live query for exactly that id$"#)]
async fn observing_that_id(w: &mut NmpWorld) {
    let id = w.signed_event().id;
    w.observe_exact_id(id, None).await;
}

// The outbox family lives next door for the same reason `then/` is a
// directory: this catalog is shared by every feature, and one family's whole
// vocabulary is readable on its own only when it has a name. See
// `given::outbox`.
mod outbox;
