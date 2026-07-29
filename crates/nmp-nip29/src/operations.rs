//! NIP-29's own kinds (join/leave/moderation, 9000-9022) -- pure event
//! composition (#989).
//!
//! Unlike kind:9 chat (`nmp-nipc7`'s) or the NIP-51 simple-groups list
//! (`nmp-nip51`'s), the schema for join, leave, put-user, remove-user,
//! edit-metadata, delete-event, create-group, delete-group and create-invite
//! is genuinely NIP-29's own -- <https://github.com/nostr-protocol/nips/blob/master/29.md>.
//! Owning it here is what lets an app write `group.remove_user(pubkey)`
//! instead of looking up kind 9001 and hand-assembling a `p` tag itself.
//!
//! Every function here returns a plain [`EventBuilder`]: kind and tags only,
//! no pubkey, no signature, no `h` tag. The `h` tag names which group a draft
//! belongs to and is minted in exactly one place, at publish time, by
//! whatever contextualizes a draft for a selected group host -- not here.
//! That keeps this module engine-free: it composes NIP-29's schema, it does
//! not route or sign (`crates/nmp-nip29/Cargo.toml` depends on exactly
//! `nostr` + `nmp-grammar`, enforced by
//! `scripts/check-nip29-ownership.sh:30-33`).
//!
//! `update-pin-list` (kind 9010) is deliberately not composed here: its
//! tag shape ("zero or more `e` or `a` tags") does not pin down what a typed
//! signature should look like without inventing one, so it is left for
//! whoever needs it to decide with a concrete falsifier in hand.

use nostr::{EventBuilder, Kind, PublicKey, Tag};

const JOIN_REQUEST: u16 = 9021;
const LEAVE_REQUEST: u16 = 9022;
const PUT_USER: u16 = 9000;
const REMOVE_USER: u16 = 9001;
const EDIT_METADATA: u16 = 9002;
const DELETE_EVENT: u16 = 9005;
const CREATE_GROUP: u16 = 9007;
const DELETE_GROUP: u16 = 9008;
const CREATE_INVITE: u16 = 9009;

/// kind:9021 -- request admission to a group.
///
/// `invite_code` becomes a `["code", "<code>"]` tag when supplied. When it is
/// `None` no `code` tag is added at all -- never an empty one.
pub fn join_request(invite_code: Option<&str>) -> EventBuilder {
    let mut builder = EventBuilder::new(Kind::from(JOIN_REQUEST), "");
    if let Some(code) = invite_code {
        builder = builder.tag(code_tag(code));
    }
    builder
}

/// kind:9022 -- leave a group; the relay removes the sender automatically.
pub fn leave_request() -> EventBuilder {
    EventBuilder::new(Kind::from(LEAVE_REQUEST), "")
}

/// kind:9000 -- put-user: add a member, optionally granting a role.
///
/// `role` becomes a third value on the `p` tag
/// (`["p", "<pubkey-hex>", "<role>"]`) when supplied; with no role the tag is
/// the plain `["p", "<pubkey-hex>"]`.
pub fn add_user(pubkey: PublicKey, role: Option<&str>) -> EventBuilder {
    let tag = match role {
        Some(role) => Tag::parse(["p", &pubkey.to_hex(), role]).expect("'p' is well-formed"),
        None => Tag::public_key(pubkey),
    };
    EventBuilder::new(Kind::from(PUT_USER), "").tag(tag)
}

/// kind:9001 -- remove-user: drop a member from the group.
pub fn remove_user(pubkey: PublicKey) -> EventBuilder {
    EventBuilder::new(Kind::from(REMOVE_USER), "").tag(Tag::public_key(pubkey))
}

/// kind:9002 -- edit-metadata: set the group's display fields.
///
/// Each field is `Some` to set it or `None` to leave it out of this draft
/// entirely -- an omitted field is not touched, never cleared, because no tag
/// for it is emitted at all.
pub fn edit_metadata(name: Option<&str>, about: Option<&str>) -> EventBuilder {
    let mut builder = EventBuilder::new(Kind::from(EDIT_METADATA), "");
    if let Some(name) = name {
        builder = builder.tag(Tag::parse(["name", name]).expect("'name' is well-formed"));
    }
    if let Some(about) = about {
        builder = builder.tag(Tag::parse(["about", about]).expect("'about' is well-formed"));
    }
    builder
}

/// kind:9005 -- delete-event: remove one group-hosted event by id.
pub fn delete_event(event_id: nostr::EventId) -> EventBuilder {
    EventBuilder::new(Kind::from(DELETE_EVENT), "").tag(Tag::event(event_id))
}

/// kind:9007 -- create-group: bring a new group into existence at the host.
pub fn create_group() -> EventBuilder {
    EventBuilder::new(Kind::from(CREATE_GROUP), "")
}

/// kind:9008 -- delete-group: remove a group from the host entirely.
pub fn delete_group() -> EventBuilder {
    EventBuilder::new(Kind::from(DELETE_GROUP), "")
}

/// kind:9009 -- create-invite: mint an arbitrary code redeemable by
/// [`join_request`].
pub fn create_invite(code: &str) -> EventBuilder {
    EventBuilder::new(Kind::from(CREATE_INVITE), "").tag(code_tag(code))
}

fn code_tag(code: &str) -> Tag {
    Tag::parse(["code", code]).expect("'code' is well-formed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{Keys, Timestamp};

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn subject() -> PublicKey {
        Keys::generate().public_key()
    }

    fn rows(builder: EventBuilder) -> Vec<Vec<String>> {
        builder
            .custom_created_at(Timestamp::from(1_700_000_000u64))
            .build(author())
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    fn kind_of(builder: EventBuilder) -> Kind {
        builder
            .custom_created_at(Timestamp::from(1_700_000_000u64))
            .build(author())
            .kind
    }

    #[test]
    fn join_request_with_code_carries_kind_h_free_code_tag() {
        assert_eq!(
            kind_of(join_request(Some("dark-slide-42"))),
            Kind::from(JOIN_REQUEST)
        );
        assert_eq!(
            rows(join_request(Some("dark-slide-42"))),
            vec![vec!["code".to_string(), "dark-slide-42".to_string()]]
        );
    }

    #[test]
    fn join_request_with_no_code_carries_no_code_tag_and_no_empty_tag() {
        assert_eq!(kind_of(join_request(None)), Kind::from(JOIN_REQUEST));
        assert!(rows(join_request(None)).is_empty());
    }

    #[test]
    fn leave_request_is_kind_9022_with_no_tags() {
        assert_eq!(kind_of(leave_request()), Kind::from(LEAVE_REQUEST));
        assert!(rows(leave_request()).is_empty());
    }

    #[test]
    fn add_user_carries_kind_9000_and_a_bare_p_tag() {
        let pubkey = subject();
        assert_eq!(kind_of(add_user(pubkey, None)), Kind::from(PUT_USER));
        assert_eq!(
            rows(add_user(pubkey, None)),
            vec![vec!["p".to_string(), pubkey.to_hex()]]
        );
    }

    #[test]
    fn add_user_with_role_carries_the_role_on_the_p_tag() {
        let pubkey = subject();
        assert_eq!(
            rows(add_user(pubkey, Some("moderator"))),
            vec![vec![
                "p".to_string(),
                pubkey.to_hex(),
                "moderator".to_string()
            ]]
        );
    }

    #[test]
    fn remove_user_carries_kind_9001_and_a_p_tag() {
        let pubkey = subject();
        assert_eq!(kind_of(remove_user(pubkey)), Kind::from(REMOVE_USER));
        assert_eq!(
            rows(remove_user(pubkey)),
            vec![vec!["p".to_string(), pubkey.to_hex()]]
        );
    }

    #[test]
    fn edit_metadata_carries_both_fields_when_both_are_supplied() {
        assert_eq!(
            kind_of(edit_metadata(Some("Photographers"), Some("film only"))),
            Kind::from(EDIT_METADATA)
        );
        assert_eq!(
            rows(edit_metadata(Some("Photographers"), Some("film only"))),
            vec![
                vec!["name".to_string(), "Photographers".to_string()],
                vec!["about".to_string(), "film only".to_string()],
            ]
        );
    }

    #[test]
    fn edit_metadata_editing_one_field_leaves_the_other_untouched() {
        assert_eq!(
            rows(edit_metadata(Some("Photographers"), None)),
            vec![vec!["name".to_string(), "Photographers".to_string()]]
        );
        assert_eq!(
            rows(edit_metadata(None, Some("film only"))),
            vec![vec!["about".to_string(), "film only".to_string()]]
        );
    }

    #[test]
    fn edit_metadata_with_nothing_supplied_carries_no_tag_at_all() {
        assert!(rows(edit_metadata(None, None)).is_empty());
    }

    #[test]
    fn delete_event_carries_kind_9005_and_an_e_tag() {
        let target = nostr::EventId::from_slice(&[9; 32]).unwrap();
        assert_eq!(kind_of(delete_event(target)), Kind::from(DELETE_EVENT));
        assert_eq!(
            rows(delete_event(target)),
            vec![vec!["e".to_string(), target.to_hex()]]
        );
    }

    #[test]
    fn create_group_is_kind_9007_with_no_tags() {
        assert_eq!(kind_of(create_group()), Kind::from(CREATE_GROUP));
        assert!(rows(create_group()).is_empty());
    }

    #[test]
    fn delete_group_is_kind_9008_with_no_tags() {
        assert_eq!(kind_of(delete_group()), Kind::from(DELETE_GROUP));
        assert!(rows(delete_group()).is_empty());
    }

    #[test]
    fn create_invite_carries_kind_9009_and_the_code_tag() {
        assert_eq!(
            kind_of(create_invite("dark-slide-42")),
            Kind::from(CREATE_INVITE)
        );
        assert_eq!(
            rows(create_invite("dark-slide-42")),
            vec![vec!["code".to_string(), "dark-slide-42".to_string()]]
        );
    }
}
