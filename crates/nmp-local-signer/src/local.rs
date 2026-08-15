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
/// amendment, ledger #12): decrypting gift-wrap/private-list ciphertext
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_crypto::{clear_wipe_audit, take_wipe_audit, SensitiveKind};
    use base64::engine::{general_purpose::STANDARD as BASE64, Engine as _};
    use bech32::{Bech32, Hrp};
    use nmp_signer::SignerSignedEventParts;
    use zeroize::Zeroize;

    fn unsigned_for(signer: &LocalKeySigner, content: &str) -> SignerUnsignedEvent {
        SignerUnsignedEvent::new(
            SignerPublicKey::new(signer.public_key.to_bytes()),
            Timestamp::now().as_secs(),
            Kind::TextNote.as_u16(),
            Vec::new(),
            content.to_string(),
        )
    }

    fn ready<T: Send + 'static>(operation: SignerOp<T>) -> Result<T, SignerError> {
        match operation {
            SignerOp::Ready(result) => result,
            SignerOp::Pending(_) => panic!("local signer must resolve synchronously"),
        }
    }

    #[test]
    fn sign_then_verify_round_trip() {
        let signer = LocalKeySigner::generate();
        let signed =
            ready(signer.sign(unsigned_for(&signer, "hello from nmp-signer"))).expect("sign");
        assert_eq!(
            signed.public_key(),
            SignerPublicKey::new(signer.public_key.to_bytes())
        );
        let SignerSignedEventParts {
            id,
            public_key,
            created_at,
            kind,
            tags,
            content,
            signature,
        } = signed.into_parts();
        let signed = nostr::Event::new(
            nostr::EventId::from_slice(&id).unwrap(),
            PublicKey::from_slice(public_key.as_bytes()).unwrap(),
            Timestamp::from(created_at),
            Kind::from(kind),
            tags.into_iter()
                .map(Tag::parse)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            content,
            nostr::secp256k1::schnorr::Signature::from_slice(&signature).unwrap(),
        );
        assert!(signed.verify().is_ok(), "signed event must verify");
    }

    /// Falsifier: the replacement signing path is the *same* BIP-340, proved
    /// against the official test vector rather than only self-consistency.
    #[test]
    fn signing_matches_official_bip340_vector_zero() {
        let signer = LocalKeySigner::parse(
            "0000000000000000000000000000000000000000000000000000000000000003",
        )
        .expect("valid vector scalar");
        let signature = local_crypto::sign_with_aux_rand(&signer.secret, &[0u8; 32], [0u8; 32])
            .expect("sign official vector");

        assert_eq!(
            signature.to_string(),
            "e907831f80848d1069a5371b402410364bdf1c5f8307b0084c55f1ce2dca821525f66a4a85ea8b71e482a74f382d2ce5ebeee8fdb2172f477df4900d310536c0"
        );
    }

    #[test]
    fn sign_rejects_pubkey_mismatch() {
        let signer = LocalKeySigner::generate();
        let other = LocalKeySigner::generate();
        assert!(matches!(
            ready(signer.sign(unsigned_for(&other, "wrong identity"))),
            Err(SignerError::Rejected(_))
        ));
    }

    /// Falsifier: an input refusal happens before any operation-scoped secret
    /// owner is created.
    #[test]
    fn signing_input_refusal_creates_no_operation_secrets() {
        let signer = LocalKeySigner::generate();
        let other = LocalKeySigner::generate();
        clear_wipe_audit();

        assert!(ready(signer.sign(unsigned_for(&other, "wrong identity"))).is_err());
        let audit = take_wipe_audit();
        assert!(!audit.contains(&SensitiveKind::SigningOperationSecret));
        assert!(!audit.contains(&SensitiveKind::SigningNonce));
        assert!(!audit.contains(&SensitiveKind::SigningResultScalar));
    }

    #[test]
    fn nip44_encrypt_decrypt_round_trip() {
        let alice = LocalKeySigner::generate();
        let bob = LocalKeySigner::generate();
        let plaintext = "the quick brown fox — nip-44 round trip";

        clear_wipe_audit();
        let ciphertext =
            ready(alice.nip44_encrypt(SignerPublicKey::new(bob.public_key.to_bytes()), plaintext))
                .expect("encrypt");
        let encrypt_audit = take_wipe_audit();
        assert!(encrypt_audit.contains(&SensitiveKind::PaddedPlaintext));
        assert!(encrypt_audit.contains(&SensitiveKind::SymmetricCipher));
        assert!(encrypt_audit.contains(&SensitiveKind::HashState));

        clear_wipe_audit();
        let decrypted = ready(bob.nip44_decrypt(
            SignerPublicKey::new(alice.public_key.to_bytes()),
            &ciphertext,
        ))
        .expect("decrypt");
        assert_eq!(decrypted, plaintext);
        let decrypt_audit = take_wipe_audit();
        assert!(decrypt_audit.contains(&SensitiveKind::DecryptedPlaintext));
        assert!(decrypt_audit.contains(&SensitiveKind::SymmetricCipher));
    }

    /// Falsifier: wire compatibility with the replaced `nostr::nips::nip44`
    /// path, proved against the official NIP-44 v2 vectors in both directions.
    #[test]
    fn decrypts_official_nip44_v2_vector() {
        let signer = LocalKeySigner::parse(
            "0000000000000000000000000000000000000000000000000000000000000002",
        )
        .expect("valid vector secret");
        let peer =
            PublicKey::from_hex("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
                .expect("valid vector public key");
        let ciphertext = "AgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABee0G5VSK0/9YypIObAtDKfYEAjD35uVkHyB0F4DwrcNaCXlCWZKaArsGrY6M9wnuTMxWfp1RTN9Xga8no+kF5Vsb";

        assert_eq!(
            ready(signer.nip44_decrypt(SignerPublicKey::new(peer.to_bytes()), ciphertext,))
                .expect("decrypt official vector"),
            "a"
        );
    }

    #[test]
    fn encrypts_official_nip44_v2_vector() {
        let signer = LocalKeySigner::parse(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .expect("valid vector secret");
        let peer =
            PublicKey::from_hex("c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5")
                .expect("valid vector public key");
        let mut nonce = [0u8; 32];
        nonce[31] = 1;

        assert_eq!(
            local_crypto::nip44_encrypt_with_test_nonce(&signer.secret, peer, "a", nonce)
                .expect("encrypt official vector"),
            "AgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABee0G5VSK0/9YypIObAtDKfYEAjD35uVkHyB0F4DwrcNaCXlCWZKaArsGrY6M9wnuTMxWfp1RTN9Xga8no+kF5Vsb"
        );
    }

    #[test]
    fn invalid_nip44_mac_drops_keys_without_plaintext_output() {
        let alice = LocalKeySigner::generate();
        let bob = LocalKeySigner::generate();
        let ciphertext =
            ready(alice.nip44_encrypt(SignerPublicKey::new(bob.public_key.to_bytes()), "wipe me"))
                .expect("encrypt");
        let mut payload = BASE64.decode(ciphertext).expect("base64");
        *payload.last_mut().expect("mac") ^= 1;
        let corrupted = BASE64.encode(payload);
        clear_wipe_audit();

        assert!(ready(bob.nip44_decrypt(
            SignerPublicKey::new(alice.public_key.to_bytes()),
            &corrupted,
        ))
        .is_err());
        let audit = take_wipe_audit();
        assert!(audit.contains(&SensitiveKind::Nip44OperationSecret));
        assert!(audit.contains(&SensitiveKind::ConversationKey));
        assert!(audit.contains(&SensitiveKind::MessageKeys));
        assert!(!audit.contains(&SensitiveKind::DecryptedPlaintext));
    }

    #[test]
    fn public_key_matches_known_secret() {
        let signer = LocalKeySigner::parse(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .expect("valid scalar");
        assert_eq!(
            signer.public_key.to_hex(),
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        );
    }

    #[test]
    fn nsec_parses_directly_and_invalid_scalars_are_rejected() {
        let mut secret = [0u8; 32];
        secret[31] = 1;
        let mut nsec =
            bech32::encode::<Bech32>(Hrp::parse("nsec").unwrap(), &secret).expect("encode nsec");
        let signer = LocalKeySigner::parse(&nsec).expect("parse nsec");
        assert_eq!(
            signer.public_key.to_hex(),
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        );
        secret.zeroize();
        nsec.zeroize();

        clear_wipe_audit();
        assert_eq!(
            LocalKeySigner::from_secret_bytes(&[0u8; 32]).unwrap_err(),
            LocalKeySignerError::InvalidSecretKey
        );
        let invalid_audit = take_wipe_audit();
        assert!(invalid_audit.contains(&SensitiveKind::CanonicalSecret));
        assert!(invalid_audit.contains(&SensitiveKind::SigningOperationSecret));
        assert_eq!(
            LocalKeySigner::parse(
                "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
            )
            .unwrap_err(),
            LocalKeySignerError::InvalidSecretKey
        );
    }

    /// Falsifier: `{:?}` on `LocalKeySigner` must never leak the secret
    /// scalar — only the public key, matching the remote-provider boundary.
    #[test]
    fn debug_output_redacts_secret_key() {
        let secret = "0000000000000000000000000000000000000000000000000000000000000001";
        let signer = LocalKeySigner::parse(secret).expect("valid scalar");
        let debug = format!("{signer:?}");
        assert!(!debug.contains(secret));
        assert!(debug.contains(&signer.public_key.to_hex()));
    }

    /// Falsifier: formatted signing/crypto errors carry neither the secret
    /// hex nor its raw bytes.
    #[test]
    fn errors_redact_secret_material() {
        let secret = "0000000000000000000000000000000000000000000000000000000000000001";
        let secret_bytes = {
            let mut bytes = [0u8; 32];
            bytes[31] = 1;
            bytes
        };
        let signer = LocalKeySigner::parse(secret).expect("valid scalar");
        let other = LocalKeySigner::generate();
        let signing_error = ready(signer.sign(unsigned_for(&other, "wrong identity")))
            .expect_err("identity mismatch")
            .to_string();
        let crypto_error = ready(signer.nip44_decrypt(
            SignerPublicKey::new(signer.public_key.to_bytes()),
            "invalid",
        ))
        .expect_err("invalid payload")
        .to_string();

        for output in [signing_error, crypto_error] {
            assert!(!output.contains(secret));
            assert!(!output
                .as_bytes()
                .windows(secret_bytes.len())
                .any(|window| window == secret_bytes));
        }
    }

    /// Falsifier for #765's core contract: the struct destructures to exactly
    /// public identity plus the canonical owner, and the production source
    /// carries no long-lived `Keys` field or upstream operational call.
    #[test]
    fn signer_has_exactly_one_long_lived_secret_field() {
        let signer = LocalKeySigner::generate();
        let LocalKeySigner {
            public_key: _,
            secret: _,
        } = signer;

        let production_source = include_str!("local.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        assert!(!production_source.contains("keys: Keys"));
        assert!(!production_source.contains("secret_bytes:"));
        assert!(!production_source.contains("sign_with_keys"));
        assert!(!production_source.contains("nip44::encrypt"));
        assert!(!production_source.contains("nip44::decrypt"));
    }

    /// Falsifier: moving the signer never relocates the secret bytes, so the
    /// wipe cannot leave an abandoned stack image behind. This also replaces
    /// the old `secret_bytes_zeroized_on_drop` probe, which read dead stack
    /// storage through a pointer whose value had gone out of scope.
    #[test]
    fn canonical_secret_allocation_does_not_relocate_with_signer() {
        let signer = LocalKeySigner::generate();
        let address = signer.secret.allocation_address();
        let signer = (signer,);
        assert_eq!(signer.0.secret.allocation_address(), address);
        let signer = Box::new(signer.0);
        assert_eq!(signer.secret.allocation_address(), address);
    }

    /// Falsifier: signer replacement and removal each release exactly one
    /// canonical owner — no retained clone survives either path.
    #[test]
    fn drop_replacement_and_removal_each_release_canonical_owner_once() {
        clear_wipe_audit();
        let first = LocalKeySigner::generate();
        let second = LocalKeySigner::generate();
        clear_wipe_audit();
        let mut slot = Some(first);
        let displaced = slot.replace(second);
        drop(displaced);
        assert_eq!(
            take_wipe_audit()
                .into_iter()
                .filter(|kind| *kind == SensitiveKind::CanonicalSecret)
                .count(),
            1,
            "replacement must drop exactly the displaced owner"
        );
        clear_wipe_audit();
        slot.take();
        assert_eq!(
            take_wipe_audit()
                .into_iter()
                .filter(|kind| *kind == SensitiveKind::CanonicalSecret)
                .count(),
            1,
            "removal must drop exactly the remaining owner"
        );
    }
}
