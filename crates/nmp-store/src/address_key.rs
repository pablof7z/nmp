//! The replaceable/addressable keying rule (bug-class ledger #1 harvested
//! semantics, M1 plan §2.2): replaceable kinds `{0,3,10000..=19999}` are
//! keyed `(pubkey,kind)`; addressable kinds `{30000..=39999}` are keyed
//! `(pubkey,kind,d-tag)`. Everything else has no address (a "regular"
//! event: no supersession, only id-dedup applies).

use std::cmp::Ordering;

use nostr::nips::nip01::Coordinate;
use nostr::{Event, Kind, PublicKey};

/// The replaceable/addressable competition key for an event, or `None` if
/// the event's kind is neither replaceable nor addressable ("regular" —
/// every insert of a distinct id is `Inserted`, never `Stale`/`Superseded`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum AddressKey {
    /// `(pubkey, kind)` — replaceable kinds `{0, 3, 10000..=19999}`.
    Replaceable(PublicKey, Kind),
    /// `(pubkey, kind, d-tag)` — addressable kinds `{30000..=39999}`. A
    /// missing `d` tag is treated as the empty-string identifier (NIP-01).
    Addressable(PublicKey, Kind, String),
}

/// The exact M1 replaceable set: `{0, 3, 10000..=19999}`.
///
/// Deliberately NOT `nostr::Kind::is_replaceable`, which additionally
/// treats `ChannelMetadata` (kind 41, NIP-28) as replaceable — a real Nostr
/// nuance, but one the M1 plan does not include in its explicit set. Using
/// our own predicate keeps `nmp-store`'s supersession rule exactly as
/// specified rather than silently widened by upstream's superset.
fn is_replaceable_kind(kind: u16) -> bool {
    kind == 0 || kind == 3 || (10_000..=19_999).contains(&kind)
}

/// The exact M1 addressable set: `{30000..=39999}`.
fn is_addressable_kind(kind: u16) -> bool {
    (30_000..=39_999).contains(&kind)
}

/// Compute the address key for `event`, if any.
pub(crate) fn address_key_for(event: &Event) -> Option<AddressKey> {
    let kind_num = event.kind.as_u16();
    if is_replaceable_kind(kind_num) {
        Some(AddressKey::Replaceable(event.pubkey, event.kind))
    } else if is_addressable_kind(kind_num) {
        let d = event.tags.identifier().unwrap_or("").to_string();
        Some(AddressKey::Addressable(event.pubkey, event.kind, d))
    } else {
        None
    }
}

/// Compute the address key a NIP-09 `a`-tag `coordinate` names, or `None` if
/// its kind is neither replaceable nor addressable (a malformed/pointless
/// `a`-tag: NIP-01 coordinates are only meaningful for those two kinds).
/// Deliberately independent of any stored event's tags — an `a`-tag carries
/// its own `(kind, pubkey, d-tag)` triple, which is exactly `AddressKey`'s
/// shape, so this never needs to look anything up.
pub(crate) fn address_key_for_coordinate(coord: &Coordinate) -> Option<AddressKey> {
    let kind_num = coord.kind.as_u16();
    if is_replaceable_kind(kind_num) {
        Some(AddressKey::Replaceable(coord.public_key, coord.kind))
    } else if is_addressable_kind(kind_num) {
        Some(AddressKey::Addressable(
            coord.public_key,
            coord.kind,
            coord.identifier.clone(),
        ))
    } else {
        None
    }
}

impl AddressKey {
    /// A canonical, unambiguous string encoding used ONLY as `RedbStore`'s
    /// `addr_index` table key. `MemoryStore` keys directly off the enum via
    /// `Hash`/`Eq` and never calls this — it exists purely because `redb`
    /// table keys need a byte-encodable type, and `&str`/`String` (already
    /// `Key`/`Value` in `redb`) is the simplest fit for one.
    ///
    /// `\0` (NUL) separates fields: valid pubkey-hex/kind-decimal segments
    /// never contain NUL, so those two segments can never collide across
    /// keys. A `d`-tag value containing a stray NUL could in principle
    /// produce a segment-boundary collision with another address — but that
    /// only risks an extra, conservative supersession comparison (`redb`'s
    /// `insert` still re-checks `candidate_wins` against whatever the
    /// collided key currently holds), never silent data loss, since
    /// dedup-by-id always runs first regardless.
    pub(crate) fn to_redb_key(&self) -> String {
        match self {
            AddressKey::Replaceable(pk, kind) => {
                format!("R\0{}\0{}", pk.to_hex(), kind.as_u16())
            }
            AddressKey::Addressable(pk, kind, d) => {
                format!("A\0{}\0{}\0{}", pk.to_hex(), kind.as_u16(), d)
            }
        }
    }
}

/// True iff `candidate` wins over `current` for the same
/// replaceable/addressable address: newest `created_at` wins; on a
/// `created_at` tie, the lexicographically-smallest id wins. Shared by both
/// `MemoryStore` and `RedbStore` so supersession semantics can never diverge
/// between the oracle and the persistent backend.
pub(crate) fn candidate_wins(candidate: &Event, current: &Event) -> bool {
    match candidate.created_at.cmp(&current.created_at) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => candidate.id < current.id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Tag};

    fn signed(kind: Kind, tags: Vec<Tag>) -> Event {
        let keys = Keys::generate();
        EventBuilder::new(kind, "")
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap()
    }

    #[test]
    fn metadata_and_contact_list_are_replaceable_keyed_by_pubkey_kind() {
        let e0 = signed(Kind::Metadata, vec![]);
        let e3 = signed(Kind::ContactList, vec![]);
        match address_key_for(&e0) {
            Some(AddressKey::Replaceable(pk, k)) => {
                assert_eq!(pk, e0.pubkey);
                assert_eq!(k, Kind::Metadata);
            }
            other => panic!("expected Replaceable, got {other:?}"),
        }
        match address_key_for(&e3) {
            Some(AddressKey::Replaceable(pk, k)) => {
                assert_eq!(pk, e3.pubkey);
                assert_eq!(k, Kind::ContactList);
            }
            other => panic!("expected Replaceable, got {other:?}"),
        }
    }

    #[test]
    fn parameterized_replaceable_range_is_keyed_by_pubkey_kind() {
        let e = signed(Kind::from(10_002u16), vec![]);
        assert_eq!(
            address_key_for(&e),
            Some(AddressKey::Replaceable(e.pubkey, Kind::from(10_002u16)))
        );
    }

    #[test]
    fn addressable_range_is_keyed_by_pubkey_kind_d_tag() {
        let e = signed(Kind::from(30_003u16), vec![Tag::identifier("g1")]);
        assert_eq!(
            address_key_for(&e),
            Some(AddressKey::Addressable(
                e.pubkey,
                Kind::from(30_003u16),
                "g1".to_string()
            ))
        );
    }

    #[test]
    fn addressable_without_d_tag_defaults_to_empty_identifier() {
        let e = signed(Kind::from(30_003u16), vec![]);
        assert_eq!(
            address_key_for(&e),
            Some(AddressKey::Addressable(
                e.pubkey,
                Kind::from(30_003u16),
                String::new()
            ))
        );
    }

    #[test]
    fn regular_kind_has_no_address_key() {
        let e = signed(Kind::TextNote, vec![]);
        assert_eq!(address_key_for(&e), None);
    }

    #[test]
    fn channel_metadata_is_not_treated_as_replaceable_in_m1() {
        // Deliberate deviation from nostr::Kind::is_replaceable(): the M1
        // plan's explicit set is {0,3,10000..=19999}, which excludes
        // ChannelMetadata (kind 41).
        let e = signed(Kind::ChannelMetadata, vec![]);
        assert_eq!(address_key_for(&e), None);
    }
}
