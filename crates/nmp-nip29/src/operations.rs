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

use nmp_grammar::EventBuilder;
use nostr::{Kind, PublicKey, Tag};

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
    let mut builder = EventBuilder::new(Kind::from(JOIN_REQUEST));
    if let Some(code) = invite_code {
        builder = builder.tag(code_tag(code));
    }
    builder
}

/// kind:9022 -- leave a group; the relay removes the sender automatically.
pub fn leave_request() -> EventBuilder {
    EventBuilder::new(Kind::from(LEAVE_REQUEST))
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
    EventBuilder::new(Kind::from(PUT_USER)).tag(tag)
}

/// kind:9001 -- remove-user: drop a member from the group.
pub fn remove_user(pubkey: PublicKey) -> EventBuilder {
    EventBuilder::new(Kind::from(REMOVE_USER)).tag(Tag::public_key(pubkey))
}

/// Who may READ a group's messages -- NIP-29's `private` marker, and the
/// `public` spelling that clears it (#1282).
///
/// NIP-29 says of kind:39000: *"`private` indicates that only members can
/// _read_ group messages. Omitting this tag indicates that anyone can read
/// group messages."* On the RECORD, presence is the whole statement and
/// absence is the opposite. On a kind:9002 EDIT it cannot be, because an edit
/// that omits every marker must leave the group's settings alone -- so a
/// three-state is unavoidable, and the reference relay's own 9002 parser
/// spells the third state `public` (relay29's `moderation_actions.go`: a
/// `public` row sets its private flag false, a `private` row sets it true,
/// and neither leaves it untouched).
///
/// Named for what NIP-29 itself says the marker controls -- reading group
/// messages -- and its variants are the two literal tag names, so nothing
/// here is a category the protocol lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadAccess {
    /// `["public"]` -- anyone may read the group's messages.
    Public,
    /// `["private"]` -- only members may read the group's messages.
    Private,
}

impl ReadAccess {
    /// The row NIP-29 spells this state with.
    fn marker(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }
}

/// Whether JOIN REQUESTS are honoured -- NIP-29's `closed` marker, and the
/// `open` spelling that clears it (#1282).
///
/// NIP-29 says of kind:39000: *"`closed` indicates that join requests are
/// ignored. Omitting this tag indicates that users can expect join requests to
/// be honored."* Same three-state reasoning as [`ReadAccess`], and the same
/// source for the clearing spelling.
///
/// Independent of [`ReadAccess`]: a group can be publicly readable and still
/// closed to new members, which is exactly what a published workspace is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JoinAccess {
    /// `["open"]` -- join requests are honoured.
    Open,
    /// `["closed"]` -- join requests are ignored.
    Closed,
}

impl JoinAccess {
    /// The row NIP-29 spells this state with.
    fn marker(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// What one kind:9002 edit says about a group (#1282).
///
/// Every field is `Some` to state it or `None` to leave it out of this draft
/// entirely -- an omitted field emits no row at all, so it is not touched and
/// never cleared. That rule is why the two markers are two-valued enums rather
/// than `bool`s: `Some(ReadAccess::Public)` means "make it readable by anyone"
/// and `None` means "do not decide", and one `bool` cannot say both.
///
/// [`Default`] is the empty edit, so
/// `GroupMetadataEdit { picture: Some(url), ..Default::default() }` is the
/// ordinary way to touch one field.
///
/// NIP-29 rows this deliberately does NOT compose: `banner`, `restricted`,
/// `hidden`, `livekit` and `supported_kinds`. `restricted` and `hidden` have
/// no clearing spelling anywhere -- neither NIP-29 nor the reference relay's
/// 9002 parser defines the row that would turn either back off -- so composing
/// the setting half alone would ship a door that can only ever tighten a
/// group. `supported_kinds` is a list whose edit semantics (replace? merge?)
/// NIP-29 does not pin down, which is the same reason kind:9010
/// `update-pin-list` is left uncomposed (see this module's own doc). `banner`
/// and `livekit` are not read by the reference relay's 9002 handler at all, so
/// composing them would emit rows that take no effect and read as though they
/// had. Each is a one-line addition the day a consumer needs it and a
/// falsifier exists for what a relay does with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupMetadataEdit {
    /// The `name` row -- the group's display name.
    pub name: Option<String>,
    /// The `about` row -- the group's description.
    pub about: Option<String>,
    /// The `picture` row. The tag NAME is NIP-29's; which URL goes in it is
    /// entirely the app's product policy.
    pub picture: Option<String>,
    /// Who may read the group's messages.
    pub read_access: Option<ReadAccess>,
    /// Whether join requests are honoured.
    pub join_access: Option<JoinAccess>,
}

/// kind:9002 -- edit-metadata: state part of the group's metadata.
///
/// NIP-29's own moderation table says kind:9002 takes *"all the fields of
/// group-metadata"*, so this composes NIP-29's rows and invents none: `name`,
/// `about` and `picture` are its value rows, and `public`/`private` and
/// `open`/`closed` are the marker rows that decide who may read and whether
/// join requests are honoured.
///
/// Before this, an app that wanted a closed group had to hand-write
/// `["closed"]` itself -- and once it is hand-writing one 9002 row it is
/// hand-assembling a 9002, which is exactly the protocol logic this crate
/// exists to own. `open`/`closed` is not decoration: it is the difference
/// between a workspace and an open room.
///
/// An omitted field emits no row, so it is left untouched rather than cleared.
/// See [`GroupMetadataEdit`] for the rows deliberately left out, and why.
pub fn edit_metadata(edit: GroupMetadataEdit) -> EventBuilder {
    let mut builder = EventBuilder::new(Kind::from(EDIT_METADATA));
    for (name, value) in [
        ("name", edit.name),
        ("about", edit.about),
        ("picture", edit.picture),
    ] {
        if let Some(value) = value {
            builder = builder
                .tag(Tag::parse([name, value.as_str()]).expect("a two-value row is well-formed"));
        }
    }
    for marker in [
        edit.read_access.map(ReadAccess::marker),
        edit.join_access.map(JoinAccess::marker),
    ]
    .into_iter()
    .flatten()
    {
        builder = builder.tag(Tag::parse([marker]).expect("a one-value row is well-formed"));
    }
    builder
}

/// kind:9005 -- delete-event: remove one group-hosted event by id.
pub fn delete_event(event_id: nostr::EventId) -> EventBuilder {
    EventBuilder::new(Kind::from(DELETE_EVENT)).tag(Tag::event(event_id))
}

/// kind:9007 -- create-group: bring a new group into existence at the host.
pub fn create_group() -> EventBuilder {
    EventBuilder::new(Kind::from(CREATE_GROUP))
}

/// kind:9008 -- delete-group: remove a group from the host entirely.
pub fn delete_group() -> EventBuilder {
    EventBuilder::new(Kind::from(DELETE_GROUP))
}

/// kind:9009 -- create-invite: mint an arbitrary code redeemable by
/// [`join_request`].
pub fn create_invite(code: &str) -> EventBuilder {
    EventBuilder::new(Kind::from(CREATE_INVITE)).tag(code_tag(code))
}

fn code_tag(code: &str) -> Tag {
    Tag::parse(["code", code]).expect("'code' is well-formed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn subject() -> PublicKey {
        Keys::generate().public_key()
    }

    fn rows(builder: EventBuilder) -> Vec<Vec<String>> {
        builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    fn kind_of(builder: EventBuilder) -> Kind {
        builder.kind
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
        let edit = || GroupMetadataEdit {
            name: Some("Photographers".to_string()),
            about: Some("film only".to_string()),
            ..GroupMetadataEdit::default()
        };
        assert_eq!(kind_of(edit_metadata(edit())), Kind::from(EDIT_METADATA));
        assert_eq!(
            rows(edit_metadata(edit())),
            vec![
                vec!["name".to_string(), "Photographers".to_string()],
                vec!["about".to_string(), "film only".to_string()],
            ]
        );
    }

    #[test]
    fn edit_metadata_editing_one_field_leaves_the_other_untouched() {
        assert_eq!(
            rows(edit_metadata(GroupMetadataEdit {
                name: Some("Photographers".to_string()),
                ..GroupMetadataEdit::default()
            })),
            vec![vec!["name".to_string(), "Photographers".to_string()]]
        );
        assert_eq!(
            rows(edit_metadata(GroupMetadataEdit {
                about: Some("film only".to_string()),
                ..GroupMetadataEdit::default()
            })),
            vec![vec!["about".to_string(), "film only".to_string()]]
        );
    }

    #[test]
    fn edit_metadata_with_nothing_supplied_carries_no_tag_at_all() {
        assert!(rows(edit_metadata(GroupMetadataEdit::default())).is_empty());
    }

    /// #1282's headline: the whole of what mosaico's surviving hand-built
    /// 9002 builder said, composed through the door instead. Every row here
    /// is NIP-29's own -- only the picture URL is the app's.
    #[test]
    fn edit_metadata_composes_the_picture_row_and_both_marker_rows() {
        let rows = rows(edit_metadata(GroupMetadataEdit {
            name: Some("Workspace".to_string()),
            picture: Some("https://cdn.example/w.png".to_string()),
            read_access: Some(ReadAccess::Public),
            join_access: Some(JoinAccess::Closed),
            ..GroupMetadataEdit::default()
        }));
        assert_eq!(
            rows,
            vec![
                vec!["name".to_string(), "Workspace".to_string()],
                vec![
                    "picture".to_string(),
                    "https://cdn.example/w.png".to_string()
                ],
                vec!["public".to_string()],
                vec!["closed".to_string()],
            ],
            "the value rows carry a value, and each marker is a bare one-value row"
        );
    }

    /// The two markers are INDEPENDENT axes, and each is two-valued. All four
    /// combinations compose, and each composes exactly the tag NIP-29 (and
    /// the reference relay's own 9002 parser) spells for that state -- so a
    /// `public` group that is nevertheless `closed` to new members, which is
    /// what a published workspace is, is expressible.
    #[test]
    fn each_marker_axis_composes_the_exact_tag_nip29_spells_for_that_state() {
        for (read, join, expected) in [
            (ReadAccess::Public, JoinAccess::Open, vec!["public", "open"]),
            (
                ReadAccess::Public,
                JoinAccess::Closed,
                vec!["public", "closed"],
            ),
            (
                ReadAccess::Private,
                JoinAccess::Open,
                vec!["private", "open"],
            ),
            (
                ReadAccess::Private,
                JoinAccess::Closed,
                vec!["private", "closed"],
            ),
        ] {
            assert_eq!(
                rows(edit_metadata(GroupMetadataEdit {
                    read_access: Some(read),
                    join_access: Some(join),
                    ..GroupMetadataEdit::default()
                })),
                expected
                    .iter()
                    .map(|marker| vec![marker.to_string()])
                    .collect::<Vec<Vec<String>>>(),
                "{read:?} + {join:?} must compose exactly {expected:?}"
            );
        }
    }

    /// An unstated marker is UNTOUCHED, not defaulted. `None` emits no row,
    /// so an edit that renames a group cannot silently reopen it -- which a
    /// `bool` defaulting to `false` would have done.
    #[test]
    fn an_unstated_marker_emits_no_row_and_therefore_clears_nothing() {
        assert_eq!(
            rows(edit_metadata(GroupMetadataEdit {
                name: Some("Workspace".to_string()),
                ..GroupMetadataEdit::default()
            })),
            vec![vec!["name".to_string(), "Workspace".to_string()]],
            "renaming a group must not restate -- or reset -- who may read it"
        );
        assert_eq!(
            rows(edit_metadata(GroupMetadataEdit {
                join_access: Some(JoinAccess::Closed),
                ..GroupMetadataEdit::default()
            })),
            vec![vec!["closed".to_string()]],
            "closing a group must not restate its name, about, picture or read access"
        );
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
