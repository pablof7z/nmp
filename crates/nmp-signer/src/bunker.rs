//! Strict, secret-aware parsing for the remote-signer initiated NIP-46 URI.

use std::fmt;

use nostr::{PublicKey, RelayUrl};
use zeroize::Zeroizing;

/// A real bunker URI is a few hundred bytes. Bound attacker-controlled input
/// before URL/query allocation.
pub const MAX_BUNKER_URI_LEN: usize = 4 * 1024;
pub const MAX_NIP46_RELAYS: usize = 8;

/// Parsed `bunker://<remote-signer-key>?relay=...` connection token.
#[derive(Clone, PartialEq, Eq)]
pub struct BunkerUri {
    pub remote_signer_public_key: PublicKey,
    pub relays: Vec<RelayUrl>,
    pub secret: Option<Zeroizing<String>>,
}

impl fmt::Debug for BunkerUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BunkerUri")
            .field("remote_signer_public_key", &self.remote_signer_public_key)
            .field("relays", &self.relays)
            .field("secret", &self.secret.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BunkerParseError {
    Empty,
    TooLong(usize),
    WrongScheme,
    MissingRemoteSignerKey,
    InvalidRemoteSignerKey,
    MissingRelay,
    TooManyRelays(usize),
    InvalidRelay(String),
    Malformed(String),
}

impl fmt::Display for BunkerParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty bunker URI"),
            Self::TooLong(len) => write!(f, "bunker URI exceeds {MAX_BUNKER_URI_LEN} bytes: {len}"),
            Self::WrongScheme => f.write_str("URI scheme must be bunker"),
            Self::MissingRemoteSignerKey => f.write_str("bunker URI has no remote signer key"),
            Self::InvalidRemoteSignerKey => f.write_str("bunker URI remote signer key is invalid"),
            Self::MissingRelay => f.write_str("bunker URI requires at least one relay"),
            Self::TooManyRelays(count) => {
                write!(f, "bunker URI exceeds {MAX_NIP46_RELAYS} relays: {count}")
            }
            Self::InvalidRelay(relay) => write!(f, "invalid bunker relay: {relay}"),
            Self::Malformed(reason) => write!(f, "malformed bunker URI: {reason}"),
        }
    }
}

impl std::error::Error for BunkerParseError {}

/// Parse a current NIP-46 bunker token. Unknown parameters are ignored: they
/// are display hints/extensions, not inputs to signer policy.
pub fn parse_bunker_uri(input: &str) -> Result<BunkerUri, BunkerParseError> {
    if input.is_empty() {
        return Err(BunkerParseError::Empty);
    }
    if input.len() > MAX_BUNKER_URI_LEN {
        return Err(BunkerParseError::TooLong(input.len()));
    }
    let url = url::Url::parse(input).map_err(|e| BunkerParseError::Malformed(e.to_string()))?;
    if url.scheme() != "bunker" {
        return Err(BunkerParseError::WrongScheme);
    }

    let key = url
        .host_str()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let path = url.path().trim_matches('/');
            (!path.is_empty()).then_some(path)
        })
        .ok_or(BunkerParseError::MissingRemoteSignerKey)?;
    let remote_signer_public_key =
        PublicKey::from_hex(key).map_err(|_| BunkerParseError::InvalidRemoteSignerKey)?;

    let mut relays = Vec::new();
    let mut secret = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "relay" => {
                let relay = RelayUrl::parse(value.as_ref())
                    .map_err(|_| BunkerParseError::InvalidRelay(value.into_owned()))?;
                if !(relay.as_str().starts_with("ws://") || relay.as_str().starts_with("wss://")) {
                    return Err(BunkerParseError::InvalidRelay(relay.to_string()));
                }
                if !relays.contains(&relay) {
                    relays.push(relay);
                    if relays.len() > MAX_NIP46_RELAYS {
                        return Err(BunkerParseError::TooManyRelays(relays.len()));
                    }
                }
            }
            "secret" if !value.is_empty() => secret = Some(Zeroizing::new(value.into_owned())),
            _ => {}
        }
    }
    if relays.is_empty() {
        return Err(BunkerParseError::MissingRelay);
    }

    Ok(BunkerUri {
        remote_signer_public_key,
        relays,
        secret,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    #[test]
    fn parses_repeated_relays_and_redacts_secret() {
        let remote = Keys::generate().public_key();
        let uri = format!(
            "bunker://{}?relay=wss%3A%2F%2Fone.example&relay=ws%3A%2F%2F127.0.0.1%3A1234&secret=top-secret",
            remote.to_hex()
        );
        let parsed = parse_bunker_uri(&uri).unwrap();
        assert_eq!(parsed.remote_signer_public_key, remote);
        assert_eq!(parsed.relays.len(), 2);
        assert_eq!(
            parsed.secret.as_deref().map(String::as_str),
            Some("top-secret")
        );
        let debug = format!("{parsed:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("top-secret"));
    }

    #[test]
    fn rejects_missing_or_non_websocket_relays_and_oversize_input() {
        let remote = Keys::generate().public_key();
        assert_eq!(
            parse_bunker_uri(&format!("bunker://{}", remote.to_hex())),
            Err(BunkerParseError::MissingRelay)
        );
        assert!(matches!(
            parse_bunker_uri(&format!(
                "bunker://{}?relay=https%3A%2F%2Frelay.example",
                remote.to_hex()
            )),
            Err(BunkerParseError::InvalidRelay(_))
        ));
        assert!(matches!(
            parse_bunker_uri(&"x".repeat(MAX_BUNKER_URI_LEN + 1)),
            Err(BunkerParseError::TooLong(_))
        ));
        let relays = (0..=MAX_NIP46_RELAYS)
            .map(|i| format!("relay=wss%3A%2F%2Frelay-{i}.example"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(matches!(
            parse_bunker_uri(&format!("bunker://{}?{relays}", remote.to_hex())),
            Err(BunkerParseError::TooManyRelays(_))
        ));
    }
}
