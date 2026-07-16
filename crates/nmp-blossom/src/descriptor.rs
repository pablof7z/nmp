//! BUD-02 blob descriptor vocabulary and its strict, bounded parser
//! (#545). A descriptor is the server's claim about a stored blob; the
//! client treats it as UNTRUSTED input -- parsed strictly, bounded in
//! size, and (in `client.rs`) cross-checked against the locally computed
//! sha256 before it is ever surfaced as a [`crate::VerifiedUpload`].

use serde::Deserialize;

use crate::sha256::{Sha256Hash, Sha256HexError};

/// Standalone fail-closed bound on descriptor JSON. The upload client
/// already caps response bytes before parsing, but this parser must remain
/// safe when called with bytes from any other source, so it re-refuses
/// oversized input itself.
pub const MAX_DESCRIPTOR_BYTES: usize = 65536;

/// A parsed BUD-02 blob descriptor. `url`/`sha256`/`size` are mandatory
/// per BUD-02; `type` (MIME) and `uploaded` (unix seconds) are tolerated
/// as absent because real servers omit them. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobDescriptor {
    pub url: String,
    pub sha256: Sha256Hash,
    pub size: u64,
    pub mime_type: Option<String>,
    pub uploaded: Option<u64>,
}

/// [`BlobDescriptor::parse_json`]'s failure modes. Exhaustive; every
/// variant is pinned by the unit tests below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
    /// Input exceeds [`MAX_DESCRIPTOR_BYTES`] -- refused before any parse.
    TooLarge { limit_bytes: usize },
    /// Not a JSON object of the expected field shapes (includes non-JSON
    /// bytes and type mismatches such as a string `size`).
    Json { reason: String },
    /// The mandatory `url` field is absent (or JSON `null`).
    MissingUrl,
    /// The mandatory `sha256` field is absent (or JSON `null`).
    MissingSha256,
    /// The mandatory `size` field is absent (or JSON `null`).
    MissingSize,
    /// `sha256` is present but not 64 lowercase hex characters.
    BadSha256(Sha256HexError),
}

impl std::fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { limit_bytes } => {
                write!(f, "blob descriptor exceeds {limit_bytes} bytes")
            }
            Self::Json { reason } => write!(f, "blob descriptor is not valid JSON: {reason}"),
            Self::MissingUrl => f.write_str("blob descriptor is missing the mandatory `url`"),
            Self::MissingSha256 => {
                f.write_str("blob descriptor is missing the mandatory `sha256`")
            }
            Self::MissingSize => f.write_str("blob descriptor is missing the mandatory `size`"),
            Self::BadSha256(error) => write!(f, "blob descriptor `sha256` is invalid: {error}"),
        }
    }
}

impl std::error::Error for DescriptorError {}

/// The permissive wire shape: every field optional so absence becomes a
/// TYPED refusal below instead of an undifferentiated serde error, and
/// unknown fields are ignored (BUD-02 servers may add fields freely).
#[derive(Deserialize)]
struct RawDescriptor {
    url: Option<String>,
    sha256: Option<String>,
    size: Option<u64>,
    #[serde(rename = "type")]
    mime_type: Option<String>,
    uploaded: Option<u64>,
}

impl BlobDescriptor {
    /// Strict, bounded parse of a BUD-02 descriptor JSON document.
    pub fn parse_json(bytes: &[u8]) -> Result<Self, DescriptorError> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(DescriptorError::TooLarge {
                limit_bytes: MAX_DESCRIPTOR_BYTES,
            });
        }
        let raw: RawDescriptor = serde_json::from_slice(bytes)
            .map_err(|error| DescriptorError::Json {
                reason: error.to_string(),
            })?;
        let url = raw.url.ok_or(DescriptorError::MissingUrl)?;
        let sha256_hex = raw.sha256.ok_or(DescriptorError::MissingSha256)?;
        let sha256 = Sha256Hash::from_hex(&sha256_hex).map_err(DescriptorError::BadSha256)?;
        let size = raw.size.ok_or(DescriptorError::MissingSize)?;
        Ok(Self {
            url,
            sha256,
            size,
            mime_type: raw.mime_type,
            uploaded: raw.uploaded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_of(bytes: &[u8]) -> String {
        Sha256Hash::of(bytes).to_hex()
    }

    /// Invariant (#545): a complete descriptor parses, unknown fields are
    /// ignored, and optional `type`/`uploaded` absence is tolerated.
    #[test]
    fn complete_descriptor_parses_and_unknown_fields_are_ignored() {
        let hex = hex_of(b"blob");
        let json = format!(
            r#"{{"url":"https://cdn.example.com/{hex}","sha256":"{hex}","size":4,"type":"text/plain","uploaded":1700000000,"nip94":["ignored"]}}"#
        );
        let descriptor = BlobDescriptor::parse_json(json.as_bytes()).expect("full descriptor");
        assert_eq!(descriptor.sha256, Sha256Hash::of(b"blob"));
        assert_eq!(descriptor.size, 4);
        assert_eq!(descriptor.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(descriptor.uploaded, Some(1700000000));

        let minimal = format!(r#"{{"url":"https://cdn.example.com/{hex}","sha256":"{hex}","size":4}}"#);
        let descriptor = BlobDescriptor::parse_json(minimal.as_bytes()).expect("minimal");
        assert_eq!(descriptor.mime_type, None);
        assert_eq!(descriptor.uploaded, None);
    }

    /// Invariant (#545): each mandatory field's absence is its OWN typed
    /// refusal -- callers can distinguish which server claim is missing.
    #[test]
    fn each_missing_mandatory_field_is_a_distinct_typed_refusal() {
        let hex = hex_of(b"blob");
        let no_url = format!(r#"{{"sha256":"{hex}","size":4}}"#);
        assert_eq!(
            BlobDescriptor::parse_json(no_url.as_bytes()),
            Err(DescriptorError::MissingUrl)
        );
        let no_sha = r#"{"url":"https://cdn.example.com/x","size":4}"#;
        assert_eq!(
            BlobDescriptor::parse_json(no_sha.as_bytes()),
            Err(DescriptorError::MissingSha256)
        );
        let no_size = format!(r#"{{"url":"https://cdn.example.com/{hex}","sha256":"{hex}"}}"#);
        assert_eq!(
            BlobDescriptor::parse_json(no_size.as_bytes()),
            Err(DescriptorError::MissingSize)
        );
    }

    /// Invariant (#545): a malformed sha256 wraps the strict hex parser's
    /// typed error; non-JSON bytes are a `Json` refusal.
    #[test]
    fn bad_sha256_and_non_json_are_typed_refusals() {
        let bad_hex = r#"{"url":"https://cdn.example.com/x","sha256":"NOT-HEX","size":4}"#;
        assert!(matches!(
            BlobDescriptor::parse_json(bad_hex.as_bytes()),
            Err(DescriptorError::BadSha256(_))
        ));
        assert!(matches!(
            BlobDescriptor::parse_json(b"not-json"),
            Err(DescriptorError::Json { .. })
        ));
    }

    /// Invariant (#545): the parser is fail-closed standalone -- input
    /// larger than [`MAX_DESCRIPTOR_BYTES`] is refused before any parse,
    /// independent of the upload client's own response cap.
    #[test]
    fn oversized_input_is_refused_before_parsing() {
        let oversized = vec![b'x'; MAX_DESCRIPTOR_BYTES + 1];
        assert_eq!(
            BlobDescriptor::parse_json(&oversized),
            Err(DescriptorError::TooLarge {
                limit_bytes: MAX_DESCRIPTOR_BYTES
            })
        );
    }
}
