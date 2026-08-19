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

