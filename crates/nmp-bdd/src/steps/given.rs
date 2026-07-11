//! `Given` — the world before the app acts (approach doc §1.2/§2.4): relay
//! topology, configured operator policy, pre-existing protocol state. Every
//! step here only STAGES data on [`NmpWorld`] -- nothing hits a socket until
//! a later step calls `ensure_started` (most directly via the `my feed ...
//! is open` shorthand below).

use cucumber::given;

use crate::steps::parse_people;
use crate::world::{NmpWorld, ME};

fn parse_quoted_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = raw.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '"' {
            if let Some(end) = raw[i + 1..].find('"') {
                out.push(raw[i + 1..i + 1 + end].to_string());
                // Skip past the closing quote.
                while let Some((j, _)) = chars.peek() {
                    if *j > i + 1 + end {
                        break;
                    }
                    chars.next();
                }
            }
        }
    }
    out
}

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

#[given(regex = r#"^my feed of my follows' notes is open$"#)]
async fn my_feed_is_open(w: &mut NmpWorld) {
    w.open_my_follows_feed().await;
}

#[cfg(test)]
mod tests {
    use super::parse_quoted_list;

    #[test]
    fn extracts_a_single_quoted_name() {
        assert_eq!(parse_quoted_list(r#""alice-relay""#), vec!["alice-relay"]);
    }

    #[test]
    fn extracts_several_quoted_names_regardless_of_joiners() {
        assert_eq!(
            parse_quoted_list(r#""relay-a", and "relay-b""#),
            vec!["relay-a", "relay-b"]
        );
        assert_eq!(
            parse_quoted_list(r#""good-relay" and "flaky-relay""#),
            vec!["good-relay", "flaky-relay"]
        );
    }
}
