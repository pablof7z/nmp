//! NIP-29 send-into-group composition (#115) -- the write-side counterpart
//! to `demand.rs`'s host-scoped reads. Kind-blind, protocol-blind at the
//! engine: this module is the ONE place that knows "a group send is a
//! `WriteIntent` routed to the group's pinned host, carrying an `h` tag and
//! (best-effort) a `previous` recency tag" -- callers never hand-roll any
//! of that themselves (see `compose_group_send`'s own doc for the
//! hand-roll/courier distinction the Fable ruling drew).
//!
//! `WriteIntent`/`WriteRouting`/`HostAuthority` etc. live in `nmp-grammar`
//! (#115's Fork 3 dependency ruling relocated them there from
//! `nmp-engine::outbox` for exactly this reason: a protocol module composing
//! a `WriteIntent` must not gain an engine dependency to do so). This module
//! and the crate's default feature set still have no router/resolver/store/
//! engine dependency -- falsifier 9 proves it. The optional `engine` feature
//! adds the semantic kind:9 operation in `message.rs` without changing this
//! lower-level seam.

use nostr::{EventId, Kind, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};

use nmp_grammar::{Durability, HostAuthority, WriteIntent, WritePayload, WriteRouting};

/// Cap on how many recent event ids a `previous` tag couriers. Best-effort
/// recency evidence, not a completeness guarantee -- see
/// [`GroupTimelineEvidence`]'s own doc.
pub const PREVIOUS_MAX: usize = 10;

/// `compose_group_send`'s failure modes. `ReservedTag` is the ratified
/// #115 case (`extra_tags` named a tag this module itself owns);
/// `MalformedTag` is not named in the ruling's illustrative enum but is
/// required by the same codebase-wide doctrine `nmp-ffi::convert::
/// tags_from_ffi` already enforces -- a malformed tag rejects the WHOLE
/// write, it is never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupSendError {
    /// `extra_tags` contained a row named `"h"` or `"previous"` -- both are
    /// owned by this function (it derives them from `group_id` and
    /// `previous` itself); surfaced, never silently rewritten or dropped.
    ReservedTag(String),
    /// `extra_tags` contained a row with no tag name at all.
    MalformedTag(Vec<String>),
}

/// Compose-time couriered evidence for a group's `previous` tag (#115 Fork
/// 2). NOT engine state -- the frozen-template invariant (`on_publish`
/// freezes the payload at acceptance) structurally bars any design where
/// the engine injects this after publish, and the engine exposes no store-
/// peek door for compose to query directly. Instead, the CALLER courier's
/// rows it already has (from its live `group_content_demand` read, #108) --
/// this type owns 100% of the selection/verification/truncation/encoding
/// of those rows into a `previous` tag, the inverse of the read-side
/// `decode_remembered_groups` courier pattern.
///
/// Best-effort, not authoritative: a `previous` ref is minted ONLY from a
/// row this client actually saw delivered -- that is the provenance
/// guarantee (falsifier 5). Whether those rows are fresh, complete, or
/// missing entries the client hasn't received yet is explicitly OUT of
/// scope: courier-starvation or replay of a stale `previous` set is
/// acceptable per NIP-29's own best-effort semantics for this tag, not a
/// bug in this type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupTimelineEvidence {
    ids: Vec<EventId>,
}

impl GroupTimelineEvidence {
    /// No couriered rows -- a legal, first-class value. `compose_group_send`
    /// emits no `previous` tag at all for this case; the send is still
    /// valid (a host that requires `previous` surfaces that as a typed
    /// rejection on the receipt stream, never a silent client-side no-op).
    pub fn none() -> Self {
        Self { ids: Vec::new() }
    }

    /// Build evidence from delivered rows -- `(id, created_at, raw tags)`
    /// exactly as a live `group_content_demand` read renders them.
    /// KIND-BLIND: membership is decided by the `h` tag alone, never by
    /// `kind`. Keeps at most [`PREVIOUS_MAX`] ids, newest (`created_at`)
    /// first.
    pub fn from_events(
        group_id: &str,
        items: impl IntoIterator<Item = (EventId, u64, Vec<Vec<String>>)>,
    ) -> Self {
        let mut rows: Vec<(EventId, u64)> = items
            .into_iter()
            .filter(|(_, _, tags)| is_member_of(tags, group_id))
            .map(|(id, created_at, _tags)| (id, created_at))
            .collect();
        rows.sort_by_key(|(_id, created_at)| std::cmp::Reverse(*created_at));
        rows.truncate(PREVIOUS_MAX);
        Self {
            ids: rows.into_iter().map(|(id, _created_at)| id).collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// 8-hex-character prefixes, in the same newest-first order `ids` is
    /// already stored in.
    fn prefixes(&self) -> Vec<String> {
        self.ids
            .iter()
            .map(|id| id.to_hex()[..8].to_string())
            .collect()
    }
}

fn is_member_of(tags: &[Vec<String>], group_id: &str) -> bool {
    tags.iter().any(|tag| {
        tag.first().map(String::as_str) == Some("h")
            && tag.get(1).map(String::as_str) == Some(group_id)
    })
}

/// The ONE generic "publish this event into this group on its host" --
/// kind-blind (`kind` is entirely the caller's choice; nothing in this
/// function branches on it). Appends `["h", group_id]` and, iff `previous`
/// is nonempty, `["previous", <8-char prefixes>...]` newest-first. Routes
/// via `WriteRouting::PinnedHost(HostAuthority::from_selected_host(host))`
/// -- the engine treats this as "route to exactly this one relay," nothing
/// more, and never learns this is a NIP-29 group host.
///
/// Takes `(host, group_id)` PRIMITIVES rather than a `GroupRef` -- a group
/// the caller has browsed but not remembered (no #63 kind:10009 entry) must
/// still be sendable.
///
/// The hand-roll/courier distinction the #115 ruling drew: *hand-rolling*
/// would be the app deciding the `h`/`previous` tag names, truncation,
/// count, or encoding itself; *couriering* is the app handing this function
/// the rows it already has (from its own live read) and this function
/// owning 100% of the selection/verification/truncation/encoding. Every
/// caller of this crate does the latter.
///
/// Durability is hardwired to [`Durability::Durable`] (phase 1, overridable
/// later) and the payload is always [`WritePayload::Unsigned`] -- signing
/// and publishing are orthogonal stages (#47/#32); the engine signs the
/// exact frozen template this function composed, so `["h", ...]`/
/// `["previous", ...]` flow through the freeze -> sign -> validate chain
/// untouched (F1: the engine cannot and does not inject either tag after
/// the fact).
// The #115 ratified spec's exact signature -- 8 positional primitives,
// matching the codebase's existing `#[allow(clippy::too_many_arguments)]`
// precedent (`nmp-store`/`nmp-transport`) rather than bundling these into
// an ad-hoc params struct the ruling never asked for.
#[allow(clippy::too_many_arguments)]
pub fn compose_group_send(
    host: RelayUrl,
    group_id: &str,
    author: PublicKey,
    created_at: Timestamp,
    kind: u16,
    content: String,
    extra_tags: Vec<Vec<String>>,
    previous: &GroupTimelineEvidence,
) -> Result<WriteIntent, GroupSendError> {
    let mut tags = Vec::with_capacity(extra_tags.len());
    for row in extra_tags {
        match row.first().map(String::as_str) {
            Some("h") | Some("previous") => {
                return Err(GroupSendError::ReservedTag(row[0].clone()));
            }
            _ => {}
        }
        let tag = Tag::parse(row.clone()).map_err(|_| GroupSendError::MalformedTag(row))?;
        tags.push(tag);
    }

    Ok(compose_group_send_with_tags(
        host, group_id, author, created_at, kind, content, tags, previous,
    ))
}

/// Typed interior seam used by the semantic group-message operation. The
/// public raw composer above remains the validation boundary for callers that
/// deliberately supply arbitrary event tags; this helper accepts only tags
/// already minted by this crate and therefore cannot fail with
/// [`GroupSendError`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn compose_group_send_with_tags(
    host: RelayUrl,
    group_id: &str,
    author: PublicKey,
    created_at: Timestamp,
    kind: u16,
    content: String,
    mut tags: Vec<Tag>,
    previous: &GroupTimelineEvidence,
) -> WriteIntent {
    tags.reserve(2);

    tags.push(Tag::parse(["h", group_id]).expect("'h' is a well-formed non-empty row"));
    if !previous.is_empty() {
        let mut row = vec!["previous".to_string()];
        row.extend(previous.prefixes());
        tags.push(Tag::parse(row).expect("'previous' is a well-formed non-empty row"));
    }

    let unsigned = UnsignedEvent::new(author, created_at, Kind::from(kind), tags, content);

    WriteIntent {
        payload: WritePayload::Unsigned(unsigned),
        durability: Durability::Durable,
        routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(host)),
        // Composed group sends keep the default identity contract (#47):
        // `author` must be the active account at publish time.
        identity_override: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> RelayUrl {
        RelayUrl::parse("wss://group-host.example.com").unwrap()
    }

    fn pubkey() -> PublicKey {
        nostr::Keys::generate().public_key()
    }

    fn event_id(byte: u8) -> EventId {
        EventId::from_slice(&[byte; 32]).unwrap()
    }

    /// `WriteIntent` (the `Ok` side) carries no `Debug` impl, so plain
    /// `.unwrap_err()` doesn't typecheck -- this matches it out by hand.
    fn expect_err(result: Result<WriteIntent, GroupSendError>) -> GroupSendError {
        match result {
            Err(err) => err,
            Ok(_) => panic!("expected an error, got Ok"),
        }
    }

    /// Falsifier 1: kind is entirely opaque to this function -- two sends
    /// differing only in `kind` produce identical routing and identical
    /// `h`/`previous` tags.
    #[test]
    fn kind_blind_send_is_identical_across_arbitrary_kinds() {
        let previous = GroupTimelineEvidence::none();
        let a = compose_group_send(
            host(),
            "group-a",
            pubkey(),
            Timestamp::from(1u64),
            9,
            "hi".to_string(),
            vec![],
            &previous,
        )
        .unwrap();
        let b = compose_group_send(
            host(),
            "group-a",
            pubkey(),
            Timestamp::from(1u64),
            9999,
            "hi".to_string(),
            vec![],
            &previous,
        )
        .unwrap();
        let WritePayload::Unsigned(a_unsigned) = &a.payload else {
            panic!("expected Unsigned")
        };
        let WritePayload::Unsigned(b_unsigned) = &b.payload else {
            panic!("expected Unsigned")
        };
        assert_eq!(a_unsigned.tags, b_unsigned.tags);
        assert!(matches!(a.routing, WriteRouting::PinnedHost(_)));
        assert!(matches!(b.routing, WriteRouting::PinnedHost(_)));
    }

    /// Falsifier 5(a): a caller-supplied `extra_tags` row naming `h` or
    /// `previous` is rejected, never silently rewritten.
    #[test]
    fn reserved_tag_names_are_rejected_not_rewritten() {
        let previous = GroupTimelineEvidence::none();
        let err = expect_err(compose_group_send(
            host(),
            "group-a",
            pubkey(),
            Timestamp::from(1u64),
            9,
            "hi".to_string(),
            vec![vec!["h".to_string(), "sneaky".to_string()]],
            &previous,
        ));
        assert_eq!(err, GroupSendError::ReservedTag("h".to_string()));

        let err = expect_err(compose_group_send(
            host(),
            "group-a",
            pubkey(),
            Timestamp::from(1u64),
            9,
            "hi".to_string(),
            vec![vec!["previous".to_string(), "deadbeef".to_string()]],
            &previous,
        ));
        assert_eq!(err, GroupSendError::ReservedTag("previous".to_string()));
    }

    /// A malformed (empty) extra_tags row rejects the whole compose, never
    /// silently dropped -- same discipline as `nmp-ffi::convert::
    /// tags_from_ffi`.
    #[test]
    fn malformed_extra_tag_rejects_the_whole_compose() {
        let previous = GroupTimelineEvidence::none();
        let err = expect_err(compose_group_send(
            host(),
            "group-a",
            pubkey(),
            Timestamp::from(1u64),
            9,
            "hi".to_string(),
            vec![vec![]],
            &previous,
        ));
        assert_eq!(err, GroupSendError::MalformedTag(vec![]));
    }

    /// Falsifier 5(c): zero couriered rows -> no `previous` tag at all,
    /// and the send is still composed (still a valid `WriteIntent`) -- a
    /// host requiring `previous` rejects it downstream, this function never
    /// blocks the send itself.
    #[test]
    fn zero_previous_rows_omit_the_tag_entirely() {
        let intent = compose_group_send(
            host(),
            "group-a",
            pubkey(),
            Timestamp::from(1u64),
            9,
            "hi".to_string(),
            vec![],
            &GroupTimelineEvidence::none(),
        )
        .unwrap();
        let WritePayload::Unsigned(unsigned) = &intent.payload else {
            panic!("expected Unsigned")
        };
        assert!(!unsigned
            .tags
            .iter()
            .any(|t| t.kind().to_string() == "previous"));
    }

    /// Falsifier 5(b)+(d): rows from a different group are excluded, and
    /// the surviving refs are 8-char prefixes, newest-first, capped at
    /// `PREVIOUS_MAX`.
    #[test]
    fn previous_evidence_excludes_other_groups_and_orders_newest_first() {
        let rows = vec![
            (
                event_id(1),
                100,
                vec![vec!["h".to_string(), "group-a".to_string()]],
            ),
            (
                event_id(2),
                300,
                vec![vec!["h".to_string(), "group-a".to_string()]],
            ),
            (
                event_id(3),
                200,
                vec![vec!["h".to_string(), "other-group".to_string()]],
            ),
        ];
        let evidence = GroupTimelineEvidence::from_events("group-a", rows);
        assert_eq!(evidence.ids, vec![event_id(2), event_id(1)]);

        let prefixes = evidence.prefixes();
        assert_eq!(prefixes.len(), 2);
        for prefix in &prefixes {
            assert_eq!(prefix.len(), 8);
        }

        let intent = compose_group_send(
            host(),
            "group-a",
            pubkey(),
            Timestamp::from(1u64),
            9,
            "hi".to_string(),
            vec![],
            &evidence,
        )
        .unwrap();
        let WritePayload::Unsigned(unsigned) = &intent.payload else {
            panic!("expected Unsigned")
        };
        let previous_tag = unsigned
            .tags
            .iter()
            .find(|t| t.kind().to_string() == "previous")
            .expect("previous tag present");
        assert_eq!(previous_tag.as_slice()[1..], prefixes);
    }

    /// `PREVIOUS_MAX` truncates even when more rows are delivered.
    #[test]
    fn previous_evidence_caps_at_previous_max() {
        let rows: Vec<_> = (0..(PREVIOUS_MAX as u8 + 5))
            .map(|n| {
                (
                    event_id(n + 1),
                    n as u64,
                    vec![vec!["h".to_string(), "group-a".to_string()]],
                )
            })
            .collect();
        let evidence = GroupTimelineEvidence::from_events("group-a", rows);
        assert_eq!(evidence.ids.len(), PREVIOUS_MAX);
    }

    /// The `h` tag always names exactly the group this send targets.
    #[test]
    fn h_tag_always_names_the_target_group() {
        let intent = compose_group_send(
            host(),
            "group-a",
            pubkey(),
            Timestamp::from(1u64),
            9,
            "hi".to_string(),
            vec![],
            &GroupTimelineEvidence::none(),
        )
        .unwrap();
        let WritePayload::Unsigned(unsigned) = &intent.payload else {
            panic!("expected Unsigned")
        };
        let h_tag = unsigned
            .tags
            .iter()
            .find(|t| t.kind().to_string() == "h")
            .expect("h tag present");
        assert_eq!(h_tag.as_slice()[1], "group-a");
    }
}
