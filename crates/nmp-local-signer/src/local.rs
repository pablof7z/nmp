//! `LocalKeySigner`: one canonical long-lived secret owner (#765).

use std::fmt;

use bech32::primitives::decode::CheckedHrpstring;
use bech32::Bech32;
use nmp_signer::{
    CryptoCapability, SignerError, SignerOp, SignerPublicKey, SignerSignedEvent,
    SignerUnsignedEvent, SigningCapability, SigningProviderDescriptor, SigningProviderId,
};
use nostr::secp256k1::rand::{rngs::OsRng, RngCore};
use nostr::{Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

use crate::local_crypto::{self, CanonicalSecret, LocalCryptoError};

/// Failure to construct a local signer from caller-supplied secret material.
///
/// Deliberately separate from [`SignerError`]: construction happens before any
/// capability operation exists, so reporting it as an unavailable/rejected
/// *signing operation* would conflate two lifecycle stages. Every variant is
/// constructed in this module's parsing/validation path, so none is dead
/// surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalKeySignerError {
    /// The input was neither a valid 32-byte secp256k1 scalar nor its accepted
    /// hex/`nsec` text representation.
    InvalidSecretKey,
}

impl fmt::Display for LocalKeySignerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid local secret key")
    }
}

impl std::error::Error for LocalKeySignerError {}

/// Implements both `SigningCapability` and `CryptoCapability` over exactly one
/// long-lived secret owner.
///
/// #765: `secret` is a non-`Clone`, non-`Copy`, compiler-fenced owner whose
/// `Drop` wipes the scalar. This type deliberately retains no parallel
/// `nostr::Keys`, `nostr::SecretKey`, or `secp256k1::Keypair`: in the pinned
/// `nostr 0.44.4`/`secp256k1 0.29.1` those are `Copy` and their only erasure
/// is `non_secure_erase`, which upstream documents as giving no guarantee
/// against compiler-created moves or copies. Every sign/encrypt/decrypt
/// borrows `secret` into the shortest-lived zeroizing view instead
/// (`local_crypto`).
pub struct LocalKeySigner {
    public_key: PublicKey,
    secret: CanonicalSecret,
}

pub const LOCAL_KEY_PROVIDER_ID: SigningProviderId = SigningProviderId::new("local-key");

impl LocalKeySigner {
    /// Copy a caller-owned 32-byte scalar into this signer's canonical
    /// zeroizing owner. The borrowed input stays the caller's responsibility;
    /// no additional long-lived operational representation is retained.
    pub fn from_secret_bytes(secret: &[u8]) -> Result<Self, LocalKeySignerError> {
        if secret.len() != 32 {
            return Err(LocalKeySignerError::InvalidSecretKey);
        }
        let mut canonical = CanonicalSecret::zeroed();
        canonical.copy_from_slice(secret);
        Self::from_canonical(canonical)
    }

    /// Parse a 64-character hex scalar or an `nsec` value *directly* into the
    /// canonical owner — no intermediate `nostr::SecretKey` is constructed.
    pub fn parse(secret: &str) -> Result<Self, LocalKeySignerError> {
        if secret.len() == 64 {
            let mut canonical = CanonicalSecret::zeroed();
            decode_hex_into(secret.as_bytes(), canonical.as_mut_bytes())?;
            return Self::from_canonical(canonical);
        }

        let decoded = CheckedHrpstring::new::<Bech32>(secret)
            .map_err(|_| LocalKeySignerError::InvalidSecretKey)?;
        if decoded.hrp().as_str() != "nsec" || decoded.byte_iter().len() != 32 {
            return Err(LocalKeySignerError::InvalidSecretKey);
        }
        let mut canonical = CanonicalSecret::zeroed();
        for (target, source) in canonical.as_mut_bytes().iter_mut().zip(decoded.byte_iter()) {
            *target = source;
        }
        Self::from_canonical(canonical)
    }

    /// Generate a fresh keypair via OS RNG — convenience for tests/tooling.
    /// The scalar is drawn straight into its zeroizing owner.
    #[must_use]
    pub fn generate() -> Self {
        loop {
            let mut canonical = CanonicalSecret::zeroed();
            OsRng.fill_bytes(canonical.as_mut_bytes());
            if let Ok(signer) = Self::from_canonical(canonical) {
                return signer;
            }
        }
    }

    fn from_canonical(secret: CanonicalSecret) -> Result<Self, LocalKeySignerError> {
        let public_key = local_crypto::validate_and_public_key(&secret)
            .map_err(|_| LocalKeySignerError::InvalidSecretKey)?;
        Ok(Self { public_key, secret })
    }
}

fn decode_hex_into(input: &[u8], output: &mut [u8; 32]) -> Result<(), LocalKeySignerError> {
    for (index, pair) in input.as_chunks::<2>().0.iter().enumerate() {
        output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(())
}

fn hex_nibble(byte: u8) -> Result<u8, LocalKeySignerError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(LocalKeySignerError::InvalidSecretKey),
    }
}

/// Redacted: never prints secret key material, matching remote-provider
/// checkpoint precedent by exposing only the public identity.
impl fmt::Debug for LocalKeySigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalKeySigner")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl SigningCapability for LocalKeySigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(SignerPublicKey::new(self.public_key.to_bytes()))
    }

    fn persistence_descriptor(&self) -> Option<SigningProviderDescriptor> {
        Some(SigningProviderDescriptor::new(
            LOCAL_KEY_PROVIDER_ID,
            1,
            self.secret.as_bytes().to_vec(),
        ))
    }

    /// Signs synchronously — the local key never blocks on I/O, so this
    /// always resolves as `SignerOp::Ready`.
    fn sign(&self, unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        // The engine is the only caller and always stamps `unsigned.pubkey`
        // from this signer's own `public_key()`; a mismatch means the
        // caller built the template for a different identity, which must
        // not silently produce an event under this signer's key.
        if unsigned.public_key().as_bytes() != &self.public_key.to_bytes() {
            return SignerOp::err(SignerError::Rejected(format!(
                "unsigned event pubkey {} does not match signer pubkey {}",
                unsigned.public_key(),
                self.public_key
            )));
        }

        let (_, created_at, kind, tags, content) = unsigned.into_parts();
        let tags = match tags
            .into_iter()
            .map(Tag::parse)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(tags) => tags,
            Err(error) => {
                return SignerOp::err(SignerError::Rejected(format!(
                    "unsigned event contains an invalid tag: {error}"
                )))
            }
        };
        let mut unsigned = UnsignedEvent::new(
            self.public_key,
            Timestamp::from(created_at),
            Kind::from(kind),
            tags,
            content,
        );

        // `UnsignedEvent::id()` computes/reuses the frozen id; `add_signature`
        // is upstream's verified attach path, so the signature still has to
        // check out against the declared id and pubkey.
        let id = unsigned.id();
        let signature = match local_crypto::sign(&self.secret, &id.to_bytes()) {
            Ok(signature) => signature,
            Err(error) => {
                return SignerOp::err(SignerError::Rejected(format!("sign failed: {error}")))
            }
        };
        match unsigned.add_signature(signature) {
            Ok(event) => SignerOp::ok(SignerSignedEvent::new(
                event.id.to_bytes(),
                SignerPublicKey::new(event.pubkey.to_bytes()),
                event.created_at.as_secs(),
                event.kind.as_u16(),
                event.tags.to_vec().into_iter().map(Tag::to_vec).collect(),
                event.content,
                event.sig.serialize(),
            )),
            Err(error) => SignerOp::err(SignerError::Rejected(format!("sign failed: {error}"))),
        }
    }
}

/// Co-located with the signer because the KEY LIVES IN THE ENGINE (M0
/// amendment, guarantee #12): decrypting gift-wrap/private-list ciphertext
/// requires the same secret material `sign` uses, so this capability lives
/// on the same type rather than behind a separate app-facing door.
impl CryptoCapability for LocalKeySigner {
    /// NIP-44 v2 encrypt through the crate's own zeroizing operation path.
    fn nip44_encrypt(&self, peer: SignerPublicKey, plaintext: &str) -> SignerOp<String> {
        let Ok(peer) = PublicKey::from_slice(peer.as_bytes()) else {
            return SignerOp::err(SignerError::Rejected("invalid peer public key".to_string()));
        };
        into_signer_op(
            "nip44 encrypt",
            local_crypto::nip44_encrypt(&self.secret, peer, plaintext),
        )
    }

    /// NIP-44 v2 decrypt. Turns gift-wrap/private-list ciphertext into raw
    /// plaintext tokens — the caller (engine) owns any further parsing; this
    /// capability never assumes the stored content was plaintext to begin
    /// with.
    fn nip44_decrypt(&self, peer: SignerPublicKey, ciphertext: &str) -> SignerOp<String> {
        let Ok(peer) = PublicKey::from_slice(peer.as_bytes()) else {
            return SignerOp::err(SignerError::Rejected("invalid peer public key".to_string()));
        };
        into_signer_op(
            "nip44 decrypt",
            local_crypto::nip44_decrypt(&self.secret, peer, ciphertext),
        )
    }
}

fn into_signer_op<T: Send + 'static>(
    operation: &str,
    result: Result<T, LocalCryptoError>,
) -> SignerOp<T> {
    match result {
        Ok(value) => SignerOp::ok(value),
        Err(error) => SignerOp::err(SignerError::Rejected(format!(
            "{operation} failed: {error}"
        ))),
    }
}

