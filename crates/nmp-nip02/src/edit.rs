use nmp::{
    Engine, EventBuilder, Identity, RegisteredReplaceableMaterializer, ReplaceableMaterializer,
    ReplaceableMaterializerOperation, ReplaceableMaterializerRefusal, ReplaceableSourcePolicy, Row,
    WriteIntent, WritePayload, WriteRouting,
};
use nmp_event_edit::{
    EventEditPlan, TagEdit, TagInsertion, TagItemPattern, TagItemSelector, TagRowPattern,
};
use nostr::{EventId, Kind, PublicKey, Tag};

/// The requested relationship after a NIP-02 edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowChange {
    Follow,
    Unfollow,
}

/// A pure edit either proves the contact list already has the requested
/// relationship or returns one closed, compare-and-swap write intent.
pub enum ComposeFollowResult {
    NoChange,
    Publish(Box<WriteIntent>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeFollowError {
    BaseHasWrongKind,
    InvalidGeneratedTag,
    InvalidOperation,
}

const FOLLOW_PROGRAM: [u8; 16] = *b"nmp-nip02-follow";
const FOLLOW_FORMAT: [u8; 16] = *b"nip02-follow-v01";
const FOLLOW_OPERATION_VERSION: u8 = 1;
const FOLLOW_OPERATION_LEN: usize = 34;

/// Registration-bound NIP-02 write composer.
///
/// The value can be obtained only by configuring the matching materializer
/// on an engine. It exposes typed follow/unfollow composition, not replay ids,
/// opaque bytes, source authority, or contributor membership.
pub struct FollowWrites {
    registration: RegisteredReplaceableMaterializer,
}

/// Configure NIP-02's synchronous materializer before composing operations.
/// A missing implementation can therefore never become retained waiting
/// work: without this returned value there is no supported operation door.
pub fn register_follow_writes(engine: &Engine) -> Result<FollowWrites, nmp::EngineError> {
    engine
        .add_replaceable_materializer(FOLLOW_PROGRAM, FOLLOW_FORMAT, FollowMaterializer)
        .map(|registration| FollowWrites { registration })
}

impl FollowWrites {
    /// Compose one semantic follow change over a complete current event.
    ///
    /// `current` may be NMP's complete signature-pending optimistic row. The
    /// registration handle binds the operation to this engine instance; NMP
    /// owns the retained source authority used for later replay.
    pub fn compose(
        &self,
        current: &Row,
        target: PublicKey,
        change: FollowChange,
    ) -> Result<ComposeFollowResult, ComposeFollowError> {
        if current.kind() != Kind::ContactList {
            return Err(ComposeFollowError::BaseHasWrongKind);
        }
        let wants_follow = change == FollowChange::Follow;
        if follows(current, target) == wants_follow {
            return Ok(ComposeFollowResult::NoChange);
        }

        let payload = self
            .registration
            .operation(
                current,
                ReplaceableSourcePolicy::Continuing,
                encode_follow_operation(target, change),
            )
            .map_err(|_| ComposeFollowError::InvalidOperation)?;
        Ok(ComposeFollowResult::Publish(Box::new(WriteIntent {
            payload,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })))
    }
}

fn encode_follow_operation(target: PublicKey, change: FollowChange) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(FOLLOW_OPERATION_LEN);
    bytes.push(FOLLOW_OPERATION_VERSION);
    bytes.push(match change {
        FollowChange::Follow => 1,
        FollowChange::Unfollow => 0,
    });
    bytes.extend_from_slice(target.as_bytes());
    bytes
}

fn decode_follow_operation(bytes: &[u8]) -> Result<(PublicKey, FollowChange), String> {
    if bytes.len() != FOLLOW_OPERATION_LEN {
        return Err("NIP-02 operation has the wrong length".to_string());
    }
    if bytes[0] != FOLLOW_OPERATION_VERSION {
        return Err("NIP-02 operation has an unsupported version".to_string());
    }
    let change = match bytes[1] {
        0 => FollowChange::Unfollow,
        1 => FollowChange::Follow,
        _ => return Err("NIP-02 operation has an invalid change".to_string()),
    };
    let target = PublicKey::from_slice(&bytes[2..])
        .map_err(|_| "NIP-02 operation has an invalid public key".to_string())?;
    Ok((target, change))
}

struct FollowMaterializer;

fn apply_follow_operations<'a>(
    current: &nostr::UnsignedEvent,
    operations: impl IntoIterator<Item = &'a [u8]>,
) -> Result<EventBuilder, String> {
    let mut tags = current.tags.clone().to_vec();
    for operation in operations {
        let (target, change) = decode_follow_operation(operation)?;
        let target_hex = target.to_hex();
        match change {
            FollowChange::Follow => {
                if !tags.iter().any(|tag| {
                    let row = tag.as_slice();
                    row.first().is_some_and(|cell| cell == "p")
                        && row.get(1).is_some_and(|cell| cell == &target_hex)
                }) {
                    tags.push(Tag::public_key(target));
                }
            }
            FollowChange::Unfollow => tags.retain(|tag| {
                let row = tag.as_slice();
                !(row.first().is_some_and(|cell| cell == "p")
                    && row.get(1).is_some_and(|cell| cell == &target_hex))
            }),
        }
    }

    Ok(EventBuilder {
        kind: Kind::ContactList,
        tags,
        content: current.content.clone(),
        created_at: None,
    })
}

impl ReplaceableMaterializer for FollowMaterializer {
    fn materialize(
        &self,
        source: &nostr::UnsignedEvent,
        current: &nostr::UnsignedEvent,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
        if source.kind != Kind::ContactList
            || current.kind != Kind::ContactList
            || source.pubkey != current.pubkey
            || source.tags.identifier() != current.tags.identifier()
        {
            return Err(ReplaceableMaterializerRefusal {
                reason: "NIP-02 materialization source coordinate changed".to_string(),
            });
        }

        apply_follow_operations(
            current,
            operations.iter().map(|operation| operation.bytes()),
        )
        .map_err(|reason| ReplaceableMaterializerRefusal { reason })
    }

    fn materialize_default(
        &self,
        _coordinate: &nostr::nips::nip01::Coordinate,
        _operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
        Err(ReplaceableMaterializerRefusal {
            reason: "NIP-02 first-value materialization is not exposed yet".to_string(),
        })
    }
}

fn follow_selector(target: PublicKey) -> TagItemSelector {
    TagItemSelector::one(
        TagItemPattern::new(vec![TagRowPattern::prefix(vec![
            "p".to_string(),
            target.to_hex(),
        ])
        .expect("the fixed NIP-02 selector has a non-empty row")])
        .expect("the fixed NIP-02 selector has one row"),
    )
}

/// True when `event` contains any NIP-02 `p` tag for `target`. Relay hints,
/// petnames, extra tag fields, malformed unrelated tags, and ordering are
/// deliberately irrelevant to membership and remain untouched by edits.
pub fn follows(event: &Row, target: PublicKey) -> bool {
    let source = event.tags().iter().map(Tag::as_slice).collect::<Vec<_>>();
    follow_selector(target).matches_any(&source)
}

/// Compose a NIP-02 whole-list replacement from an exact local base.
///
/// Every tag and the content string are preserved byte-for-byte and in the
/// same order except for the requested target: follow appends one minimal
/// `p` tag (NIP-02's chronological convention), while unfollow removes all
/// matching `p` tags. The returned payload carries `base.id` as an atomic
/// acceptance precondition; a concurrent winner produces a typed conflict
/// before any write is journaled. This ordinary edit requires an established
/// base. Creating a first contact list needs a separately named policy and
/// cannot masquerade as `follow`.
///
/// It takes neither an author nor a clock. A base somebody else authored
/// needs no error of its own: the precondition is checked at the editing
/// identity's own coordinate, where a foreign event is never the winner, so
/// it reports through the same conflict door as every other stale base. And
/// the timestamp is decided inside the acceptance transaction as
/// `max(clock, winner + 1)` -- against the row the precondition is holding,
/// which is the only place monotonicity can actually be guaranteed, so
/// there is no arithmetic here left to exhaust.
pub fn compose_follow_change(
    base: &Row,
    target: PublicKey,
    change: FollowChange,
) -> Result<ComposeFollowResult, ComposeFollowError> {
    if base.kind() != Kind::ContactList {
        return Err(ComposeFollowError::BaseHasWrongKind);
    }

    let wants_follow = change == FollowChange::Follow;
    let selector = follow_selector(target);
    let edit = if wants_follow {
        TagEdit::ensure_present(
            selector,
            vec![vec!["p".to_string(), target.to_hex()]],
            TagInsertion::end(),
        )
        .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?
    } else {
        TagEdit::remove(selector)
    };
    let plan = EventEditPlan::tags(edit);
    // Borrow raw cells for matching; only the final changed document is
    // reconstructed. There is no input-wide string clone before the edit.
    let source = base.tags().iter().map(Tag::as_slice).collect::<Vec<_>>();
    let applied = plan
        .apply_tags(&source)
        .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?;
    let Some(rows) = applied.replacement else {
        return Ok(ComposeFollowResult::NoChange);
    };
    // `nostr::Tag` owns raw cells. Reparse only reconstructs that foreign
    // container after the lower transform; focused raw-cell tests pin this
    // conversion as byte-exact so a future upstream normalization fails.
    let tags = rows
        .into_iter()
        .map(Tag::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?;

    Ok(ComposeFollowResult::Publish(Box::new(WriteIntent {
        payload: WritePayload::ReplaceableEdit {
            builder: EventBuilder {
                kind: Kind::ContactList,
                tags,
                content: base.content().to_owned(),
                created_at: None,
            },
            expected_base: Some(base.id()),
        },
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    })))
}

/// Extract the precondition for tests and adapters without opening up any
/// mutable registry or protocol projection.
pub fn expected_base(intent: &WriteIntent) -> Option<Option<EventId>> {
    match &intent.payload {
        WritePayload::ReplaceableEdit { expected_base, .. } => Some(*expected_base),
        WritePayload::Event(_)
        | WritePayload::ReplaceableOperation(_)
        | WritePayload::Signed(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn event(author: &Keys, at: u64, raw_tags: Vec<Vec<&str>>, content: &str) -> Row {
        let tags = raw_tags
            .into_iter()
            .map(|values| {
                Tag::parse(values.into_iter().map(str::to_string).collect::<Vec<_>>()).unwrap()
            })
            .collect::<Vec<_>>();
        let event = nostr::UnsignedEvent::new(
            author.public_key(),
            nostr::Timestamp::from_secs(at),
            Kind::ContactList,
            tags,
            content,
        )
        .sign_with_keys(author)
        .unwrap();
        Row::from_parts(
            event.id,
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags,
            event.content,
            nmp::RowSignature::Signed(event.sig),
            Default::default(),
        )
    }

    fn composed(intent: &WriteIntent) -> &EventBuilder {
        let WritePayload::ReplaceableEdit { builder, .. } = &intent.payload else {
            panic!("expected replaceable edit")
        };
        builder
    }

    #[test]
    fn follow_appends_and_preserves_every_existing_field() {
        let author = Keys::generate();
        let existing = Keys::generate();
        let target = Keys::generate();
        let base = event(
            &author,
            10,
            vec![
                vec!["client", "keep-me"],
                vec![
                    "p",
                    &existing.public_key().to_hex(),
                    "wss://hint.example",
                    "pet",
                ],
                vec!["x", "opaque", "tokens"],
            ],
            "legacy content must survive",
        );

        let ComposeFollowResult::Publish(intent) =
            compose_follow_change(&base, target.public_key(), FollowChange::Follow).unwrap()
        else {
            panic!("must publish")
        };

        let draft = composed(&intent);
        // Unstated on purpose: the acceptance transaction stamps
        // `max(clock, winner + 1)` against the row it is CAS-ing, which is
        // the only place the winner is actually known.
        assert_eq!(draft.created_at, None);
        assert_eq!(draft.content, base.content());
        let actual: Vec<Vec<String>> = draft.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        let mut expected: Vec<Vec<String>> =
            base.tags().iter().map(|t| t.as_slice().to_vec()).collect();
        expected.push(vec!["p".into(), target.public_key().to_hex()]);
        assert_eq!(actual, expected);
        assert_eq!(expected_base(&intent), Some(Some(base.id())));
    }

    #[test]
    fn unfollow_removes_all_target_tags_only_and_keeps_order() {
        let author = Keys::generate();
        let target = Keys::generate();
        let other = Keys::generate();
        let target_hex = target.public_key().to_hex();
        let other_hex = other.public_key().to_hex();
        let base = event(
            &author,
            20,
            vec![
                vec!["p", &target_hex, "wss://one", "one"],
                vec!["x", "keep"],
                vec!["p", &other_hex, "wss://other", "friend"],
                vec!["p", &target_hex, "wss://two", "two"],
            ],
            "",
        );
        let ComposeFollowResult::Publish(intent) =
            compose_follow_change(&base, target.public_key(), FollowChange::Unfollow).unwrap()
        else {
            panic!("must publish")
        };
        let actual: Vec<Vec<String>> = composed(&intent)
            .tags
            .iter()
            .map(|t| t.as_slice().to_vec())
            .collect();
        assert_eq!(
            actual,
            vec![
                vec!["x".into(), "keep".into()],
                vec!["p".into(), other_hex, "wss://other".into(), "friend".into()]
            ]
        );
    }

    #[test]
    fn already_requested_relationship_is_a_noop() {
        let author = Keys::generate();
        let target = Keys::generate();
        let target_hex = target.public_key().to_hex();
        let base = event(&author, 1, vec![vec!["p", &target_hex]], "");
        assert!(matches!(
            compose_follow_change(&base, target.public_key(), FollowChange::Follow,),
            Ok(ComposeFollowResult::NoChange)
        ));
        let empty = event(&author, 1, vec![], "");
        assert!(matches!(
            compose_follow_change(&empty, target.public_key(), FollowChange::Unfollow,),
            Ok(ComposeFollowResult::NoChange)
        ));
    }

    /// A base somebody else authored is composed without complaint and gets
    /// no error of its own: the precondition is checked at the editing
    /// identity's own coordinate, where a foreign event is never the winner,
    /// so it is unsatisfiable and reports through the ordinary conflict door
    /// at acceptance instead of a compose-time author comparison.
    #[test]
    fn a_foreign_base_composes_and_is_left_to_the_precondition() {
        let wrong = Keys::generate();
        let target = Keys::generate();
        let wrong_author = event(&wrong, 1, vec![], "");
        let ComposeFollowResult::Publish(intent) =
            compose_follow_change(&wrong_author, target.public_key(), FollowChange::Follow)
                .unwrap()
        else {
            panic!("must publish")
        };
        assert_eq!(expected_base(&intent), Some(Some(wrong_author.id())));
    }

    #[test]
    fn base_validation_fails_closed() {
        let author = Keys::generate();
        let target = Keys::generate();
        let contact_list = event(&author, 1, vec![], "");
        let wrong_kind = Row::from_parts(
            contact_list.id(),
            contact_list.pubkey(),
            contact_list.created_at(),
            Kind::TextNote,
            contact_list.tags().clone(),
            contact_list.content().to_owned(),
            contact_list.signature(),
            contact_list.sources.clone(),
        );
        assert_eq!(
            compose_follow_change(&wrong_kind, target.public_key(), FollowChange::Follow,).err(),
            Some(ComposeFollowError::BaseHasWrongKind)
        );
    }

    #[test]
    fn nostr_tag_reconstruction_keeps_raw_cells_exact() {
        let raw = vec![
            "unknown".to_string(),
            "01".to_string(),
            "1e+09".to_string(),
            "extra".to_string(),
        ];
        let reconstructed = Tag::parse(raw.clone()).expect("non-empty raw tag");
        assert_eq!(reconstructed.as_slice(), raw.as_slice());
    }

    #[test]
    fn semantic_operations_compose_in_order_and_preserve_unowned_fields() {
        let author = Keys::generate();
        let alice = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let remove_alice = encode_follow_operation(alice, FollowChange::Unfollow);
        let add_alice = encode_follow_operation(alice, FollowChange::Follow);
        let add_bob = encode_follow_operation(bob, FollowChange::Follow);
        let base = event(
            &author,
            10,
            vec![vec!["client", "keep"], vec!["x", "opaque", "cells"]],
            "opaque content survives",
        );
        let current = nostr::UnsignedEvent {
            id: Some(base.id()),
            pubkey: base.pubkey(),
            created_at: base.created_at(),
            kind: base.kind(),
            tags: base.tags().clone(),
            content: base.content().to_owned(),
        };

        let materialized = apply_follow_operations(
            &current,
            [
                add_alice.as_slice(),
                add_bob.as_slice(),
                remove_alice.as_slice(),
            ],
        )
        .unwrap();

        assert_eq!(materialized.content, "opaque content survives");
        let raw = materialized
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            raw,
            vec![
                vec!["client".to_string(), "keep".to_string()],
                vec!["x".to_string(), "opaque".to_string(), "cells".to_string()],
                vec!["p".to_string(), bob.to_hex()],
            ]
        );
    }

    #[test]
    fn semantic_operation_codec_is_versioned_and_closed() {
        let target = Keys::generate().public_key();
        let bytes = encode_follow_operation(target, FollowChange::Follow);
        assert_eq!(bytes.len(), FOLLOW_OPERATION_LEN);
        assert_eq!(
            decode_follow_operation(&bytes),
            Ok((target, FollowChange::Follow))
        );

        let mut wrong_version = bytes.clone();
        wrong_version[0] = 2;
        assert!(decode_follow_operation(&wrong_version).is_err());
        let mut wrong_change = bytes.clone();
        wrong_change[1] = 9;
        assert!(decode_follow_operation(&wrong_change).is_err());
        assert!(decode_follow_operation(&bytes[..bytes.len() - 1]).is_err());
    }
}
