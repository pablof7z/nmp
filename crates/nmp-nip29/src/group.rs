//! `Group` -- the one app-facing door into a NIP-29 relay-based group
//! (#977, `docs/internals/nip29/group-publication.md`).
//!
//! A `Group` is an IDENTITY, not a subscription: it is `(host, group_id)` and
//! nothing else, so a kind:9021 join request into a group you cannot read yet
//! is expressible. From that identity it mints both halves of a group's
//! traffic:
//!
//! - a read [`Demand`], which the app takes through the ordinary one read
//!   door (`engine.observe(LiveQuery(group.demand(filter)), None)`). There is
//!   deliberately no `Group::observe`: a second door onto the same mechanism
//!   is exactly the shape #838 deleted on the write side.
//! - a [`WriteIntent`], carrying the `h` row this crate owns and
//!   `WriteRouting::Explicit([host])`. The `h` tag carries the GROUP ID,
//!   never the relay, so the host is not derivable from the event and no
//!   resolver could ever compute it -- which is why group routing is minted
//!   here, from the identity the app already gave at construction, rather
//!   than spelled by the app or derived by the engine.
//!
//! Everything in this module is PURE composition over `nostr` +
//! `nmp-grammar`. It knows nothing about an engine, a signer or a receipt;
//! `nmp`'s own extension trait is what hands a minted intent to the one
//! publish door. That is what keeps `crates/nmp-nip29/Cargo.toml` free of
//! any core or mechanism edge (`scripts/check-nip29-ownership.sh`).
//!
//! This module is KIND-BLIND. It reads no kind, branches on no kind, and
//! privileges none: NIP-29 permits any kind to carry an `h` and live in a
//! group, and declaring a fixed content catalogue was the measured defect
//! #838 closed. The kinds NIP-29 itself DEFINES (9000-9022) live next door in
//! [`crate::operations`], which composes them without ever naming a group.

use std::collections::BTreeSet;

use nmp_grammar::{
    AccessContext, Binding, Demand, Durability, EventBuilder, Filter, Identity, IndexedTagName,
    SourceAuthority, WriteIntent, WritePayload, WriteRouting,
};
use nostr::{Event, RelayUrl, Tag};

/// The row NIP-29 owns: which group an event belongs to.
const CONTEXT_TAG: &str = "h";
/// Reserved, never emitted, never accepted from a caller. `previous` remains
/// unimplemented until a host-scoped, group-scoped, author-aware live-window
/// capability can mint it without caller tuples or silent truncation (#838).
const RESERVED_TIMELINE_TAG: &str = "previous";

/// A NIP-29 group: one host relay and one group id.
///
/// Constructing one contacts nothing and subscribes to nothing. The same
/// value serves every read and every write for a room's whole lifetime --
/// 29er-next's measured shape is several simultaneous observations (chat,
/// activity, reactions, membership) plus repeated writes off ONE group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    host: RelayUrl,
    id: String,
}

/// Typed refusal from group contextualization. Every variant is a caller
/// error at the door -- none of them is a relay rejection, and none of them
/// mutates, repairs or re-signs what the caller supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupContextError {
    /// An unsigned draft arrived already carrying an `h` row. The group id
    /// given at construction is the only source of that row, so a caller's
    /// own is refused whether it matches this group or not -- the refusal is
    /// about WHO OWNS the tag, not about which value it happened to hold.
    CallerSuppliedContext,
    /// An unsigned draft arrived already carrying a `previous` row.
    CallerSuppliedTimeline,
    /// A pre-signed event carries no `h` at all. Appending one would change
    /// the bytes and therefore the `EventId`, so this is a refusal rather
    /// than a repair.
    MissingContext { expected: String },
    /// A pre-signed event names a different group than the one publishing it.
    MismatchedContext { found: String, expected: String },
    /// A pre-signed event carries more than one `h` row, so which group it
    /// claims to be in has no single answer.
    AmbiguousContext { expected: String },
}

impl std::fmt::Display for GroupContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CallerSuppliedContext => write!(
                f,
                "the '{CONTEXT_TAG}' tag belongs to the group, not to the caller"
            ),
            Self::CallerSuppliedTimeline => write!(
                f,
                "the '{RESERVED_TIMELINE_TAG}' tag belongs to the group, not to the caller, \
                 and the group never mints one"
            ),
            Self::MissingContext { expected } => write!(
                f,
                "a signed event carries no '{CONTEXT_TAG}' tag, so it is not in group \
                 {expected:?}; appending one would change its event id"
            ),
            Self::MismatchedContext { found, expected } => write!(
                f,
                "a signed event names group {found:?}, but it is being published through \
                 group {expected:?}"
            ),
            Self::AmbiguousContext { expected } => write!(
                f,
                "a signed event carries more than one '{CONTEXT_TAG}' tag, so its membership \
                 of group {expected:?} has no single answer"
            ),
        }
    }
}

impl std::error::Error for GroupContextError {}

impl Group {
    /// `(host, group_id)` and nothing else. No I/O, no subscription, no
    /// active account required.
    pub fn new(host: RelayUrl, group_id: impl Into<String>) -> Self {
        Self {
            host,
            id: group_id.into(),
        }
    }

    /// The host this group lives on. Read-only: no operation anywhere takes
    /// a relay from a caller, so this is the only relay a group write can
    /// ever reach.
    pub fn host(&self) -> &RelayUrl {
        &self.host
    }

    /// The group id, i.e. the value of every `h` row this group mints or
    /// validates.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Mint the read demand for an APP-SUPPLIED selection.
    ///
    /// The group contributes exactly two things: the host pinning
    /// (`SourceAuthority::Pinned({host})` -- #107's primitive, deliberately
    /// never a directory fact) and the `#h` scoping. Which kinds live in the
    /// group is the app's to say; a fixed catalogue here is precisely what
    /// #838 removed.
    ///
    /// Hand the result to the one read door:
    /// `engine.observe(LiveQuery(group.demand(filter)), None)`.
    pub fn demand(&self, selection: Filter) -> Demand {
        let mut selection = selection;
        selection.tags.insert(
            IndexedTagName::new('h').expect("'h' is a single ASCII letter"),
            Binding::Literal(BTreeSet::from([self.id.clone()])),
        );
        Demand::new(
            selection,
            SourceAuthority::Pinned(BTreeSet::from([self.host.clone()])),
            AccessContext::Public,
        )
        .expect(
            "a singleton pinned relay set with a non-outbox source can never violate \
             Demand::new's validation rules",
        )
    }

    /// Mint the write intent for an unsigned draft: append exactly one
    /// `["h", group_id]` row and route explicitly to the host.
    ///
    /// The append happens on the BUILDER -- before the stamp/sign step -- so
    /// the context tag is inside the bytes that get signed and is covered by
    /// the id and the signature.
    pub fn write_intent(&self, builder: EventBuilder) -> Result<WriteIntent, GroupContextError> {
        Ok(self.intent(WritePayload::Event(self.contextualize(builder)?)))
    }

    /// Mint the write intent for an ALREADY-SIGNED event: validate the `h`
    /// it already carries and route explicitly to the host.
    ///
    /// Nothing is appended, nothing is re-signed, nothing is recomputed --
    /// appending would change the bytes and therefore the `EventId`, which is
    /// the whole reason an app signs first (it already published that id, or
    /// armed an observation on it). A missing, wrong or duplicated `h` is a
    /// typed refusal.
    pub fn signed_write_intent(&self, event: Event) -> Result<WriteIntent, GroupContextError> {
        self.validate_context(&event)?;
        Ok(self.intent(WritePayload::Signed(event)))
    }

    /// The validation half of [`Self::signed_write_intent`], separately
    /// callable so an app can ask whether a signed event belongs to this
    /// group without building a write out of it.
    pub fn validate_context(&self, event: &Event) -> Result<(), GroupContextError> {
        let mut values = event
            .tags
            .iter()
            .filter_map(|tag| {
                let row = tag.as_slice();
                (row.first().map(String::as_str) == Some(CONTEXT_TAG)).then(|| {
                    row.get(1)
                        .map(String::to_string)
                        .unwrap_or_else(String::new)
                })
            })
            .peekable();
        let Some(found) = values.next() else {
            return Err(GroupContextError::MissingContext {
                expected: self.id.clone(),
            });
        };
        if values.peek().is_some() {
            return Err(GroupContextError::AmbiguousContext {
                expected: self.id.clone(),
            });
        }
        if found != self.id {
            return Err(GroupContextError::MismatchedContext {
                found,
                expected: self.id.clone(),
            });
        }
        Ok(())
    }

    /// Append the group's own `h` row to a draft, refusing a draft that
    /// already claims either tag this crate owns. Every other field and tag
    /// survives verbatim, in the caller's own order.
    fn contextualize(&self, builder: EventBuilder) -> Result<EventBuilder, GroupContextError> {
        for tag in &builder.tags {
            match tag.as_slice().first().map(String::as_str) {
                Some(CONTEXT_TAG) => return Err(GroupContextError::CallerSuppliedContext),
                Some(RESERVED_TIMELINE_TAG) => {
                    return Err(GroupContextError::CallerSuppliedTimeline)
                }
                _ => {}
            }
        }
        Ok(builder
            .tag(Tag::parse([CONTEXT_TAG, &self.id]).expect("'h' is a well-formed non-empty row")))
    }

    /// The one shape a group write has. `Explicit([host])` is minted HERE,
    /// from the identity the app gave at construction: no caller supplies it
    /// and no engine derives it.
    fn intent(&self, payload: WritePayload) -> WriteIntent {
        WriteIntent {
            payload,
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![self.host.clone()]),
            // A group write says nothing about WHO is publishing: the app is
            // posting as itself, and #974's `Active` is exactly that
            // statement. NIP-29 owns the group context and the route, never
            // the author.
            identity: Identity::Active,
            correlation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventId, Keys, Kind, Timestamp, UnsignedEvent};

    fn host() -> RelayUrl {
        RelayUrl::parse("wss://groups.example.com").unwrap()
    }

    fn group() -> Group {
        Group::new(host(), "photographers")
    }

    fn rows(builder: &EventBuilder) -> Vec<Vec<String>> {
        builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    fn builder_of(intent: WriteIntent) -> EventBuilder {
        match intent.payload {
            WritePayload::Event(builder) => builder,
            _ => panic!("an unsigned draft mints an Event payload"),
        }
    }

    fn explicit_relays(intent: &WriteIntent) -> Vec<RelayUrl> {
        match &intent.routing {
            WriteRouting::Explicit(relays) => relays.clone(),
            WriteRouting::Auto => panic!("a group write is never Auto"),
        }
    }

    fn signed(tags: Vec<Tag>) -> Event {
        let keys = Keys::generate();
        UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(1_700_000_000u64),
            Kind::from(9u16),
            tags,
            "first light".to_string(),
        )
        .sign_with_keys(&keys)
        .expect("fixture keys sign cleanly")
    }

    fn timeline_tag() -> Tag {
        Tag::parse([RESERVED_TIMELINE_TAG, "deadbeef"]).expect("a two-value row is well-formed")
    }

    /// The property carried over from the deleted free-function seam this
    /// door replaces: NIP-29 owns neither the kind nor the schema of what it
    /// carries,
    /// so a complete draft survives byte-for-byte except for one appended
    /// `h` row -- in the caller's original tag order.
    #[test]
    fn draft_kind_and_schema_survive_except_for_appended_h() {
        let created_at = Timestamp::from(1_700_000_000u64);
        let draft = EventBuilder::new(Kind::from(20u16))
            .content("draft content")
            .tag(Tag::parse(["title", "sunset"]).unwrap())
            .tag(Tag::parse(["imeta", "url https://cdn.example/sunset.jpg"]).unwrap())
            .created_at(created_at);

        let intent = group().write_intent(draft).unwrap();
        assert_eq!(explicit_relays(&intent), vec![host()]);
        let built = builder_of(intent);
        assert_eq!(built.kind, Kind::from(20u16));
        assert_eq!(built.content, "draft content");
        assert_eq!(built.created_at, Some(created_at));
        assert_eq!(
            rows(&built),
            vec![
                vec!["title".to_string(), "sunset".to_string()],
                vec![
                    "imeta".to_string(),
                    "url https://cdn.example/sunset.jpg".to_string()
                ],
                vec!["h".to_string(), "photographers".to_string()],
            ]
        );
    }

    /// The other carried-over property: no `previous` row is ever
    /// synthesized, on any path, for any draft.
    #[test]
    fn publication_never_synthesizes_previous() {
        let intent = group()
            .write_intent(EventBuilder::new(Kind::from(30023u16)))
            .unwrap();
        assert_eq!(
            rows(&builder_of(intent)),
            vec![vec!["h".to_string(), "photographers".to_string()]]
        );
    }

    #[test]
    fn a_c7_q_reply_survives_without_nip29_interpreting_it() {
        let parent = EventId::from_slice(&[7; 32]).unwrap();
        let draft = EventBuilder::new(Kind::from(9u16))
            .content("reply")
            .tag(Tag::parse(["q", &parent.to_hex(), "wss://chat.example.com"]).unwrap());
        let built = builder_of(group().write_intent(draft).unwrap());
        assert_eq!(rows(&built)[0][0], "q");
        assert_eq!(
            rows(&built).last().unwrap(),
            &vec!["h".to_string(), "photographers".to_string()]
        );
    }

    #[test]
    fn a_caller_supplied_h_is_refused_whether_or_not_it_matches() {
        for value in ["photographers", "darkroom"] {
            let draft = EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", value]).unwrap());
            assert_eq!(
                group().write_intent(draft).err(),
                Some(GroupContextError::CallerSuppliedContext)
            );
        }
    }

    #[test]
    fn a_caller_supplied_previous_is_refused() {
        let draft = EventBuilder::new(Kind::from(9u16)).tag(timeline_tag());
        assert_eq!(
            group().write_intent(draft).err(),
            Some(GroupContextError::CallerSuppliedTimeline)
        );
    }

    #[test]
    fn a_correctly_contextualized_signed_event_is_routed_verbatim() {
        let event = signed(vec![Tag::parse(["h", "photographers"]).unwrap()]);
        let intent = group().signed_write_intent(event.clone()).unwrap();
        assert_eq!(explicit_relays(&intent), vec![host()]);
        match intent.payload {
            WritePayload::Signed(out) => assert_eq!(out, event),
            _ => panic!("a signed event mints a Signed payload"),
        }
    }

    #[test]
    fn a_signed_event_with_no_context_is_refused_not_repaired() {
        let event = signed(Vec::new());
        assert_eq!(
            group().signed_write_intent(event.clone()).err(),
            Some(GroupContextError::MissingContext {
                expected: "photographers".to_string()
            })
        );
        assert!(!event
            .tags
            .iter()
            .any(|t| t.as_slice().first().map(String::as_str) == Some("h")));
    }

    #[test]
    fn a_signed_event_naming_another_group_names_both_in_its_refusal() {
        let event = signed(vec![Tag::parse(["h", "darkroom"]).unwrap()]);
        let error = group()
            .signed_write_intent(event)
            .err()
            .expect("another group's h is a refusal");
        assert_eq!(
            error,
            GroupContextError::MismatchedContext {
                found: "darkroom".to_string(),
                expected: "photographers".to_string(),
            }
        );
        let said = error.to_string();
        assert!(
            said.contains("darkroom") && said.contains("photographers"),
            "{said}"
        );
    }

    #[test]
    fn a_signed_event_with_two_context_rows_is_ambiguous() {
        let event = signed(vec![
            Tag::parse(["h", "photographers"]).unwrap(),
            Tag::parse(["h", "darkroom"]).unwrap(),
        ]);
        assert_eq!(
            group().signed_write_intent(event).err(),
            Some(GroupContextError::AmbiguousContext {
                expected: "photographers".to_string()
            })
        );
    }

    #[test]
    fn the_demand_pins_the_host_and_scopes_the_app_supplied_selection() {
        let selection = Filter {
            kinds: Some(BTreeSet::from([9u16, 30315u16])),
            ..Filter::default()
        };
        let demand = group().demand(selection);
        assert_eq!(demand.selection.kinds, Some(BTreeSet::from([9, 30315])));
        assert_eq!(
            demand.source,
            SourceAuthority::Pinned(BTreeSet::from([host()]))
        );
        assert_eq!(demand.access, AccessContext::Public);
        assert_eq!(
            demand
                .selection
                .tags
                .get(&IndexedTagName::new('h').unwrap()),
            Some(&Binding::Literal(BTreeSet::from([
                "photographers".to_string()
            ])))
        );
    }

    /// Two groups on the same host stay separated by their `#h` scoping, and
    /// two groups on different hosts never share a route.
    #[test]
    fn two_groups_never_bleed_into_each_other() {
        let darkroom = Group::new(
            RelayUrl::parse("wss://darkroom.example.com").unwrap(),
            "darkroom",
        );
        let intent = darkroom
            .write_intent(EventBuilder::new(Kind::from(9u16)))
            .unwrap();
        assert_eq!(
            explicit_relays(&intent),
            vec![RelayUrl::parse("wss://darkroom.example.com").unwrap()]
        );
        assert_eq!(
            rows(&builder_of(intent)),
            vec![vec!["h".to_string(), "darkroom".to_string()]]
        );
    }
}
