//! Exact immutable values shared by the engine and signer providers.
//!
//! These deliberately model only the NIP-01 body and signature bytes needed
//! at the capability boundary. They do not depend on a protocol library,
//! parser, serializer, URL type, runtime, or transport.

use std::fmt;

/// One exact x-only public-key encoding.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SignerPublicKey([u8; 32]);

impl SignerPublicKey {
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for SignerPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignerPublicKey(")?;
        fmt::Display::fmt(self, f)?;
        f.write_str(")")
    }
}

impl fmt::Display for SignerPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Frozen unsigned NIP-01 event body presented to one signer capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignerUnsignedEvent {
    public_key: SignerPublicKey,
    created_at: u64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
}

impl SignerUnsignedEvent {
    #[must_use]
    pub fn new(
        public_key: SignerPublicKey,
        created_at: u64,
        kind: u16,
        tags: Vec<Vec<String>>,
        content: String,
    ) -> Self {
        Self {
            public_key,
            created_at,
            kind,
            tags,
            content,
        }
    }

    #[must_use]
    pub const fn public_key(&self) -> SignerPublicKey {
        self.public_key
    }

    #[must_use]
    pub const fn created_at(&self) -> u64 {
        self.created_at
    }

    #[must_use]
    pub const fn kind(&self) -> u16 {
        self.kind
    }

    #[must_use]
    pub fn tags(&self) -> &[Vec<String>] {
        &self.tags
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    #[must_use]
    pub fn into_parts(self) -> (SignerPublicKey, u64, u16, Vec<Vec<String>>, String) {
        (
            self.public_key,
            self.created_at,
            self.kind,
            self.tags,
            self.content,
        )
    }
}

/// Signed NIP-01 event returned by one signer capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignerSignedEvent {
    id: [u8; 32],
    public_key: SignerPublicKey,
    created_at: u64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
    signature: [u8; 64],
}

/// Owned exact fields recovered when an adapter consumes a signed event.
///
/// Naming the fields keeps engine and provider adapters independent of tuple
/// position while preserving the same dependency-free value boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignerSignedEventParts {
    pub id: [u8; 32],
    pub public_key: SignerPublicKey,
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub signature: [u8; 64],
}

impl SignerSignedEvent {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        id: [u8; 32],
        public_key: SignerPublicKey,
        created_at: u64,
        kind: u16,
        tags: Vec<Vec<String>>,
        content: String,
        signature: [u8; 64],
    ) -> Self {
        Self {
            id,
            public_key,
            created_at,
            kind,
            tags,
            content,
            signature,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &[u8; 32] {
        &self.id
    }

    #[must_use]
    pub const fn public_key(&self) -> SignerPublicKey {
        self.public_key
    }

    #[must_use]
    pub const fn created_at(&self) -> u64 {
        self.created_at
    }

    #[must_use]
    pub const fn kind(&self) -> u16 {
        self.kind
    }

    #[must_use]
    pub fn tags(&self) -> &[Vec<String>] {
        &self.tags
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    #[must_use]
    pub const fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    #[must_use]
    pub fn into_parts(self) -> SignerSignedEventParts {
        SignerSignedEventParts {
            id: self.id,
            public_key: self.public_key,
            created_at: self.created_at,
            kind: self.kind,
            tags: self.tags,
            content: self.content,
            signature: self.signature,
        }
    }
}
