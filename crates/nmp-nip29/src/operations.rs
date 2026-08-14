//! NIP-29's own kinds (join/leave/moderation, 9000-9022) -- pure event
//! composition (#989).
//!
//! Unlike kind:9 chat (`nmp-nipc7`'s), the schema for join, leave, put-user,
//! remove-user,
//! edit-metadata, delete-event, create-group, delete-group and create-invite
//! is genuinely NIP-29's own -- <https://github.com/nostr-protocol/nips/blob/master/29.md>.
//! Owning it here is what lets an app write `group.remove_users(pubkeys)`
//! instead of looking up kind 9001 and hand-assembling `p` tags itself.
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
//!
//! Subgroups (NIP-29's `Subgroups` section, merged as nostr-protocol/nips#2319
//! on 2026-07-16) are stated on [`create_group`] and nowhere else. That
//! function documents the live probe behind the choice, including the one
//! place NIP-29's prose and its only implementation disagree.

use std::collections::{BTreeMap, BTreeSet};

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

/// One user named by a kind:9000 put-user operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupUser {
    pub pubkey: PublicKey,
    pub role: Option<String>,
}

impl GroupUser {
    pub fn new(pubkey: PublicKey, role: Option<String>) -> Self {
        Self { pubkey, role }
    }
}

/// Why a multi-user moderation event could not be composed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupUsersError {
    NoUsers,
    ConflictingRoles { pubkey: PublicKey },
}

impl std::fmt::Display for GroupUsersError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoUsers => write!(f, "a NIP-29 user operation must name at least one user"),
            Self::ConflictingRoles { pubkey } => write!(
                f,
                "NIP-29 user operation names {pubkey} with conflicting roles"
            ),
        }
    }
}

impl std::error::Error for GroupUsersError {}

/// kind:9000 -- put-user: add several members in ONE event, optionally
/// granting each one a role.
///
/// A role becomes the third value on that user's `p` tag
/// (`["p", "<pubkey-hex>", "<role>"]`); with no role the row is the plain
/// `["p", "<pubkey-hex>"]`. Exact duplicates collapse deterministically.
/// Naming one pubkey with different roles is refused rather than emitting an
/// ambiguous moderation command.
pub fn add_users(
    users: impl IntoIterator<Item = GroupUser>,
) -> Result<EventBuilder, GroupUsersError> {
    let mut unique = BTreeMap::new();
    for user in users {
        match unique.get(&user.pubkey) {
            Some(role) if role != &user.role => {
                return Err(GroupUsersError::ConflictingRoles {
                    pubkey: user.pubkey,
                });
            }
            Some(_) => {}
            None => {
                unique.insert(user.pubkey, user.role);
            }
        }
    }
    if unique.is_empty() {
        return Err(GroupUsersError::NoUsers);
    }

    let mut builder = EventBuilder::new(Kind::from(PUT_USER));
    for (pubkey, role) in unique {
        let tag = match role {
            Some(role) => Tag::parse(vec!["p".to_string(), pubkey.to_hex(), role])
                .expect("a public key and role form a p row"),
            None => Tag::public_key(pubkey),
        };
        builder = builder.tag(tag);
    }
    Ok(builder)
}

/// kind:9001 -- remove-user: drop several members in ONE event. Exact
/// duplicates collapse deterministically.
pub fn remove_users(
    pubkeys: impl IntoIterator<Item = PublicKey>,
) -> Result<EventBuilder, GroupUsersError> {
    let unique: BTreeSet<_> = pubkeys.into_iter().collect();
    if unique.is_empty() {
        return Err(GroupUsersError::NoUsers);
    }

    let mut builder = EventBuilder::new(Kind::from(REMOVE_USER));
    for pubkey in unique {
        builder = builder.tag(Tag::public_key(pubkey));
    }
    Ok(builder)
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
///
/// `parent` is absent for that last reason specifically, and #1301 establishes
/// it by probe rather than by inference: NIP-29's `Subgroups` section puts
/// parenting on kind:9002, and the only relay that implements subgroups reads
/// `parent` on kind:9007 and ignores it on kind:9002. It is therefore stated
/// on [`create_group`], which documents the probe.
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

/// kind:9007 -- create-group: bring a new group into existence at the host,
/// optionally as a SUBGROUP of one that already exists there (#1301).
///
/// `parent` is the parent's group id -- NIP-29's own `d` value, a relay-scoped
/// string, never an `naddr` and never a key. `Some(id)` composes
/// `["parent", "<id>"]`; `None` composes no row at all, never an empty one,
/// and NIP-29 says exactly that means a root group: *"A group without a
/// `parent` tag is a root group."*
///
/// # Why the row rides on kind:9007 and not on kind:9002
///
/// Subgroups are NIP-29 proper, not a proposal: nostr-protocol/nips#2319
/// merged on 2026-07-16 and NIP-29 now carries a `Subgroups` section defining
/// `parent` and `child`. So the row NAME below is the protocol's own and
/// invents nothing.
///
/// Which KIND carries it is a different question, and there the spec and the
/// only implementation disagree. NIP-29's prose says *"Parenting and promotion
/// to root are triggered by a `kind:9002` (`edit-metadata`) event ... with the
/// desired `parent` value"* and never mentions kind:9007. The relay both
/// consumer applications actually use does the exact opposite. Probed live
/// against `wss://nip29.f7z.io` on 2026-08-05:
///
/// * A kind:9007 carrying `["parent", "<id>"]` is HONOURED -- the new group's
///   relay-authored kind:39000 comes back carrying `["parent", "<id>"]` and
///   the parent's own kind:39000 gains `["child", "<new-id>"]`. It is also
///   VALIDATED there: a parent that does not exist is refused with
///   `restricted: parent group '<id>' doesn't exist`, and a signer who is not
///   a parent admin with `restricted: must be an admin of the parent group`.
/// * A kind:9002 carrying `["parent", "<other>"]` beside a `name` row is
///   accepted and the `parent` row is IGNORED -- the name changes and the
///   group stays under its original parent. A kind:9002 carrying `parent`
///   ALONE is refused outright with `invalid moderation action: missing
///   metadata tags`, which is the reference relay's own error for a 9002 in
///   which it recognised no field: relay29's `moderation_actions.go` reads
///   `name`, `picture`, `about`, `public`/`private` and `open`/`closed`, and
///   contains no `parent` or `child` code at all.
///
/// So [`GroupMetadataEdit`] deliberately has no `parent` field. Composing one
/// would emit a row that takes no effect while reading as though it had --
/// the same reason `banner` and `livekit` are left out of the 9002 edit
/// (#1287) -- except worse here, because an app would believe it had moved a
/// group under a new parent when it had not. On the one relay that implements
/// subgroups a group's parent cannot be restated after creation at all, which
/// is what makes it identity rather than metadata.
///
/// This divergence is recorded rather than absorbed: if a relay ever honours
/// `parent` on a 9002, the field is a one-line addition to
/// [`GroupMetadataEdit`] the day a falsifier exists for it.
pub fn create_group(parent: Option<&str>) -> EventBuilder {
    let mut builder = EventBuilder::new(Kind::from(CREATE_GROUP));
    if let Some(parent) = parent {
        builder =
            builder.tag(Tag::parse(["parent", parent]).expect("a two-value row is well-formed"));
    }
    builder
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
    fn add_users_carries_kind_9000_and_a_bare_p_tag() {
        let pubkey = subject();
        let compose = || add_users([GroupUser::new(pubkey, None)]).unwrap();
        assert_eq!(kind_of(compose()), Kind::from(PUT_USER));
        assert_eq!(
            rows(compose()),
            vec![vec!["p".to_string(), pubkey.to_hex()]]
        );
    }

    #[test]
    fn add_users_with_role_carries_the_role_on_the_p_tag() {
        let pubkey = subject();
        assert_eq!(
            rows(add_users([GroupUser::new(pubkey, Some("moderator".to_string()),)]).unwrap()),
            vec![vec![
                "p".to_string(),
                pubkey.to_hex(),
                "moderator".to_string()
            ]]
        );
    }

    #[test]
    fn remove_users_carries_kind_9001_and_a_p_tag() {
        let pubkey = subject();
        let compose = || remove_users([pubkey]).unwrap();
        assert_eq!(kind_of(compose()), Kind::from(REMOVE_USER));
        assert_eq!(
            rows(compose()),
            vec![vec!["p".to_string(), pubkey.to_hex()]]
        );
    }

    #[test]
    fn add_users_composes_one_event_with_every_user_once_in_pubkey_order() {
        let alice = subject();
        let bob = subject();
        let mut expected = vec![
            vec!["p".to_string(), alice.to_hex(), "admin".to_string()],
            vec!["p".to_string(), bob.to_hex()],
        ];
        expected.sort();

        assert_eq!(
            rows(
                add_users([
                    GroupUser::new(bob, None),
                    GroupUser::new(alice, Some("admin".to_string())),
                    GroupUser::new(bob, None),
                ])
                .unwrap()
            ),
            expected
        );
    }

    #[test]
    fn remove_users_composes_one_event_with_every_pubkey_once_in_order() {
        let alice = subject();
        let bob = subject();
        let mut expected = vec![
            vec!["p".to_string(), alice.to_hex()],
            vec!["p".to_string(), bob.to_hex()],
        ];
        expected.sort();

        assert_eq!(rows(remove_users([bob, alice, bob]).unwrap()), expected);
    }

    #[test]
    fn user_batches_refuse_empty_and_conflicting_role_inputs() {
        let pubkey = subject();
        assert_eq!(
            add_users(std::iter::empty::<GroupUser>()).unwrap_err(),
            GroupUsersError::NoUsers
        );
        assert_eq!(
            remove_users(std::iter::empty::<PublicKey>()).unwrap_err(),
            GroupUsersError::NoUsers
        );
        assert_eq!(
            add_users([
                GroupUser::new(pubkey, None),
                GroupUser::new(pubkey, Some("admin".to_string())),
            ])
            .unwrap_err(),
            GroupUsersError::ConflictingRoles { pubkey }
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
    fn create_group_at_the_root_is_kind_9007_with_no_tag_at_all() {
        assert_eq!(kind_of(create_group(None)), Kind::from(CREATE_GROUP));
        assert!(
            rows(create_group(None)).is_empty(),
            "a root group states no parent, and states it by carrying no row -- never an empty one"
        );
    }

    /// The row rides on the kind:9007 CREATE because that is the one place a
    /// relay implementing subgroups reads it. See [`create_group`] for the
    /// probe that establishes this, and for why kind:9002 gets no such field.
    #[test]
    fn create_group_under_a_parent_carries_the_parent_row_on_the_create() {
        assert_eq!(
            kind_of(create_group(Some("darkroom"))),
            Kind::from(CREATE_GROUP)
        );
        assert_eq!(
            rows(create_group(Some("darkroom"))),
            vec![vec!["parent".to_string(), "darkroom".to_string()]]
        );
    }

    /// A parent is a group id -- NIP-29's own `d` value, a relay-scoped
    /// string. It is never an `naddr` and never a key, so there is nothing to
    /// decode and nothing this composer may re-spell: whatever the app names,
    /// the row carries verbatim.
    #[test]
    fn the_parent_row_carries_the_group_id_verbatim() {
        assert_eq!(
            rows(create_group(Some("Photographers/Darkroom 2"))),
            vec![vec![
                "parent".to_string(),
                "Photographers/Darkroom 2".to_string()
            ]]
        );
    }

    /// The 9002 edit door must NOT grow a parent field. The reference relay's
    /// 9002 parser reads none, so composing one would emit a row that takes
    /// no effect while reading as though it had -- and an app would believe it
    /// had re-parented a group when it had not.
    #[test]
    fn a_metadata_edit_never_composes_a_parent_row() {
        let rows = rows(edit_metadata(GroupMetadataEdit {
            name: Some("Darkroom".to_string()),
            about: Some("film only".to_string()),
            picture: Some("https://cdn.example/p.png".to_string()),
            read_access: Some(ReadAccess::Public),
            join_access: Some(JoinAccess::Closed),
        }));
        assert!(
            !rows
                .iter()
                .any(|row| row.first().map(String::as_str) == Some("parent")),
            "parenting is stated on the kind:9007 create; a 9002 that carried the row would be \
             ignored by the only relay implementing subgroups"
        );
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
