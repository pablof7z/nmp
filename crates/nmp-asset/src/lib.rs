//! Protocol-neutral exact-byte asset identity.
//!
//! [`Sha256Hash`] is a digest value and may be parsed from an untrusted wire
//! claim. [`VerifiedAsset`] is different: its private digest is computed from
//! exact bytes supplied to [`VerifiedAsset::from_bytes`]. A claimed hash,
//! descriptor, URL, or decoded event cannot mint the witness.

use sha2::{Digest, Sha256};

/// The SHA-256 digest of exact bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Hash([u8; 32]);

/// [`Sha256Hash::from_hex`]'s exhaustive failure modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sha256HexError {
    /// Not exactly 64 characters.
    BadLength { length: usize },
    /// A character outside lowercase `[0-9a-f]`.
    NotLowercaseHex { character: char },
}

impl std::fmt::Display for Sha256HexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadLength { length } => {
                write!(f, "sha256 hex must be exactly 64 characters, got {length}")
            }
            Self::NotLowercaseHex { character } => write!(
                f,
                "sha256 hex must be lowercase [0-9a-f], got {character:?}"
            ),
        }
    }
}

impl std::error::Error for Sha256HexError {}

impl Sha256Hash {
    /// Hash the exact supplied bytes.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// The canonical lowercase-hex representation.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Parse exactly 64 lowercase hexadecimal characters.
    ///
    /// Parsing proves syntax only. It does not produce a [`VerifiedAsset`].
    pub fn from_hex(hex: &str) -> Result<Self, Sha256HexError> {
        let characters: Vec<char> = hex.chars().collect();
        if characters.len() != 64 {
            return Err(Sha256HexError::BadLength {
                length: characters.len(),
            });
        }
        let mut bytes = [0u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let hi = nibble(characters[2 * index])?;
            let lo = nibble(characters[2 * index + 1])?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn nibble(character: char) -> Result<u8, Sha256HexError> {
    match character {
        '0'..='9' => Ok(character as u8 - b'0'),
        'a'..='f' => Ok(character as u8 - b'a' + 10),
        _ => Err(Sha256HexError::NotLowercaseHex { character }),
    }
}

/// Proof that [`Self::sha256`] was computed from exact bytes observed by the
/// constructor.
///
/// URL and MIME are deliberately carried as untrusted presentation/wire
/// values. The witness proves byte identity, not that a URL currently serves
/// those bytes or that a MIME label is truthful.
///
/// A parsed or otherwise caller-claimed digest cannot mint the witness:
///
/// ```compile_fail,E0451
/// use nmp_asset::{Sha256Hash, VerifiedAsset};
///
/// let claimed = Sha256Hash::from_hex(
///     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
/// ).unwrap();
/// let forged = VerifiedAsset {
///     sha256: claimed,
///     byte_len: 0,
///     url: "https://example.invalid/claim".to_string(),
///     mime_type: Some("image/png".to_string()),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAsset {
    sha256: Sha256Hash,
    byte_len: usize,
    url: String,
    mime_type: Option<String>,
}

impl VerifiedAsset {
    /// Compute the witness from exact bytes.
    pub fn from_bytes(bytes: &[u8], url: String, mime_type: Option<String>) -> Self {
        Self {
            sha256: Sha256Hash::of(bytes),
            byte_len: bytes.len(),
            url,
            mime_type,
        }
    }

    pub fn sha256(&self) -> Sha256Hash {
        self.sha256
    }

    pub fn byte_len(&self) -> usize {
        self.byte_len
    }

    /// Untrusted locator text associated with the verified byte identity.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Untrusted MIME text associated with the verified byte identity.
    pub fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_known_vector_and_strict_hex_round_trips() {
        let empty = Sha256Hash::of(b"");
        let hex = empty.to_hex();
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(Sha256Hash::from_hex(&hex), Ok(empty));
    }

    /// Carried verbatim from the deleted `nmp-blossom::sha256` suite (#545):
    /// anything but exactly 64 characters is a typed length refusal,
    /// including the empty string and a 63/65-character slip.
    #[test]
    fn wrong_length_hex_is_a_typed_refusal_carrying_the_observed_length() {
        assert_eq!(
            Sha256Hash::from_hex(""),
            Err(Sha256HexError::BadLength { length: 0 })
        );
        let short = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85";
        assert_eq!(
            Sha256Hash::from_hex(short),
            Err(Sha256HexError::BadLength { length: 63 })
        );
        let long = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8555";
        assert_eq!(
            Sha256Hash::from_hex(long),
            Err(Sha256HexError::BadLength { length: 65 })
        );
    }

    /// Carried verbatim from the deleted `nmp-blossom::sha256` suite (#545):
    /// a claim outside lowercase `[0-9a-f]` is refused, never case-folded or
    /// repaired -- a value the wire says is lowercase arriving otherwise is
    /// evidence of a non-conforming peer.
    #[test]
    fn non_lowercase_hex_is_a_typed_refusal_never_case_folded() {
        assert_eq!(
            Sha256Hash::from_hex(
                "E3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            ),
            Err(Sha256HexError::NotLowercaseHex { character: 'E' })
        );
        let upper = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
        assert_eq!(
            Sha256Hash::from_hex(upper),
            Err(Sha256HexError::NotLowercaseHex { character: 'E' })
        );
        let with_g = "g3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            Sha256Hash::from_hex(with_g),
            Err(Sha256HexError::NotLowercaseHex { character: 'g' })
        );
    }

    #[test]
    fn witness_identity_is_derived_from_exact_bytes_not_locator_text() {
        let first = VerifiedAsset::from_bytes(
            b"first",
            "https://cdn.example/asset".to_string(),
            Some("image/png".to_string()),
        );
        let second = VerifiedAsset::from_bytes(
            b"second",
            "https://cdn.example/asset".to_string(),
            Some("image/png".to_string()),
        );
        assert_ne!(first.sha256(), second.sha256());
        assert_eq!(first.byte_len(), 5);
        assert_eq!(first.url(), "https://cdn.example/asset");
    }
}
