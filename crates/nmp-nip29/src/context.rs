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

use crate::discovery::JOIN_KEY_TAG;

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
    /// A group-content read selection named one of NIP-29's own relay-signed
    /// group records (39000/39001/39002).
    ///
    /// Those records key themselves by `d`, never by `h`, so constraining
    /// them by this group's `h` row builds a filter no such event can match:
    /// the read would return nothing, forever, and an app could not tell that
    /// apart from a group whose relay published no roster (#1245). A door
    /// that returns nothing forever is worse than one that says no, so this
    /// says no -- the same ownership refusal
    /// [`Self::CallerSuppliedContextConstraint`] already makes, on the other
    /// axis.
    ///
    /// It is not a kind catalogue and it privileges no kind: which kinds live
    /// IN a group stays entirely the app's to choose. These three are not in
    /// the group, they are ABOUT it, and they are read through the group's
    /// own records door instead.
    RecordsAreNotContextScoped { kinds: BTreeSet<u16> },
    /// A write was composed for no group at all (#1281). An event with no
    /// `h` row is not in a group, so there is nothing for the door to
    /// contextualize and no honest route to mint. The refusal is at
    /// construction, which is what keeps every method below infallible with
    /// respect to the group set -- the same shape
    /// [`crate::GroupContextError`]'s relay-side counterpart
    /// (`nmp::nip29::RelayScopeError::EmptyRelaySet`) already has.
    NoGroupNamed,
    /// A pre-signed event carries no `h` at all. Appending one would change
    /// the bytes and therefore the `EventId`, so this is a refusal rather
    /// than a repair.
    MissingContext { expected: BTreeSet<String> },
    /// A pre-signed event names a different SET of groups than the one
    /// publishing it -- too few, too many, or the wrong ones. One group is
    /// the one-element case: an event carrying `["h", "darkroom"]` published
    /// through `photographers` reports `found: {darkroom}`,
    /// `expected: {photographers}`, and an event carrying a second `h` row
    /// beside the right one reports both in `found`.
    MismatchedContext {
        found: BTreeSet<String>,
        expected: BTreeSet<String>,
    },
    /// A pre-signed event names the same group in more than one `h` row.
    ///
    /// The set of groups it claims is right, so this is not a
    /// [`Self::MismatchedContext`]; what is wrong is that the rows are not
    /// the rows this door would ever mint. Refusing keeps the pre-signed
    /// path exactly as strict as the unsigned one, which appends each id
    /// once and only once.
    RepeatedContext { repeated: BTreeSet<String> },
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
            Self::RecordsAreNotContextScoped { kinds } => {
                let kinds: Vec<String> = kinds.iter().map(u16::to_string).collect();
                write!(
                    f,
                    "kinds {} are NIP-29's own relay-signed group records: they key themselves \
                     by '{JOIN_KEY_TAG}', never by '{CONTEXT_TAG}', so no such event could ever \
                     match a group-content read -- read them through the group's records door",
                    kinds.join(", ")
                )
            }
            Self::NoGroupNamed => write!(
                f,
                "a group write must name at least one group: an event with no \
                 '{CONTEXT_TAG}' row is not in a group at all"
            ),
            Self::MissingContext { expected } => write!(
                f,
                "a signed event carries no '{CONTEXT_TAG}' tag, so it is not in {}; \
                 appending one would change its event id",
                named(expected)
            ),
            Self::MismatchedContext { found, expected } => write!(
                f,
                "a signed event names {}, but it is being published through {}",
                named(found),
                named(expected)
            ),
            Self::RepeatedContext { repeated } => write!(
                f,
                "a signed event names {} in more than one '{CONTEXT_TAG}' row",
                named(repeated)
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
///
/// The one selection this door refuses on its content is NIP-29's own
/// relay-signed records (39000/39001/39002), because they are `d`-keyed and
/// an `h`-scoped filter over them matches nothing at all (#1245). That is a
/// refusal about which AXIS keys the event, not a catalogue of the group's
/// kinds: every other kind, defined by NIP-29, by another NIP, or by nothing,
/// passes through unread.
pub fn group_demand_at(
    host: &RelayUrl,
    group_id: &str,
    selection: Filter,
) -> Result<Demand, GroupContextError> {
    let mut selection = selection;
    if selection.tags.contains_key(&context_tag()) {
        return Err(GroupContextError::CallerSuppliedContextConstraint);
    }
    let records: BTreeSet<u16> = selection
        .kinds
        .iter()
        .flatten()
        .copied()
        .filter(|selected| crate::records::GroupRecord::of_kind(*selected).is_some())
        .collect();
    if !records.is_empty() {
        return Err(GroupContextError::RecordsAreNotContextScoped { kinds: records });
    }
    selection.tags.insert(
        context_tag(),
        Binding::Literal(BTreeSet::from([group_id.to_string()])),
    );
    Ok(crate::discovery::pinned_public_at(host, selection))
}

/// Append one `h` row per group in `group_ids` to a draft, refusing a draft
/// that already claims either tag this crate owns. Every other field and tag
/// survives verbatim, in the caller's own order; the appended rows follow the
/// set's own canonical order, so two callers naming the same groups in
/// different orders compose the identical bytes.
///
/// One group is the ONE-ELEMENT case and has no separate path: a write that
/// belongs to several groups at once -- a kind:30315 session status carrying
/// one `h` per room the session occupies (#1281) -- is the same call with a
/// larger set. There is no arity anywhere below this line.
///
/// The append happens on the BUILDER -- before the stamp/sign step -- so the
/// context tags are inside the bytes that get signed and are covered by the
/// id and the signature.
///
/// `group_ids` is proved nonempty by whoever formed it
/// ([`GroupContextError::NoGroupNamed`] is decided at that construction, not
/// here), exactly as [`crate::group_records_at`]'s record selection is.
pub fn contextualize(
    group_ids: &BTreeSet<String>,
    builder: EventBuilder,
) -> Result<EventBuilder, GroupContextError> {
    debug_assert!(
        !group_ids.is_empty(),
        "the door proves its group set is nonempty before contextualizing"
    );
    for tag in &builder.tags {
        match tag.as_slice().first().map(String::as_str) {
            Some(name) if name == CONTEXT_TAG.to_string() => {
                return Err(GroupContextError::CallerSuppliedContext)
            }
            Some(RESERVED_TIMELINE_TAG) => return Err(GroupContextError::CallerSuppliedTimeline),
            _ => {}
        }
    }
    Ok(group_ids.iter().fold(builder, |builder, group_id| {
        builder.tag(
            Tag::parse([CONTEXT_TAG.to_string().as_str(), group_id.as_str()])
                .expect("'h' is a well-formed non-empty row"),
        )
    }))
}

/// Validate the `h` rows an ALREADY-SIGNED event carries. Nothing is
/// appended, nothing is re-signed, nothing is recomputed -- appending would
/// change the bytes and therefore the `EventId`, which is the whole reason an
/// app signs first.
///
/// The event's `h` rows must name EXACTLY `group_ids`, each once. Naming none
/// is [`GroupContextError::MissingContext`], naming a different set is
/// [`GroupContextError::MismatchedContext`], and naming the right set with a
/// row repeated is [`GroupContextError::RepeatedContext`] -- which is the
/// refusal that keeps this path exactly as strict as
/// [`contextualize`], whose rows a set can never repeat.
pub fn validate_context(
    group_ids: &BTreeSet<String>,
    event: &Event,
) -> Result<(), GroupContextError> {
    debug_assert!(
        !group_ids.is_empty(),
        "the door proves its group set is nonempty before validating"
    );
    let context = CONTEXT_TAG.to_string();
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut repeated: BTreeSet<String> = BTreeSet::new();
    let mut rows = 0usize;
    for tag in event.tags.iter() {
        let row = tag.as_slice();
        if row.first() != Some(&context) {
            continue;
        }
        rows += 1;
        let value = row
            .get(1)
            .map(String::to_string)
            .unwrap_or_else(String::new);
        if !found.insert(value.clone()) {
            repeated.insert(value);
        }
    }
    if rows == 0 {
        return Err(GroupContextError::MissingContext {
            expected: group_ids.clone(),
        });
    }
    if &found != group_ids {
        return Err(GroupContextError::MismatchedContext {
            found,
            expected: group_ids.clone(),
        });
    }
    if !repeated.is_empty() {
        return Err(GroupContextError::RepeatedContext { repeated });
    }
    Ok(())
}

fn context_tag() -> IndexedTagName {
    IndexedTagName::new(CONTEXT_TAG).expect("'h' is a single ASCII letter")
}

/// One group named as `group "x"`, several as `groups "x", "y"` -- so a
/// refusal message reads as a sentence at either arity without a caller
/// having to know which one it got.
fn named(group_ids: &BTreeSet<String>) -> String {
    let listed: Vec<String> = group_ids.iter().map(|id| format!("{id:?}")).collect();
    match listed.len() {
        1 => format!("group {}", listed[0]),
        _ => format!("groups {}", listed.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{AccessContext, SourceAuthority};
    use nostr::{EventId, Keys, Kind, Timestamp, UnsignedEvent};

    const GROUP: &str = "photographers";

    /// The one-element case, spelled once so every single-group test below
    /// reads exactly as it did when the door took a bare `&str`.
    fn one(group_id: &str) -> BTreeSet<String> {
        BTreeSet::from([group_id.to_string()])
    }

    fn many<'a>(group_ids: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
        group_ids.into_iter().map(str::to_string).collect()
    }

    fn context_rows(builder: &EventBuilder) -> Vec<String> {
        builder
            .tags
            .iter()
            .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
            .map(|tag| tag.as_slice()[1].clone())
            .collect()
    }

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

    /// PROTOCOL-KINDBLINDNESS-001 (write half) and PROTOCOL-KINDBLINDNESS-004
    /// (direct half): a table of kinds NIP-29 itself defines (9021), kinds
    /// other NIPs define (7, 30315), and a kind nothing defines at all
    /// (44815) all take the IDENTICAL contextualization path -- the same
    /// single appended `h` row, no refusal, no branch, no special case.
    /// Passing on 44815 alone would not prove kind-blindness (an
    /// allow-list could pass that too); the point is that every row in
    /// this table is handled by the exact same code, which
    /// `check-nip29-kind-blindness.sh` additionally proves structurally by
    /// showing that code never reads `.kind` at all.
    #[test]
    fn contextualize_takes_the_identical_path_for_every_kind_familiar_or_not() {
        for kind in [9021u16, 7, 30315, 44815, 20, 1] {
            let built = contextualize(
                &one(GROUP),
                EventBuilder::new(Kind::from(kind)).content("whatever this is"),
            )
            .unwrap_or_else(|error| panic!("kind {kind} must contextualize cleanly: {error}"));
            assert_eq!(
                built.kind,
                Kind::from(kind),
                "kind {kind} must survive unchanged"
            );
            assert_eq!(
                rows(&built),
                vec![vec!["h".to_string(), GROUP.to_string()]],
                "kind {kind} must get exactly the one appended h row, nothing else"
            );
        }
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

        let built = contextualize(&one(GROUP), draft).expect("a plain draft is contextualizable");
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

    /// PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-005: the unsigned door never
    /// INVENTS a `previous` row of its own, on any path, for any draft. This
    /// is a claim about the unsigned semantic-composition door only -- it
    /// does not claim there is no way anywhere in the repository for an
    /// already-signed event to carry a tag shaped like `previous`; the
    /// pre-signed path (`validate_context`) validates only `h` and preserves
    /// whatever the caller already signed verbatim.
    #[test]
    fn the_unsigned_door_never_invents_a_previous_tag() {
        let built = contextualize(&one(GROUP), EventBuilder::new(Kind::from(30023u16))).unwrap();
        assert_eq!(rows(&built), vec![vec!["h".to_string(), GROUP.to_string()]]);
    }

    #[test]
    fn a_c7_q_reply_survives_without_nip29_interpreting_it() {
        let parent = EventId::from_slice(&[7; 32]).unwrap();
        let draft = EventBuilder::new(Kind::from(9u16))
            .content("reply")
            .tag(Tag::parse(["q", &parent.to_hex(), "wss://chat.example.com"]).unwrap());
        let built = contextualize(&one(GROUP), draft).unwrap();
        assert_eq!(rows(&built)[0][0], "q");
        assert_eq!(
            rows(&built).last().unwrap(),
            &vec!["h".to_string(), GROUP.to_string()]
        );
    }

    /// PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-001: an unsigned draft already
    /// carrying THIS group's own `h` is refused before signing or routing --
    /// the refusal is about who owns the tag, not about which value it holds.
    #[test]
    fn caller_supplied_own_h_is_refused_before_signing_or_routing() {
        let draft = EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", GROUP]).unwrap());
        assert_eq!(
            contextualize(&one(GROUP), draft).err(),
            Some(GroupContextError::CallerSuppliedContext)
        );
    }

    /// PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-002: the same refusal, the same
    /// error variant, for a draft naming ANOTHER group's `h` -- proving the
    /// refusal never depends on whether the caller's value happens to match.
    #[test]
    fn caller_supplied_other_group_h_is_refused_the_same_way() {
        let draft = EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", "darkroom"]).unwrap());
        assert_eq!(
            contextualize(&one(GROUP), draft).err(),
            Some(GroupContextError::CallerSuppliedContext)
        );
    }

    /// PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-003.
    #[test]
    fn a_caller_supplied_previous_is_refused() {
        let draft = EventBuilder::new(Kind::from(9u16)).tag(timeline_tag());
        assert_eq!(
            contextualize(&one(GROUP), draft).err(),
            Some(GroupContextError::CallerSuppliedTimeline)
        );
    }

    /// PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-004: a draft carrying BOTH an `h`
    /// and a `previous` row is refused on whichever tag the caller wrote
    /// FIRST, never silently trimmed down to one. This is a genuine
    /// precedence claim, not a fixed "h always wins" shortcut: reversing the
    /// caller's own tag order reverses which typed error comes back.
    #[test]
    fn combined_h_and_previous_is_refused_deterministically_on_whichever_tag_came_first() {
        let h_first = EventBuilder::new(Kind::from(9u16))
            .tag(Tag::parse(["h", GROUP]).unwrap())
            .tag(timeline_tag());
        assert_eq!(
            contextualize(&one(GROUP), h_first).err(),
            Some(GroupContextError::CallerSuppliedContext),
            "h was the caller's first tag, so the refusal names h, not previous"
        );

        let previous_first = EventBuilder::new(Kind::from(9u16))
            .tag(timeline_tag())
            .tag(Tag::parse(["h", GROUP]).unwrap());
        assert_eq!(
            contextualize(&one(GROUP), previous_first).err(),
            Some(GroupContextError::CallerSuppliedTimeline),
            "previous was the caller's first tag, so the refusal names previous, not h -- \
             precedence follows the caller's own tag order, not a fixed check order"
        );
    }

    /// PROTOCOL-CONTEXTTAGISSIGNED-001: whatever eventually signs this draft
    /// receives a builder that already carries exactly one `h` row -- the
    /// context tag is appended before the builder is handed onward for
    /// signing, never after.
    #[test]
    fn the_builder_handed_onward_for_signing_already_carries_exactly_one_h_row() {
        let built = contextualize(
            &one(GROUP),
            EventBuilder::new(Kind::from(9u16)).content("first light"),
        )
        .unwrap();
        let h_rows: Vec<&Tag> = built
            .tags
            .iter()
            .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
            .collect();
        assert_eq!(h_rows.len(), 1, "exactly one h row: {:?}", built.tags);
        assert_eq!(h_rows[0].as_slice(), &["h".to_string(), GROUP.to_string()]);
    }

    /// PROTOCOL-CONTEXTTAGISSIGNED-002: the h row is inside the bytes that
    /// get signed, so it is covered by both the id and the signature --
    /// changing it after the fact must invalidate them.
    #[test]
    fn the_delivered_event_s_id_and_signature_cover_the_h_tag() {
        let built = contextualize(
            &one(GROUP),
            EventBuilder::new(Kind::from(9u16)).content("first light"),
        )
        .unwrap();
        let keys = Keys::generate();
        let event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(1_700_000_000u64),
            built.kind,
            built.tags.clone(),
            built.content.clone(),
        )
        .sign_with_keys(&keys)
        .expect("fixture keys sign cleanly");

        assert!(
            event.verify().is_ok(),
            "the signature and id must verify over the exact delivered bytes, h row included"
        );

        let mut tampered = event.clone();
        let without_h: Vec<Tag> = event
            .tags
            .iter()
            .filter(|tag| tag.as_slice().first().map(String::as_str) != Some("h"))
            .cloned()
            .collect();
        assert_ne!(
            without_h.len(),
            event.tags.len(),
            "NOTHING TO OBSERVE -- the delivered event carries no h row to remove"
        );
        tampered.tags = without_h.into_iter().collect();
        assert!(
            tampered.verify().is_err(),
            "removing the h row must invalidate the event's own id, proving h was inside \
             the signed bytes"
        );
    }

    /// PROTOCOL-PRESIGNEDPUBLICATION-003.
    #[test]
    fn a_signed_event_with_no_context_is_refused_not_repaired() {
        let event = signed(Vec::new());
        assert_eq!(
            validate_context(&one(GROUP), &event).err(),
            Some(GroupContextError::MissingContext {
                expected: one(GROUP)
            })
        );
        assert!(!event
            .tags
            .iter()
            .any(|t| t.as_slice().first().map(String::as_str) == Some("h")));
    }

    /// PROTOCOL-PRESIGNEDPUBLICATION-004.
    #[test]
    fn a_signed_event_naming_another_group_names_both_in_its_refusal() {
        let event = signed(vec![Tag::parse(["h", "darkroom"]).unwrap()]);
        let error =
            validate_context(&one(GROUP), &event).expect_err("another group's h is a refusal");
        assert_eq!(
            error,
            GroupContextError::MismatchedContext {
                found: one("darkroom"),
                expected: one(GROUP),
            }
        );
        let said = error.to_string();
        assert!(said.contains("darkroom") && said.contains(GROUP), "{said}");
    }

    /// PROTOCOL-PRESIGNEDPUBLICATION-005: a signed event claiming a group
    /// this door was not asked for is refused, and the refusal NAMES the
    /// whole set it found beside the whole set expected -- so a caller can
    /// see which room leaked in rather than only that something did.
    #[test]
    fn a_signed_event_naming_a_group_the_door_was_not_asked_for_is_refused() {
        let event = signed(vec![
            Tag::parse(["h", GROUP]).unwrap(),
            Tag::parse(["h", "darkroom"]).unwrap(),
        ]);
        assert_eq!(
            validate_context(&one(GROUP), &event).err(),
            Some(GroupContextError::MismatchedContext {
                found: many([GROUP, "darkroom"]),
                expected: one(GROUP),
            })
        );
    }

    #[test]
    fn a_correctly_contextualized_signed_event_validates() {
        let event = signed(vec![Tag::parse(["h", GROUP]).unwrap()]);
        assert_eq!(validate_context(&one(GROUP), &event), Ok(()));
    }

    /// #1281, the write with no door. A kind:30315 session status is
    /// addressable at `(author, d=status)` and carries one `h` per room the
    /// session occupies, so publishing it once per room would make each copy
    /// REPLACE the last -- several `h` rows on one event is the only correct
    /// shape, not a convenience. One call composes all of them.
    #[test]
    fn a_draft_for_several_groups_carries_one_h_row_per_group() {
        let built = contextualize(
            &many(["darkroom", GROUP, "studio"]),
            EventBuilder::new(Kind::from(30315u16)).tag(Tag::parse(["d", "status"]).unwrap()),
        )
        .expect("a plain draft contextualizes for several groups");
        assert_eq!(
            context_rows(&built),
            vec![
                "darkroom".to_string(),
                GROUP.to_string(),
                "studio".to_string()
            ],
            "one h row per group, in the set's own canonical order"
        );
        assert_eq!(
            rows(&built)[0],
            vec!["d".to_string(), "status".to_string()],
            "the app's own addressable coordinate survives ahead of the appended rows"
        );
    }

    /// The set is canonical, so the composed BYTES are: two callers naming
    /// the same rooms in different orders, or naming one twice, compose the
    /// identical event. An `h` row can therefore never be repeated on the
    /// unsigned path -- which is what makes
    /// [`GroupContextError::RepeatedContext`] a claim about pre-signed bytes
    /// alone.
    #[test]
    fn the_composed_rows_do_not_depend_on_the_callers_own_order_or_repetition() {
        let forwards = contextualize(&many(["a", "b", "c"]), EventBuilder::new(Kind::from(9u16)))
            .expect("three rooms contextualize");
        let backwards = contextualize(
            &many(["c", "b", "a", "b"]),
            EventBuilder::new(Kind::from(9u16)),
        )
        .expect("the same three rooms, differently spelled, contextualize");
        assert_eq!(context_rows(&forwards), context_rows(&backwards));
        assert_eq!(context_rows(&forwards), vec!["a", "b", "c"]);
    }

    /// The ownership refusals do not weaken at the larger arity: a caller's
    /// own `h` is still refused whichever of the named rooms it happens to
    /// name, and a `previous` row still is too.
    #[test]
    fn a_caller_supplied_row_is_refused_at_the_several_group_arity_too() {
        let groups = many([GROUP, "darkroom"]);
        assert_eq!(
            contextualize(
                &groups,
                EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", GROUP]).unwrap())
            )
            .err(),
            Some(GroupContextError::CallerSuppliedContext)
        );
        assert_eq!(
            contextualize(
                &groups,
                EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", "elsewhere"]).unwrap())
            )
            .err(),
            Some(GroupContextError::CallerSuppliedContext),
            "the refusal is about who owns the row, not about which value it held"
        );
        assert_eq!(
            contextualize(
                &groups,
                EventBuilder::new(Kind::from(9u16)).tag(timeline_tag())
            )
            .err(),
            Some(GroupContextError::CallerSuppliedTimeline)
        );
    }

    /// The pre-signed half of #1281: a signed event's `h` rows must name
    /// EXACTLY the rooms it is being published into. Too few is as wrong as
    /// too many -- a status that dropped a room silently stops rendering
    /// there, which is the failure this validation exists to make loud.
    #[test]
    fn a_signed_event_for_several_groups_must_name_exactly_those_groups() {
        let all_three = many(["a", "b", "c"]);
        let complete = signed(vec![
            Tag::parse(["h", "a"]).unwrap(),
            Tag::parse(["h", "b"]).unwrap(),
            Tag::parse(["h", "c"]).unwrap(),
        ]);
        assert_eq!(validate_context(&all_three, &complete), Ok(()));

        let short = signed(vec![
            Tag::parse(["h", "a"]).unwrap(),
            Tag::parse(["h", "b"]).unwrap(),
        ]);
        assert_eq!(
            validate_context(&all_three, &short).err(),
            Some(GroupContextError::MismatchedContext {
                found: many(["a", "b"]),
                expected: all_three.clone(),
            }),
            "a room the app forgot to sign in must be a refusal, not a quiet partial write"
        );

        let over = signed(vec![
            Tag::parse(["h", "a"]).unwrap(),
            Tag::parse(["h", "b"]).unwrap(),
            Tag::parse(["h", "c"]).unwrap(),
            Tag::parse(["h", "d"]).unwrap(),
        ]);
        assert_eq!(
            validate_context(&all_three, &over).err(),
            Some(GroupContextError::MismatchedContext {
                found: many(["a", "b", "c", "d"]),
                expected: all_three.clone(),
            })
        );

        assert_eq!(
            validate_context(&all_three, &signed(Vec::new())).err(),
            Some(GroupContextError::MissingContext {
                expected: all_three
            })
        );
    }

    /// The refusal the retired `AmbiguousContext` used to make for the
    /// one-group case, kept rather than lost: a signed event whose `h` rows
    /// name the right SET but repeat one of them is still refused, because
    /// those are not rows [`contextualize`] could ever have produced.
    ///
    /// Without this variant a set comparison alone would say `Ok` here,
    /// which would be strictly weaker than the door was before #1281.
    #[test]
    fn a_signed_event_repeating_a_group_is_refused_even_though_the_set_is_right() {
        let event = signed(vec![
            Tag::parse(["h", GROUP]).unwrap(),
            Tag::parse(["h", GROUP]).unwrap(),
        ]);
        let error = validate_context(&one(GROUP), &event)
            .expect_err("a repeated row is not a row this door mints");
        assert_eq!(
            error,
            GroupContextError::RepeatedContext {
                repeated: one(GROUP)
            }
        );
        assert!(error.to_string().contains(GROUP), "{error}");
    }

    /// THE asymmetry falsifier, and the one with real consumer history.
    ///
    /// A consumer once refused a multi-`h` write on the UNSIGNED path and not
    /// on the signed one, and reached the shape it wanted by routing a
    /// kind:30315 session status through the signed path specifically to
    /// exploit that gap. #1274 closed it by running NMP's own validation on
    /// both paths; #1281 widens what "either arity" means, and this test is
    /// what stops the gap reopening at the new arity.
    ///
    /// The claim is exact: at EVERY arity, the only `h` set a signed event may
    /// carry is the one the door was asked for, and the bytes that validate
    /// are exactly what [`contextualize`] would have composed for that same
    /// set. So there is no laxity on the signed path that the unsigned path
    /// lacks, and no spelling of "sign it yourself to get past the refusal".
    ///
    /// What makes a multi-`h` write legitimate is therefore never a property
    /// of the EVENT -- it is the retained set the door holds. The identical
    /// bytes that validate through `{a, b}` are refused through `{a}`.
    #[test]
    fn the_signed_path_is_exactly_as_strict_as_the_unsigned_one_at_every_arity() {
        let h = |ids: &[&str]| -> Vec<Tag> {
            ids.iter()
                .map(|id| Tag::parse(["h", id]).expect("a two-value row is well-formed"))
                .collect()
        };
        for retained in [many(["a"]), many(["a", "b"]), many(["a", "b", "c"])] {
            let ids: Vec<&str> = retained.iter().map(String::as_str).collect();

            // The unsigned door's own output is the reference: whatever it
            // composes is exactly what the signed door must accept.
            let composed = contextualize(&retained, EventBuilder::new(Kind::from(30315u16)))
                .unwrap_or_else(|error| panic!("{retained:?} must contextualize: {error}"));
            assert_eq!(
                context_rows(&composed),
                ids.iter()
                    .map(|id| (*id).to_string())
                    .collect::<Vec<String>>(),
                "the unsigned door must compose exactly the retained set"
            );
            assert_eq!(
                validate_context(&retained, &signed(h(&ids))),
                Ok(()),
                "the signed door must accept exactly the bytes the unsigned door composes"
            );

            // Every NEIGHBOUR of that set is refused: one group more, one
            // fewer, one swapped for a stranger, one repeated. None of these
            // is expressible on the unsigned path at all, so accepting any of
            // them would be laxity the signed path alone had.
            let one_more: Vec<&str> = ids.iter().copied().chain(["elsewhere"]).collect();
            assert!(
                validate_context(&retained, &signed(h(&one_more))).is_err(),
                "{retained:?}: a group the door was not asked for must be refused"
            );

            let one_fewer: Vec<&str> = ids.iter().copied().skip(1).collect();
            assert!(
                validate_context(&retained, &signed(h(&one_fewer))).is_err(),
                "{retained:?}: a dropped group must be refused, never quietly narrowed"
            );

            let mut swapped: Vec<&str> = ids.clone();
            swapped[0] = "elsewhere";
            assert!(
                validate_context(&retained, &signed(h(&swapped))).is_err(),
                "{retained:?}: a swapped group must be refused even though the count matches"
            );

            let repeated: Vec<&str> = ids.iter().copied().chain([ids[0]]).collect();
            assert_eq!(
                validate_context(&retained, &signed(h(&repeated))).err(),
                Some(GroupContextError::RepeatedContext {
                    repeated: one(ids[0])
                }),
                "{retained:?}: a repeated row is not a row the unsigned door could mint"
            );
        }

        // The same bytes, two doors: legality is the DOOR's property, never
        // the event's. This is the sentence the whole design turns on.
        let both = signed(vec![
            Tag::parse(["h", "a"]).unwrap(),
            Tag::parse(["h", "b"]).unwrap(),
        ]);
        assert_eq!(validate_context(&many(["a", "b"]), &both), Ok(()));
        assert_eq!(
            validate_context(&one("a"), &both).err(),
            Some(GroupContextError::MismatchedContext {
                found: many(["a", "b"]),
                expected: one("a"),
            }),
            "an identical event is legitimate through the door that named both rooms and \
             refused through the door that named one -- no property of the bytes alone \
             makes a multi-h write acceptable"
        );
    }

    /// A refusal reads as a sentence at either arity -- `group "x"` for one,
    /// `groups "x", "y"` for several -- so the message never leaks which
    /// internal shape produced it.
    #[test]
    fn a_refusal_names_one_group_singular_and_several_plural() {
        let single = GroupContextError::MissingContext {
            expected: one(GROUP),
        }
        .to_string();
        assert!(
            single.contains("group \"photographers\"") && !single.contains("groups"),
            "{single}"
        );
        let plural = GroupContextError::MissingContext {
            expected: many(["a", "b"]),
        }
        .to_string();
        assert!(plural.contains("groups \"a\", \"b\""), "{plural}");
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

    /// #1245: the three relay-signed group records key themselves by `d`, so
    /// an `h`-scoped read of them can only ever match nothing. Each is
    /// refused, individually and together, and the refusal NAMES the kinds so
    /// a caller can see which part of its selection was the problem.
    ///
    /// Before this, `Group::read(kinds:[39001,39002])` returned an
    /// `Ok(LiveQuery)` that opened a subscription and then stayed empty
    /// forever, indistinguishable from a group whose relay published no
    /// roster.
    #[test]
    fn a_read_selection_naming_the_relay_signed_records_is_refused_not_answered() {
        for (kinds, expected) in [
            (BTreeSet::from([39000u16]), BTreeSet::from([39000u16])),
            (BTreeSet::from([39001u16]), BTreeSet::from([39001u16])),
            (BTreeSet::from([39002u16]), BTreeSet::from([39002u16])),
            (
                BTreeSet::from([39001u16, 39002u16]),
                BTreeSet::from([39001u16, 39002u16]),
            ),
            // Mixed with ordinary group content: the presence of readable
            // content does not license silently dropping the unmatchable part.
            (BTreeSet::from([9u16, 39002u16]), BTreeSet::from([39002u16])),
        ] {
            let selection = Filter {
                kinds: Some(kinds.clone()),
                ..Filter::default()
            };
            assert_eq!(
                group_demand_at(&host(), GROUP, selection).err(),
                Some(GroupContextError::RecordsAreNotContextScoped { kinds: expected }),
                "selection {kinds:?} must be refused, not answered with a permanent silence"
            );
        }
    }

    /// The refusal says which axis the records are keyed by, so a caller
    /// reading the message learns where to go instead.
    #[test]
    fn the_records_refusal_names_both_tag_axes() {
        let selection = Filter {
            kinds: Some(BTreeSet::from([39002u16])),
            ..Filter::default()
        };
        let said = group_demand_at(&host(), GROUP, selection)
            .expect_err("a d-keyed record is unreachable through the h door")
            .to_string();
        assert!(said.contains("'d'") && said.contains("'h'"), "{said}");
        assert!(said.contains("39002"), "{said}");
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

    /// PROTOCOL-READSTHROUGHTHEONEDOOR-003: the app chooses the kinds; the
    /// group imposes no catalogue of its own. Exercised over an arbitrary
    /// table of kind sets -- including ones NIP-29 itself does not define
    /// (31337) and ones that belong to other NIPs entirely (7, 30315) -- so
    /// passing is never explainable by "it happens to be a kind the group
    /// recognizes". Every case must come back with EXACTLY the app's own
    /// kind set, still pinned to `host` and still scoped to `GROUP`.
    ///
    /// The one thing this door does read the selection for is the `d`-keyed
    /// records refusal below, which is about which tag axis keys the event
    /// and not about which kinds may live in a group.
    #[test]
    fn a_read_branch_imposes_no_kind_catalogue_over_arbitrary_app_selections() {
        let cases: Vec<BTreeSet<u16>> = vec![
            BTreeSet::from([9u16]),
            BTreeSet::from([9u16, 9000u16]),
            BTreeSet::from([30315u16]),
            BTreeSet::from([7u16]),
            BTreeSet::from([9022u16]),
            BTreeSet::from([31337u16]),
        ];
        for kinds in cases {
            let selection = Filter {
                kinds: Some(kinds.clone()),
                ..Filter::default()
            };
            let demand = group_demand_at(&host(), GROUP, selection)
                .unwrap_or_else(|error| panic!("kinds {kinds:?} must scope cleanly: {error}"));
            assert_eq!(
                demand.selection.kinds,
                Some(kinds.clone()),
                "the app's exact kind set for {kinds:?} must survive untouched"
            );
            assert_eq!(
                demand.source,
                SourceAuthority::Pinned(BTreeSet::from([host()])),
                "kinds {kinds:?}: the branch stays pinned to the one host regardless of kind"
            );
            assert_eq!(
                demand.selection.tags.get(&context_tag()),
                Some(&Binding::Literal(BTreeSet::from([GROUP.to_string()]))),
                "kinds {kinds:?}: the branch stays scoped to the one group id regardless of kind"
            );
        }
    }
}
