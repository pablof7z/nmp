//! The Blossom blob identity: a sha256 digest with STRICT lowercase-hex
//! text form (#545). BUD-01 addresses every blob by the lowercase-hex
//! sha256 of its exact bytes; this newtype is the one place that hashing
//! and that strict text codec live, so an uppercase or truncated hash can
//! never silently round-trip into an `x` binding or a descriptor identity.

use nostr::hashes::sha256;
use nostr::hashes::Hash as _;

/// The sha256 digest of a blob's exact bytes -- Blossom's blob identity
/// (BUD-01). Constructed by hashing ([`Sha256Hash::of`]) or by STRICT
/// lowercase-hex parsing ([`Sha256Hash::from_hex`]); there is no lenient
/// path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Hash([u8; 32]);

/// [`Sha256Hash::from_hex`]'s failure modes. Exhaustive; each variant is
/// constructed by the strict parser and pinned by the unit tests below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sha256HexError {
    /// Not exactly 64 characters.
    BadLength { length: usize },
    /// A character outside `[0-9a-f]` -- BUD-11 mandates LOWERCASE hex, so
    /// uppercase digits are rejected rather than case-folded.
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
    /// Hash the EXACT blob bytes (BUD-01's blob identity).
    pub fn of(bytes: &[u8]) -> Self {
        Self(sha256::Hash::hash(bytes).to_byte_array())
    }

    /// The canonical lowercase-hex text form -- the value BUD-11 `x` tags
    /// and BUD-02 descriptors carry.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// STRICT parse: exactly 64 characters, lowercase `[0-9a-f]` only.
    /// Uppercase hex is a typed refusal, never case-folded -- a value the
    /// spec says is lowercase and arrives otherwise is evidence of a
    /// non-conforming (or tampering) peer, not something to repair.
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

    /// The raw digest bytes.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Invariant (#545): hashing produces the well-known sha256 test vector
    /// and round-trips through the strict hex codec unchanged.
    #[test]
    fn hash_of_empty_input_matches_the_known_vector_and_round_trips() {
        let empty = Sha256Hash::of(b"");
        let hex = empty.to_hex();
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(Sha256Hash::from_hex(&hex), Ok(empty));
    }

    /// Invariant (#545): BUD-11 mandates LOWERCASE hex -- an uppercase
    /// digest is a typed refusal, never silently case-folded.
    #[test]
    fn uppercase_hex_is_rejected_not_case_folded() {
        let upper = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
        assert_eq!(
            Sha256Hash::from_hex(upper),
            Err(Sha256HexError::NotLowercaseHex { character: 'E' })
        );
    }

    /// Invariant (#545): anything but exactly 64 characters is a typed
    /// length refusal, including the empty string and a 63/65-char slip.
    #[test]
    fn wrong_length_hex_is_rejected_with_the_observed_length() {
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

    /// Invariant (#545): a non-hex character anywhere in an otherwise
    /// well-shaped string is refused (including multi-byte characters,
    /// which the char-wise length count sees as one character each).
    #[test]
    fn non_hex_characters_are_rejected() {
        let with_g = "g3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            Sha256Hash::from_hex(with_g),
            Err(Sha256HexError::NotLowercaseHex { character: 'g' })
        );
    }
}
