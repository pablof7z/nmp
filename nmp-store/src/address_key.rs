//! The replaceable/addressable keying rule (bug-class ledger #1 harvested
//! semantics, M1 plan §2.2): replaceable kinds `{0,3,10000..=19999}` are
//! keyed `(pubkey,kind)`; addressable kinds `{30000..=39999}` are keyed
//! `(pubkey,kind,d-tag)`. Everything else has no address (a "regular"
//! event: no supersession, only id-dedup applies).

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
