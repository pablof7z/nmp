use nmp::{
    Engine, EventBuilder, Identity, RegisteredReplaceableMaterializer, ReplaceableMaterializer,
    ReplaceableMaterializerOperation, ReplaceableMaterializerRefusal, ReplaceableSourcePolicy, Row,
    WriteIntent, WriteRouting,
};
use nostr::{Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

/// The requested relationship after a NIP-02 edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowChange {
    Follow,
    Unfollow,
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
#[derive(Clone)]
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
    /// Compile one follow/unfollow action into the ordinary write noun.
    ///
    /// The selected `author` is explicit so an account switch after this call
    /// cannot retarget the operation. NMP applies the operation to its
    /// canonical source when one exists; otherwise this capability supplies
    /// the complete empty kind-3 value through [`FollowMaterializer`].
    pub(crate) fn intent(
        &self,
        author: PublicKey,
        target: PublicKey,
        change: FollowChange,
    ) -> Result<WriteIntent, ()> {
        let payload = self
            .registration
            .first_value_operation(
                Kind::ContactList,
                String::new(),
                ReplaceableSourcePolicy::Continuing,
                encode_follow_operation(target, change),
            )
            .map_err(|_| ())?;
        Ok(WriteIntent {
            payload,
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
            correlation: None,
        })
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
        coordinate: &nostr::nips::nip01::Coordinate,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
        if coordinate.kind != Kind::ContactList || !coordinate.identifier.is_empty() {
            return Err(ReplaceableMaterializerRefusal {
                reason: "NIP-02 first-value coordinate is not a contact list".to_string(),
            });
        }
        let empty = UnsignedEvent::new(
            coordinate.public_key,
            Timestamp::from(0),
            Kind::ContactList,
            Vec::new(),
            String::new(),
        );
        apply_follow_operations(&empty, operations.iter().map(|operation| operation.bytes()))
            .map_err(|reason| ReplaceableMaterializerRefusal { reason })
    }
}

/// True when `event` contains any NIP-02 `p` tag for `target`. Relay hints,
/// petnames, extra tag fields, malformed unrelated tags, and ordering are
/// deliberately irrelevant to membership and remain untouched by edits.
pub fn follows(event: &Row, target: PublicKey) -> bool {
    let target_hex = target.to_hex();
    event.tags().iter().any(|tag| {
        let row = tag.as_slice();
        row.first().is_some_and(|cell| cell == "p")
            && row.get(1).is_some_and(|cell| cell == &target_hex)
    })
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
