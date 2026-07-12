//! `decode_nostr_entity` -- the bech32 nostr-entity DECODE codec exported
//! over UniFFI (#116): `npub`/`nprofile`/`note`/`nevent`/`naddr` -> their
//! hex id/pubkey plus any embedded relay hints (and, for `naddr`, kind +
//! `d`-tag identifier). A top-level function, not a method on `NmpEngine`:
//! this needs no engine instance at all -- no network, no signing, pure
//! codec (`nmp::decode_nostr_entity`, itself a thin adapter over `nostr`'s
//! own `nip19`/`nip21` modules; see that function's doc for why `nsec`/
//! `ncryptsec` are refused rather than decoded).

use nmp::NostrEntity;

use crate::convert::FfiError;
use crate::types::FfiNostrEntity;

fn entity_to_ffi(entity: NostrEntity) -> FfiNostrEntity {
    match entity {
        NostrEntity::Pubkey { pubkey } => FfiNostrEntity::Pubkey { pubkey },
        NostrEntity::Profile { pubkey, relays } => FfiNostrEntity::Profile { pubkey, relays },
        NostrEntity::EventId { id } => FfiNostrEntity::EventId { id },
        NostrEntity::Event {
            id,
            author,
            kind,
            relays,
        } => FfiNostrEntity::Event {
            id,
            author,
            kind,
            relays,
        },
        NostrEntity::Coordinate {
            kind,
            author,
            identifier,
            relays,
        } => FfiNostrEntity::Coordinate {
            kind,
            author,
            identifier,
            relays,
        },
    }
}

impl From<nmp::NostrEntityError> for FfiError {
    fn from(err: nmp::NostrEntityError) -> Self {
        match err {
            nmp::NostrEntityError::Malformed { reason } => Self::InvalidNostrEntity { reason },
            nmp::NostrEntityError::SecretKeyRejected => Self::NostrEntitySecretKeyRejected,
        }
    }
}

/// Decode a bech32 nostr entity -- `npub`/`nprofile`/`note`/`nevent`/
/// `naddr` -- accepting either the bare bech32 string or a `nostr:`-
/// prefixed URI (NIP-21). Pure codec: no network, no signing, no engine
/// state, callable with no `NmpEngine` constructed at all.
#[uniffi::export]
pub fn decode_nostr_entity(input: String) -> Result<FfiNostrEntity, FfiError> {
    Ok(entity_to_ffi(nmp::decode_nostr_entity(&input)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_bare_npub_to_the_ffi_pubkey_variant() {
        let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";
        assert_eq!(
            decode_nostr_entity(npub.to_string()),
            Ok(FfiNostrEntity::Pubkey {
                pubkey: "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4"
                    .to_string(),
            })
        );
    }

    #[test]
    fn decodes_a_nostr_uri_prefixed_entity_identically_to_the_bare_form() {
        let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";
        let uri = format!("nostr:{npub}");
        assert_eq!(
            decode_nostr_entity(uri),
            decode_nostr_entity(npub.to_string())
        );
    }

    #[test]
    fn decodes_an_naddr_to_the_ffi_coordinate_variant() {
        let naddr = "naddr1qqxnzd3exgersv33xymnsve3qgs8suecw4luyht9ekff89x4uacneapk8r5dyk0gmn6uwwurf6u9rusrqsqqqa282m3gxt";
        assert_eq!(
            decode_nostr_entity(naddr.to_string()),
            Ok(FfiNostrEntity::Coordinate {
                kind: 30023,
                author: "787338757fc25d65cd929394d5e7713cf43638e8d259e8dcf5c73b834eb851f2"
                    .to_string(),
                identifier: "1692282117831".to_string(),
                relays: Vec::new(),
            })
        );
    }

    #[test]
    fn rejects_an_nsec_with_a_typed_error_never_a_panic() {
        let nsec = "nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99";
        assert_eq!(
            decode_nostr_entity(nsec.to_string()),
            Err(FfiError::NostrEntitySecretKeyRejected)
        );
    }

    #[test]
    fn rejects_malformed_bech32_with_a_typed_error_never_a_panic() {
        match decode_nostr_entity("npub1notvalidbech32".to_string()) {
            Err(FfiError::InvalidNostrEntity { .. }) => {}
            other => panic!("expected InvalidNostrEntity, got {other:?}"),
        }
    }
}
