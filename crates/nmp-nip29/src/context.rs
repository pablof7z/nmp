//! The `h` row: which group an event belongs to, and what may be said about
//! it (#977, #1033).
//!
//! This module owns NIP-29's group-context SEMANTICS and nothing else -- it
//! contextualizes an unsigned draft, validates an already-signed event, and
//! scopes one host's read branch to one group id. It holds no relay set, no
//! route, no engine, no signer and no intent: the app-facing door that
//! retains a scope and mints an opaque write lives in the `nmp` facade
//! (`nmp::nip29`), which is what keeps this crate's dependencies at exactly
//! `nostr` + `nmp-grammar` (`scripts/check-nip29-ownership.sh`).
//!
//! Everything here is KIND-BLIND. It reads no kind, branches on no kind, and
//! privileges none: NIP-29 permits any kind to carry an `h` and live in a
//! group, and declaring a fixed content catalogue was the measured defect
//! #838 closed. The kinds NIP-29 itself DEFINES (9000-9022) live in
//! [`crate::operations`]; the kinds it defines for DESCRIBING a group
//! (39000/39001/39002) live in [`crate::discovery`].

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, EventBuilder, Filter, IndexedTagName};
use nostr::{Event, RelayUrl, Tag};

/// The row NIP-29 owns: which group an event belongs to.
const CONTEXT_TAG: char = 'h';
/// Reserved, never emitted, never accepted from a caller. `previous` remains
/// unimplemented until a host-scoped, group-scoped, author-aware live-window
/// capability can mint it without caller tuples or silent truncation (#838).
const RESERVED_TIMELINE_TAG: &str = "previous";

/// Typed refusal from group contextualization. Every variant is a caller
/// error at the door -- none of them is a relay rejection, and none of them
/// mutates, repairs or re-signs what the caller supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupContextError {
    /// An unsigned draft arrived already carrying an `h` row. The group id
    /// retained by the group is the only source of that row, so a caller's
    /// own is refused whether it matches this group or not -- the refusal is
    /// about WHO OWNS the tag, not about which value it happened to hold.
    CallerSuppliedContext,
    /// A read selection arrived already constraining `#h`. Same ownership
    /// rule from the read side: the retained group id is the sole semantic
    /// source of `h`, so a caller's own constraint is refused rather than
    /// silently overwritten (which would answer a question nobody asked).
    CallerSuppliedContextConstraint,
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
            Self::CallerSuppliedContextConstraint => write!(
                f,
                "the '#{CONTEXT_TAG}' constraint belongs to the group, not to the caller's \
                 selection"
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

/// One host's complete read branch for one group: the app's OWN selection,
/// scoped by `#h = group_id` and pinned to exactly that host.
///
/// Which kinds live in a group is the app's to say; a fixed catalogue here is
/// precisely what #838 removed. An app selection that already constrains `#h`
/// is REFUSED rather than overwritten.
pub fn group_demand_at(
    host: &RelayUrl,
    group_id: &str,
    selection: Filter,
) -> Result<Demand, GroupContextError> {
    let mut selection = selection;
    if selection.tags.contains_key(&context_tag()) {
        return Err(GroupContextError::CallerSuppliedContextConstraint);
    }
    selection.tags.insert(
        context_tag(),
        Binding::Literal(BTreeSet::from([group_id.to_string()])),
    );
    Ok(crate::discovery::pinned_public_at(host, selection))
}

/// Append the group's own `h` row to a draft, refusing a draft that already
/// claims either tag this crate owns. Every other field and tag survives
/// verbatim, in the caller's own order.
///
/// The append happens on the BUILDER -- before the stamp/sign step -- so the
/// context tag is inside the bytes that get signed and is covered by the id
/// and the signature.
pub fn contextualize(
    group_id: &str,
    builder: EventBuilder,
) -> Result<EventBuilder, GroupContextError> {
    for tag in &builder.tags {
        match tag.as_slice().first().map(String::as_str) {
            Some(name) if name == CONTEXT_TAG.to_string() => {
                return Err(GroupContextError::CallerSuppliedContext)
            }
            Some(RESERVED_TIMELINE_TAG) => return Err(GroupContextError::CallerSuppliedTimeline),
            _ => {}
        }
    }
    Ok(builder.tag(
        Tag::parse([CONTEXT_TAG.to_string().as_str(), group_id])
            .expect("'h' is a well-formed non-empty row"),
    ))
}

/// Validate the `h` an ALREADY-SIGNED event carries. Nothing is appended,
/// nothing is re-signed, nothing is recomputed -- appending would change the
/// bytes and therefore the `EventId`, which is the whole reason an app signs
/// first. A missing, wrong or duplicated `h` is a typed refusal.
pub fn validate_context(group_id: &str, event: &Event) -> Result<(), GroupContextError> {
    let context = CONTEXT_TAG.to_string();
    let mut values = event
        .tags
        .iter()
        .filter_map(|tag| {
            let row = tag.as_slice();
            (row.first() == Some(&context)).then(|| {
                row.get(1)
                    .map(String::to_string)
                    .unwrap_or_else(String::new)
            })
        })
        .peekable();
    let Some(found) = values.next() else {
        return Err(GroupContextError::MissingContext {
            expected: group_id.to_string(),
        });
    };
    if values.peek().is_some() {
        return Err(GroupContextError::AmbiguousContext {
            expected: group_id.to_string(),
        });
    }
    if found != group_id {
        return Err(GroupContextError::MismatchedContext {
            found,
            expected: group_id.to_string(),
        });
    }
    Ok(())
}

fn context_tag() -> IndexedTagName {
    IndexedTagName::new(CONTEXT_TAG).expect("'h' is a single ASCII letter")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{AccessContext, SourceAuthority};
    use nostr::{EventId, Keys, Kind, Timestamp, UnsignedEvent};

    const GROUP: &str = "photographers";

    fn host() -> RelayUrl {
        RelayUrl::parse("wss://groups.example.com").expect("a well-formed host")
    }

    fn rows(builder: &EventBuilder) -> Vec<Vec<String>> {
        builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
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

    /// NIP-29 owns neither the kind nor the schema of what it carries, so a
    /// complete draft survives byte-for-byte except for one appended `h` row
    /// -- in the caller's original tag order.
    #[test]
    fn draft_kind_and_schema_survive_except_for_appended_h() {
        let created_at = Timestamp::from(1_700_000_000u64);
        let draft = EventBuilder::new(Kind::from(20u16))
            .content("draft content")
            .tag(Tag::parse(["title", "sunset"]).unwrap())
            .tag(Tag::parse(["imeta", "url https://cdn.example/sunset.jpg"]).unwrap())
            .created_at(created_at);

        let built = contextualize(GROUP, draft).expect("a plain draft is contextualizable");
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
                vec!["h".to_string(), GROUP.to_string()],
            ]
        );
    }

    /// No `previous` row is ever synthesized, on any path, for any draft.
    #[test]
    fn publication_never_synthesizes_previous() {
        let built = contextualize(GROUP, EventBuilder::new(Kind::from(30023u16))).unwrap();
        assert_eq!(
            rows(&built),
            vec![vec!["h".to_string(), GROUP.to_string()]]
        );
    }

    #[test]
    fn a_c7_q_reply_survives_without_nip29_interpreting_it() {
        let parent = EventId::from_slice(&[7; 32]).unwrap();
        let draft = EventBuilder::new(Kind::from(9u16))
            .content("reply")
            .tag(Tag::parse(["q", &parent.to_hex(), "wss://chat.example.com"]).unwrap());
        let built = contextualize(GROUP, draft).unwrap();
        assert_eq!(rows(&built)[0][0], "q");
        assert_eq!(
            rows(&built).last().unwrap(),
            &vec!["h".to_string(), GROUP.to_string()]
        );
    }

    #[test]
    fn a_caller_supplied_h_is_refused_whether_or_not_it_matches() {
        for value in [GROUP, "darkroom"] {
            let draft = EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", value]).unwrap());
            assert_eq!(
                contextualize(GROUP, draft).err(),
                Some(GroupContextError::CallerSuppliedContext)
            );
        }
    }

    #[test]
    fn a_caller_supplied_previous_is_refused() {
        let draft = EventBuilder::new(Kind::from(9u16)).tag(timeline_tag());
        assert_eq!(
            contextualize(GROUP, draft).err(),
            Some(GroupContextError::CallerSuppliedTimeline)
        );
    }

    #[test]
    fn a_signed_event_with_no_context_is_refused_not_repaired() {
        let event = signed(Vec::new());
        assert_eq!(
            validate_context(GROUP, &event).err(),
            Some(GroupContextError::MissingContext {
                expected: GROUP.to_string()
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
        let error = validate_context(GROUP, &event)
            .err()
            .expect("another group's h is a refusal");
        assert_eq!(
            error,
            GroupContextError::MismatchedContext {
                found: "darkroom".to_string(),
                expected: GROUP.to_string(),
            }
        );
        let said = error.to_string();
        assert!(
            said.contains("darkroom") && said.contains(GROUP),
            "{said}"
        );
    }

    #[test]
    fn a_signed_event_with_two_context_rows_is_ambiguous() {
        let event = signed(vec![
            Tag::parse(["h", GROUP]).unwrap(),
            Tag::parse(["h", "darkroom"]).unwrap(),
        ]);
        assert_eq!(
            validate_context(GROUP, &event).err(),
            Some(GroupContextError::AmbiguousContext {
                expected: GROUP.to_string()
            })
        );
    }

    #[test]
    fn a_correctly_contextualized_signed_event_validates() {
        let event = signed(vec![Tag::parse(["h", GROUP]).unwrap()]);
        assert_eq!(validate_context(GROUP, &event), Ok(()));
    }

    #[test]
    fn a_read_branch_pins_the_host_and_scopes_the_app_supplied_selection() {
        let selection = Filter {
            kinds: Some(BTreeSet::from([9u16, 30315u16])),
            ..Filter::default()
        };
        let demand = group_demand_at(&host(), GROUP, selection).expect("a plain selection scopes");
        assert_eq!(demand.selection.kinds, Some(BTreeSet::from([9, 30315])));
        assert_eq!(
            demand.source,
            SourceAuthority::Pinned(BTreeSet::from([host()]))
        );
        assert_eq!(demand.access, AccessContext::Public);
        assert_eq!(
            demand.selection.tags.get(&context_tag()),
            Some(&Binding::Literal(BTreeSet::from([GROUP.to_string()])))
        );
    }

    /// The retained group id is the SOLE semantic source of `h`, so a
    /// selection that already constrains it is refused, never overwritten.
    #[test]
    fn a_read_selection_that_already_constrains_h_is_refused() {
        let mut selection = Filter::default();
        selection.tags.insert(
            context_tag(),
            Binding::Literal(BTreeSet::from(["darkroom".to_string()])),
        );
        assert_eq!(
            group_demand_at(&host(), GROUP, selection).err(),
            Some(GroupContextError::CallerSuppliedContextConstraint)
        );
    }
}
