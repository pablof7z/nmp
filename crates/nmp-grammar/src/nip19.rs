//! Bech32 nostr-entity DECODE (#116): `npub`/`nprofile`/`note`/`nevent`/
//! `naddr` -> their hex id/pubkey plus any embedded relay hints (and, for
//! `naddr`, kind + `d`-tag identifier). A thin adapter over `nostr`'s own
//! `nip19`/`nip21` modules (`Nip19::from_bech32`, `Nip21`'s closed
//! public-entity vocabulary) — no scratch bech32, no hand-rolled TLV
//! parsing (memory rule: use rust-nostr, not scratch crypto).
//!
//! Decode-only: encoding (hex -> `npub`) already exists via `nostr::
//! ToBech32` at existing call sites (`add_account`'s nsec path, `nmp/src/
//! engine.rs`'s own tests) and is out of scope here.

use nostr::nips::nip19::{FromBech32, Nip19};
use nostr::nips::nip21::Nip21;

/// A decoded public NIP-19 nostr entity (#116). Each variant carries EXACTLY
/// the fields NIP-19 defines for that entity — never force-fit into one
/// shared shape: `npub`/`note` carry no relay hints at all (the format has
/// none to carry); `nevent`'s `author`/`kind` are independently optional
/// metadata, never implied by the id alone; `naddr`'s `kind`/`author`/
/// `identifier` are ALL required by the format, unlike `nevent`'s.
///
/// Deliberately excludes `nsec`/`ncryptsec` — a secret-key entity is never a
/// valid decode target for a display/mention codec; see [`decode`]'s doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NostrEntity {
    /// `npub` — a bare public key. No relay hints (the format carries
    /// none).
    Pubkey { pubkey: String },
    /// `nprofile` — a public key plus zero or more relay hints.
    Profile { pubkey: String, relays: Vec<String> },
    /// `note` — a bare event id. No relay hints (the format carries none).
    EventId { id: String },
    /// `nevent` — an event id plus OPTIONAL author and/or kind (NIP-19: both
    /// are independently optional metadata, never implied by the id alone),
    /// plus zero or more relay hints.
    Event {
        id: String,
        author: Option<String>,
        kind: Option<u16>,
        relays: Vec<String>,
    },
    /// `naddr` — a parameterized-replaceable-event coordinate: `kind` +
    /// `author` + `d`-tag `identifier` (all REQUIRED by the format, unlike
    /// `nevent`'s optional author/kind), plus zero or more relay hints.
    Coordinate {
        kind: u16,
        author: String,
        identifier: String,
        relays: Vec<String>,
    },
}

/// Every way [`decode`] can fail — typed states, never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NostrEntityError {
    /// Malformed bech32 (bad checksum/charset), an unrecognized HRP prefix,
    /// or well-formed-bech32-but-malformed TLV payload (`nprofile`/
    /// `nevent`/`naddr`'s own inner encoding).
    Malformed { reason: String },
    /// The input decoded to `nsec`/`ncryptsec` — refused rather than
    /// returned, since a secret-key entity is never a valid decode target
    /// for a display/mention codec (mirrors `nostr::nips::nip21::Nip21`'s
    /// own closed public-entity vocabulary, which excludes both secret-key
    /// variants for the identical reason).
    SecretKeyRejected,
}

impl std::fmt::Display for NostrEntityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed { reason } => write!(f, "malformed nostr entity: {reason}"),
            Self::SecretKeyRejected => write!(f, "refusing to decode a secret-key entity"),
        }
    }
}

impl std::error::Error for NostrEntityError {}

/// Decode a bech32 nostr entity — `npub`/`nprofile`/`note`/`nevent`/
/// `naddr` — accepting either the bare bech32 string or a `nostr:`-prefixed
/// URI (NIP-21). Pure codec: no network, no signing, no engine state.
pub fn decode(input: &str) -> Result<NostrEntity, NostrEntityError> {
    let bech32 = input.strip_prefix("nostr:").unwrap_or(input);
    let nip19 = Nip19::from_bech32(bech32).map_err(|e| NostrEntityError::Malformed {
        reason: e.to_string(),
    })?;
    // `Nip21::try_from(Nip19)`'s only error path is the secret-key variants
    // (its own impl: `Nip19::Secret`/`EncryptedSecret` -> `Err`, every other
    // variant -> `Ok`) — precise to map directly, not a catch-all.
    let nip21 = Nip21::try_from(nip19).map_err(|_| NostrEntityError::SecretKeyRejected)?;
    Ok(match nip21 {
        Nip21::Pubkey(pk) => NostrEntity::Pubkey {
            pubkey: pk.to_hex(),
        },
        Nip21::Profile(profile) => NostrEntity::Profile {
            pubkey: profile.public_key.to_hex(),
            relays: profile.relays.iter().map(ToString::to_string).collect(),
        },
        Nip21::EventId(id) => NostrEntity::EventId { id: id.to_hex() },
        Nip21::Event(event) => NostrEntity::Event {
            id: event.event_id.to_hex(),
            author: event.author.map(|pk| pk.to_hex()),
            kind: event.kind.map(|k| k.as_u16()),
            relays: event.relays.iter().map(ToString::to_string).collect(),
        },
        Nip21::Coordinate(coord) => NostrEntity::Coordinate {
            kind: coord.kind.as_u16(),
            author: coord.public_key.to_hex(),
            identifier: coord.identifier.clone(),
            relays: coord.relays.iter().map(ToString::to_string).collect(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_bare_npub() {
        let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";
        let entity = decode(npub).expect("valid npub must decode");
        assert_eq!(
            entity,
            NostrEntity::Pubkey {
                pubkey: "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4"
                    .to_string(),
            }
        );
    }

    #[test]
    fn decodes_a_nostr_uri_prefixed_npub_identically_to_the_bare_form() {
        let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";
        let uri = format!("nostr:{npub}");
        assert_eq!(decode(&uri).unwrap(), decode(npub).unwrap());
    }

    #[test]
    fn decodes_a_bare_note_event_id_with_no_relay_hints() {
        let note = "note1m99r7nwc0wdrkzldrqan96gklg5usqspq7z9696j6unf0ljnpxjspqfw99";
        let entity = decode(note).expect("valid note must decode");
        assert_eq!(
            entity,
            NostrEntity::EventId {
                id: "d94a3f4dd87b9a3b0bed183b32e916fa29c8020107845d1752d72697fe5309a5".to_string(),
            }
        );
    }

    #[test]
    fn decodes_an_nprofile_with_relay_hints() {
        let nprofile = "nprofile1qqsrhuxx8l9ex335q7he0f09aej04zpazpl0ne2cgukyawd24mayt8gppemhxue69uhhytnc9e3k7mf0qyt8wumn8ghj7er2vfshxtnnv9jxkc3wvdhk6tclr7lsh";
        let entity = decode(nprofile).expect("valid nprofile must decode");
        assert_eq!(
            entity,
            NostrEntity::Profile {
                pubkey: "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
                    .to_string(),
                relays: vec![
                    "wss://r.x.com/".to_string(),
                    "wss://djbas.sadkb.com/".to_string(),
                ],
            }
        );
    }

    /// #116's honest-modeling requirement: `nevent`'s `author`/`kind` are
    /// independently optional, never force-fit -- this fixture has an
    /// author but no kind and no relays.
    #[test]
    fn decodes_an_nevent_with_an_author_but_no_kind_or_relays() {
        let nevent = "nevent1qqsdhet4232flykq3048jzc9msmaa3hnxuesxy3lnc33vd0wt9xwk6szyqewrqnkx4zsaweutf739s0cu7et29zrntqs5elw70vlm8zudr3y24sqsgy";
        let entity = decode(nevent).expect("valid nevent must decode");
        assert_eq!(
            entity,
            NostrEntity::Event {
                id: "dbe57554549f92c08bea790b05dc37dec6f3373303123f9e231635ee594ceb6a".to_string(),
                author: Some(
                    "32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245".to_string()
                ),
                kind: None,
                relays: Vec::new(),
            }
        );
    }

    #[test]
    fn decodes_an_naddr_with_kind_author_and_identifier() {
        let naddr = "naddr1qqxnzd3exgersv33xymnsve3qgs8suecw4luyht9ekff89x4uacneapk8r5dyk0gmn6uwwurf6u9rusrqsqqqa282m3gxt";
        let entity = decode(naddr).expect("valid naddr must decode");
        assert_eq!(
            entity,
            NostrEntity::Coordinate {
                kind: 30023,
                author: "787338757fc25d65cd929394d5e7713cf43638e8d259e8dcf5c73b834eb851f2"
                    .to_string(),
                identifier: "1692282117831".to_string(),
                relays: Vec::new(),
            }
        );
    }

    /// The refusal case: a secret-key entity must never decode as if it
    /// were a display target.
    #[test]
    fn rejects_an_nsec_rather_than_leaking_the_secret_key() {
        let nsec = "nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99";
        assert_eq!(decode(nsec), Err(NostrEntityError::SecretKeyRejected));
    }

    #[test]
    fn rejects_malformed_bech32_with_a_typed_error_never_a_panic() {
        match decode("npub1notvalidbech32") {
            Err(NostrEntityError::Malformed { .. }) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn rejects_an_unrecognized_hrp_prefix() {
        match decode("nzzz1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq") {
            Err(NostrEntityError::Malformed { .. }) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
