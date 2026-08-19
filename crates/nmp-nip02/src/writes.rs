//! Durable NIP-02 follow/unfollow semantic operations over the current
//! account's kind:3 contact list.
//!
//! Moved back here from `nmp` by #1707: `nmp` must not know what a kind:3
//! contact list or a follow/unfollow edit means, only how to carry it
//! through custody. This module composes the ordinary
//! [`WriteIntent`](nmp::WriteIntent), freezes the selected account, and
//! enters the engine's receipt lifecycle over `nmp`'s own engine surface --
//! the same capability-owns-its-meaning shape every other protocol crate
//! now uses.

use nmp_grammar::{EventBuilder, Identity, WriteIntent, WriteRouting};
use nostr::{Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

use nmp::{
    RegisteredReplaceableMaterializer, ReplaceableMaterializer, ReplaceableMaterializerOperation,
    ReplaceableMaterializerRefusal, ReplaceableMaterializerSpec, Row,
};

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

/// Compiled NIP-02 write composer.
///
/// The handle names only this crate's program/format. Publishing through an
/// engine that was not constructed with [`follow_capability`] is refused
/// before custody. It exposes typed follow/unfollow composition, not replay
/// ids, opaque bytes, routing, or contributor membership.
#[derive(Clone, Copy)]
pub struct FollowWrites {
    registration: RegisteredReplaceableMaterializer,
}

/// The compiled NIP-02 capability that must be supplied before engine recovery.
#[must_use]
pub fn follow_capability() -> ReplaceableMaterializerSpec {
    ReplaceableMaterializerSpec::new(FOLLOW_PROGRAM, FOLLOW_FORMAT, FollowMaterializer)
}

/// Typed NIP-02 write constructor bound to [`follow_capability`].
#[must_use]
pub fn follow_writes() -> FollowWrites {
    FollowWrites {
        registration: follow_capability().handle(),
    }
}

impl FollowWrites {
    /// Compile one follow/unfollow action into the ordinary write noun.
    ///
    /// The selected `author` is explicit so an account switch after this call
    /// cannot retarget the operation. NMP applies the operation to its
    /// canonical source when one exists; otherwise this capability supplies
    /// the complete empty kind-3 value through [`FollowMaterializer`].
    ///
    /// Infallible: `Kind::ContactList` is always replaceable, the identifier
    /// is always empty (so the non-addressable-identifier refusal cannot
    /// trigger), and [`encode_follow_operation`] always produces exactly
    /// [`FOLLOW_OPERATION_LEN`] non-empty bytes, well under the operation
    /// size bound. [`RegisteredReplaceableMaterializer::first_value_operation`]
    /// has no other way to refuse this call's fixed shape.
    pub(crate) fn intent(
        &self,
        author: PublicKey,
        target: PublicKey,
        change: FollowChange,
    ) -> WriteIntent {
        let payload = self
            .registration
            .first_value_operation(
                Kind::ContactList,
                String::new(),
                encode_follow_operation(target, change),
            )
            .expect("Kind::ContactList with an empty identifier and a fixed non-empty operation is always accepted");
        WriteIntent {
            payload,
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
        }
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
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
        if source.kind != Kind::ContactList {
            return Err(ReplaceableMaterializerRefusal {
                reason: "NIP-02 materialization source is not a contact list".to_string(),
            });
        }

        apply_follow_operations(source, operations.iter().map(|operation| operation.bytes()))
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

