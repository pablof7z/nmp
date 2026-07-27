//! Narrow, operation-scoped cryptography for [`super::local::LocalKeySigner`].
//!
//! This module deliberately does not construct `nostr::Keys`,
//! `nostr::SecretKey`, or a secp256k1 `Keypair`: those upstream types are
//! `Copy` internally and use `non_secure_erase`. Every NMP-owned secret
//! buffer below instead has one explicit owner whose `Drop` performs a
//! compiler-fenced wipe.

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{compiler_fence, Ordering};

use base64::engine::{general_purpose::STANDARD as BASE64, Engine as _};
use k256::ecdh::diffie_hellman;
use k256::elliptic_curve::bigint::U256;
use k256::elliptic_curve::ops::Reduce;
use k256::elliptic_curve::point::AffineCoordinates;
use k256::elliptic_curve::subtle::ConstantTimeEq;
use k256::{FieldBytes, NonZeroScalar, ProjectivePoint, PublicKey as K256PublicKey, Scalar};
use nostr::secp256k1::rand::{rngs::OsRng, RngCore};
use nostr::secp256k1::schnorr::Signature;
use nostr::PublicKey;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

const _: () = assert!(
    !std::mem::needs_drop::<Sha256>(),
    "WipingSha256 requires a trivially droppable hash state"
);

const NIP44_SALT: &[u8] = b"nip44-v2";
const NIP44_MESSAGE_KEYS_LEN: usize = 76;
const MAX_NIP44_PLAINTEXT_LEN: usize = 65_536 - 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SensitiveKind {
    CanonicalSecret,
    SigningOperationSecret,
    SigningScalar,
    SigningAux,
    SigningNonce,
    SigningResultScalar,
    Nip44OperationSecret,
    ConversationKey,
    HkdfBlock,
    MessageKeys,
    SymmetricCipher,
    PaddedPlaintext,
    DecryptedPlaintext,
    HashDigest,
    HashState,
}

/// An owned sensitive value with deterministic, compiler-fenced erasure.
///
/// It intentionally is neither `Clone` nor `Copy`. `Drop` first calls
/// `Zeroize`, then exposes the still-live erased state to the test audit, so
/// cleanup also runs on every `?` and unwind path.
pub(super) trait Erase: Zeroize {
    fn erased(&self) -> bool;
}

impl<const N: usize> Erase for [u8; N] {
    fn erased(&self) -> bool {
        self.iter().all(|byte| *byte == 0)
    }
}

impl<const N: usize> Erase for [u32; N] {
    fn erased(&self) -> bool {
        self.iter().all(|word| *word == 0)
    }
}

impl Erase for Vec<u8> {
    fn erased(&self) -> bool {
        // `Zeroize for Vec` clears the logical length after wiping every byte
        // through capacity. Inspect that still-owned allocation directly;
        // checking `iter()` here would be vacuous because length is now zero.
        // SAFETY: the allocation remains owned by this live `Vec`, and
        // zeroize initialized the full spare capacity before returning.
        let allocation = unsafe { std::slice::from_raw_parts(self.as_ptr(), self.capacity()) };
        allocation.iter().all(|byte| *byte == 0)
    }
}

impl Erase for Scalar {
    fn erased(&self) -> bool {
        *self == Scalar::ZERO
    }
}

impl Erase for NonZeroScalar {
    fn erased(&self) -> bool {
        // `NonZeroScalar::zeroize` first wipes its scalar, then writes one
        // solely to re-establish the type's non-zero invariant.
        self.as_ref() == &Scalar::ONE
    }
}

impl Erase for FieldBytes {
    fn erased(&self) -> bool {
        self.iter().all(|byte| *byte == 0)
    }
}

pub(super) struct Sensitive<T: Erase> {
    value: T,
    kind: SensitiveKind,
}

impl<T: Erase> Sensitive<T> {
    pub(super) fn new(kind: SensitiveKind, value: T) -> Self {
        Self { value, kind }
    }
}

impl<T: Erase> Deref for Sensitive<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T: Erase> DerefMut for Sensitive<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<T: Erase> Drop for Sensitive<T> {
    fn drop(&mut self) {
        self.value.zeroize();
        record_wipe(self.kind, self.value.erased());
    }
}

/// The signer's sole long-lived secret owner.
pub(super) struct CanonicalSecret(Box<Sensitive<[u8; 32]>>);

impl CanonicalSecret {
    /// Allocate the canonical storage before secret bytes enter it. Moving a
    /// signer after this point moves only the `Box` pointer; the long-lived
    /// scalar allocation itself is never relocated.
    pub(super) fn zeroed() -> Self {
        Self(Box::new(Sensitive::new(
            SensitiveKind::CanonicalSecret,
            [0u8; 32],
        )))
    }

    pub(super) fn copy_from_slice(&mut self, bytes: &[u8]) {
        self.0.copy_from_slice(bytes);
    }

    pub(super) fn as_mut_bytes(&mut self) -> &mut [u8; 32] {
        &mut self.0
    }

    #[cfg(test)]
    pub(super) fn allocation_address(&self) -> *const u8 {
        (self.0.as_ref() as *const Sensitive<[u8; 32]>).cast()
    }

    fn operation_copy(&self, kind: SensitiveKind) -> Box<Sensitive<[u8; 32]>> {
        // Allocate first, then copy, for the same no-relocation property as
        // the canonical owner. The operation view is short-lived but still
        // must not leave an abandoned stack image when returned to a caller.
        let mut bytes = Box::new(Sensitive::new(kind, [0u8; 32]));
        bytes.copy_from_slice(&self.0[..]);
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalCryptoError {
    InvalidSecret,
    InvalidPeer,
    InvalidSignature,
    EmptyPlaintext,
    PlaintextTooLong,
    InvalidPayload,
    InvalidMac,
    InvalidPadding,
    InvalidUtf8,
}

impl fmt::Display for LocalCryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidSecret => "invalid secret key",
            Self::InvalidPeer => "invalid peer public key",
            Self::InvalidSignature => "could not construct Schnorr signature",
            Self::EmptyPlaintext => "message is empty",
            Self::PlaintextTooLong => "message is too long",
            Self::InvalidPayload => "invalid NIP-44 payload",
            Self::InvalidMac => "invalid NIP-44 authentication code",
            Self::InvalidPadding => "invalid NIP-44 padding",
            Self::InvalidUtf8 => "decrypted NIP-44 plaintext is not UTF-8",
        })
    }
}

pub(super) fn validate_and_public_key(
    secret: &CanonicalSecret,
) -> Result<PublicKey, LocalCryptoError> {
    let operation = secret.operation_copy(SensitiveKind::SigningOperationSecret);
    let scalar = sensitive_nonzero_scalar(&operation, SensitiveKind::SigningScalar)?;
    let point = (ProjectivePoint::GENERATOR * scalar.as_ref()).to_affine();
    PublicKey::from_slice(point.x().as_ref()).map_err(|_| LocalCryptoError::InvalidSecret)
}

pub(super) fn sign(
    secret: &CanonicalSecret,
    message: &[u8; 32],
) -> Result<Signature, LocalCryptoError> {
    let mut aux = Sensitive::new(SensitiveKind::SigningAux, [0u8; 32]);
    OsRng.fill_bytes(&mut aux[..]);
    sign_with_owned_aux(secret, message, aux)
}

fn sign_with_owned_aux(
    secret: &CanonicalSecret,
    message: &[u8; 32],
    aux: Sensitive<[u8; 32]>,
) -> Result<Signature, LocalCryptoError> {
    let operation = secret.operation_copy(SensitiveKind::SigningOperationSecret);
    let d0 = sensitive_nonzero_scalar(&operation, SensitiveKind::SigningScalar)?;
    let public_point = (ProjectivePoint::GENERATOR * d0.as_ref()).to_affine();

    let mut d = Sensitive::new(SensitiveKind::SigningScalar, *d0.as_ref());
    if bool::from(public_point.y_is_odd()) {
        *d = -*d;
    }

    let public_x = public_point.x();
    let aux_hash = tagged_hash_sensitive(b"BIP0340/aux", &[&aux[..]], SensitiveKind::HashDigest);
    let mut t = Sensitive::new(SensitiveKind::SigningNonce, [0u8; 32]);
    let d_bytes = Sensitive::new(SensitiveKind::SigningScalar, d.to_bytes());
    for ((out, hash), secret_byte) in t.iter_mut().zip(aux_hash.iter()).zip(d_bytes.iter()) {
        *out = hash ^ secret_byte;
    }

    let nonce_hash = tagged_hash_sensitive(
        b"BIP0340/nonce",
        &[&t[..], public_x.as_ref(), message],
        SensitiveKind::SigningNonce,
    );
    let mut k = Sensitive::new(
        SensitiveKind::SigningNonce,
        <Scalar as Reduce<U256>>::reduce_bytes(FieldBytes::from_slice(&nonce_hash[..])),
    );
    if bool::from(k.is_zero()) {
        return Err(LocalCryptoError::InvalidSignature);
    }

    let nonce_point = (ProjectivePoint::GENERATOR * *k).to_affine();
    let r = nonce_point.x();
    if bool::from(nonce_point.y_is_odd()) {
        *k = -*k;
    }

    let challenge_hash = tagged_hash_public(
        b"BIP0340/challenge",
        &[r.as_ref(), public_x.as_ref(), message],
    );
    let challenge = <Scalar as Reduce<U256>>::reduce_bytes(FieldBytes::from_slice(&challenge_hash));
    let s = Sensitive::new(SensitiveKind::SigningResultScalar, *k + challenge * *d);
    let s_bytes = Sensitive::new(SensitiveKind::SigningResultScalar, s.to_bytes());

    let mut encoded = [0u8; 64];
    encoded[..32].copy_from_slice(r.as_ref());
    encoded[32..].copy_from_slice(&s_bytes[..]);
    Signature::from_slice(&encoded).map_err(|_| LocalCryptoError::InvalidSignature)
}

pub(super) fn nip44_encrypt(
    secret: &CanonicalSecret,
    peer: PublicKey,
    plaintext: &str,
) -> Result<String, LocalCryptoError> {
    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);
    nip44_encrypt_with_nonce(secret, peer, plaintext, &nonce)
}

fn nip44_encrypt_with_nonce(
    secret: &CanonicalSecret,
    peer: PublicKey,
    plaintext: &str,
    nonce: &[u8; 32],
) -> Result<String, LocalCryptoError> {
    let conversation_key = derive_conversation_key(secret, peer)?;
    let plaintext = plaintext.as_bytes();
    if plaintext.is_empty() {
        return Err(LocalCryptoError::EmptyPlaintext);
    }
    if plaintext.len() > MAX_NIP44_PLAINTEXT_LEN {
        return Err(LocalCryptoError::PlaintextTooLong);
    }

    let message_keys = derive_message_keys(&conversation_key, nonce);

    let padded_len = nip44_padded_len(plaintext.len());
    let mut padded = Sensitive::new(SensitiveKind::PaddedPlaintext, vec![0u8; 2 + padded_len]);
    padded[..2].copy_from_slice(&(plaintext.len() as u16).to_be_bytes());
    padded[2..2 + plaintext.len()].copy_from_slice(plaintext);

    chacha20_xor(&message_keys[..32], &message_keys[32..44], &mut padded);

    let mac = hmac_sha256(
        &message_keys[44..],
        &[nonce, &padded[..]],
        SensitiveKind::HashDigest,
    );
    let mut payload = Vec::with_capacity(1 + 32 + padded.len() + 32);
    payload.push(2);
    payload.extend_from_slice(nonce);
    payload.extend_from_slice(&padded[..]);
    payload.extend_from_slice(&mac[..]);
    Ok(BASE64.encode(payload))
}

#[cfg(test)]
pub(super) fn sign_with_aux_rand(
    secret: &CanonicalSecret,
    message: &[u8; 32],
    aux: [u8; 32],
) -> Result<Signature, LocalCryptoError> {
    sign_with_owned_aux(
        secret,
        message,
        Sensitive::new(SensitiveKind::SigningAux, aux),
    )
}

#[cfg(test)]
pub(super) fn nip44_encrypt_with_test_nonce(
    secret: &CanonicalSecret,
    peer: PublicKey,
    plaintext: &str,
    nonce: [u8; 32],
) -> Result<String, LocalCryptoError> {
    nip44_encrypt_with_nonce(secret, peer, plaintext, &nonce)
}

pub(super) fn nip44_decrypt(
    secret: &CanonicalSecret,
    peer: PublicKey,
    ciphertext: &str,
) -> Result<String, LocalCryptoError> {
    let payload = BASE64
        .decode(ciphertext)
        .map_err(|_| LocalCryptoError::InvalidPayload)?;
    if payload.len() < 99 || payload.first() != Some(&2) {
        return Err(LocalCryptoError::InvalidPayload);
    }

    let nonce = &payload[1..33];
    let encrypted = &payload[33..payload.len() - 32];
    let supplied_mac = &payload[payload.len() - 32..];
    let conversation_key = derive_conversation_key(secret, peer)?;
    let message_keys = derive_message_keys(&conversation_key, nonce);
    let calculated_mac = hmac_sha256(
        &message_keys[44..],
        &[nonce, encrypted],
        SensitiveKind::HashDigest,
    );
    if !constant_time_eq(supplied_mac, &calculated_mac[..]) {
        return Err(LocalCryptoError::InvalidMac);
    }

    let mut plaintext = Sensitive::new(SensitiveKind::DecryptedPlaintext, encrypted.to_vec());
    chacha20_xor(&message_keys[..32], &message_keys[32..44], &mut plaintext);

    let declared_len = plaintext
        .get(..2)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_be_bytes)
        .map(usize::from)
        .ok_or(LocalCryptoError::InvalidPadding)?;
    if declared_len == 0
        || declared_len > MAX_NIP44_PLAINTEXT_LEN
        || plaintext.len() != 2 + nip44_padded_len(declared_len)
        || 2 + declared_len > plaintext.len()
    {
        return Err(LocalCryptoError::InvalidPadding);
    }

    let unpadded = &plaintext[2..2 + declared_len];
    let text = std::str::from_utf8(unpadded).map_err(|_| LocalCryptoError::InvalidUtf8)?;
    // This is the one intentional plaintext transfer: the returned `String`
    // becomes the capability caller's owner. `plaintext`, the internal padded
    // buffer, still wipes immediately after this copy on both success/error.
    Ok(text.to_owned())
}

fn sensitive_nonzero_scalar(
    bytes: &[u8; 32],
    kind: SensitiveKind,
) -> Result<Sensitive<NonZeroScalar>, LocalCryptoError> {
    NonZeroScalar::try_from(&bytes[..])
        .map(|scalar| Sensitive::new(kind, scalar))
        .map_err(|_| LocalCryptoError::InvalidSecret)
}

fn derive_conversation_key(
    secret: &CanonicalSecret,
    peer: PublicKey,
) -> Result<Sensitive<[u8; 32]>, LocalCryptoError> {
    let operation = secret.operation_copy(SensitiveKind::Nip44OperationSecret);
    let scalar = sensitive_nonzero_scalar(&operation, SensitiveKind::Nip44OperationSecret)?;

    // Nostr public keys are x-only BIP-340 keys. Their canonical SEC1
    // representative is the even-y point.
    let mut encoded_peer = [0u8; 33];
    encoded_peer[0] = 0x02;
    encoded_peer[1..].copy_from_slice(peer.as_bytes());
    let peer =
        K256PublicKey::from_sec1_bytes(&encoded_peer).map_err(|_| LocalCryptoError::InvalidPeer)?;
    let shared = diffie_hellman(scalar.deref(), peer.as_affine());

    Ok(hmac_sha256(
        NIP44_SALT,
        &[shared.raw_secret_bytes().as_ref()],
        SensitiveKind::ConversationKey,
    ))
}

fn derive_message_keys(
    conversation_key: &[u8; 32],
    nonce: &[u8],
) -> Sensitive<[u8; NIP44_MESSAGE_KEYS_LEN]> {
    let mut output = Sensitive::new(SensitiveKind::MessageKeys, [0u8; NIP44_MESSAGE_KEYS_LEN]);
    let mut previous: Option<Sensitive<[u8; 32]>> = None;
    let mut offset = 0usize;
    let mut counter = 1u8;

    while offset < output.len() {
        let block = match previous.as_ref() {
            Some(previous) => hmac_sha256(
                conversation_key,
                &[&previous[..], nonce, &[counter]],
                SensitiveKind::HkdfBlock,
            ),
            None => hmac_sha256(
                conversation_key,
                &[nonce, &[counter]],
                SensitiveKind::HkdfBlock,
            ),
        };
        let take = (output.len() - offset).min(block.len());
        output[offset..offset + take].copy_from_slice(&block[..take]);
        offset += take;
        previous = Some(block);
        counter += 1;
    }
    output
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]], kind: SensitiveKind) -> Sensitive<[u8; 32]> {
    debug_assert!(
        key.len() <= 64,
        "all NMP local-key HMAC keys are <= 64 bytes"
    );
    let mut inner_pad = Sensitive::new(SensitiveKind::HashDigest, [0x36u8; 64]);
    let mut outer_pad = Sensitive::new(SensitiveKind::HashDigest, [0x5cu8; 64]);
    for (index, byte) in key.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }

    let inner = sha256_sensitive(
        std::iter::once(&inner_pad[..]).chain(parts.iter().copied()),
        SensitiveKind::HashDigest,
    );
    sha256_sensitive([&outer_pad[..], &inner[..]], kind)
}

fn tagged_hash_sensitive(tag: &[u8], parts: &[&[u8]], kind: SensitiveKind) -> Sensitive<[u8; 32]> {
    let tag_hash = sha256_public([tag]);
    sha256_sensitive(
        [&tag_hash[..], &tag_hash[..]]
            .into_iter()
            .chain(parts.iter().copied()),
        kind,
    )
}

fn tagged_hash_public(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let tag_hash = sha256_public([tag]);
    sha256_public(
        [&tag_hash[..], &tag_hash[..]]
            .into_iter()
            .chain(parts.iter().copied()),
    )
}

fn sha256_sensitive<'a>(
    parts: impl IntoIterator<Item = &'a [u8]>,
    kind: SensitiveKind,
) -> Sensitive<[u8; 32]> {
    Sensitive::new(kind, sha256(parts))
}

fn sha256_public<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> [u8; 32] {
    sha256(parts)
}

fn sha256<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> [u8; 32] {
    let mut state = WipingSha256(Sha256::new());
    for part in parts {
        state.0.update(part);
    }
    let mut digest = state.0.finalize_reset();
    let mut output = [0u8; 32];
    output.copy_from_slice(&digest);
    digest.zeroize();
    output
}

/// `sha2` does not currently implement `Zeroize`; keep the state behind a
/// local owner and wipe its complete representation with volatile writes.
/// This claims only NMP-owned memory, not registers or backend-internal stack
/// frames.
struct WipingSha256(Sha256);

impl Drop for WipingSha256 {
    fn drop(&mut self) {
        let ptr = (&mut self.0 as *mut Sha256).cast::<u8>();
        for index in 0..std::mem::size_of::<Sha256>() {
            // SAFETY: `ptr` covers exactly the live `Sha256` object owned by
            // `self`; byte writes are valid for every object representation.
            unsafe { ptr.add(index).write_volatile(0) };
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: the object is still live for the duration of `Drop`.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, std::mem::size_of::<Sha256>()) };
        record_wipe(
            SensitiveKind::HashState,
            bytes.iter().all(|byte| *byte == 0),
        );
    }
}

fn nip44_padded_len(len: usize) -> usize {
    if len <= 32 {
        return 32;
    }
    let next_power = 1usize << ((usize::BITS - (len - 1).leading_zeros()) as usize);
    let chunk = if next_power <= 256 {
        32
    } else {
        next_power / 8
    };
    chunk * (((len - 1) / chunk) + 1)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn chacha20_xor(key: &[u8], nonce: &[u8], buffer: &mut [u8]) {
    debug_assert_eq!(key.len(), 32);
    debug_assert_eq!(nonce.len(), 12);
    let mut state = Sensitive::new(SensitiveKind::SymmetricCipher, [0u32; 16]);
    state[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for (index, chunk) in key.as_chunks::<4>().0.iter().enumerate() {
        state[4 + index] = u32::from_le_bytes(*chunk);
    }
    for (index, chunk) in nonce.as_chunks::<4>().0.iter().enumerate() {
        state[13 + index] = u32::from_le_bytes(*chunk);
    }

    for (counter, output) in buffer.chunks_mut(64).enumerate() {
        state[12] = u32::try_from(counter).expect("NIP-44 message counter fits u32");
        let mut working = Sensitive::new(SensitiveKind::SymmetricCipher, [0u32; 16]);
        working.copy_from_slice(&state[..]);

        for _ in 0..10 {
            quarter_round(&mut working, 0, 4, 8, 12);
            quarter_round(&mut working, 1, 5, 9, 13);
            quarter_round(&mut working, 2, 6, 10, 14);
            quarter_round(&mut working, 3, 7, 11, 15);
            quarter_round(&mut working, 0, 5, 10, 15);
            quarter_round(&mut working, 1, 6, 11, 12);
            quarter_round(&mut working, 2, 7, 8, 13);
            quarter_round(&mut working, 3, 4, 9, 14);
        }
        for index in 0..16 {
            working[index] = working[index].wrapping_add(state[index]);
        }

        let mut keystream = Sensitive::new(SensitiveKind::SymmetricCipher, [0u8; 64]);
        for (chunk, word) in keystream[..]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(working.iter())
        {
            *chunk = word.to_le_bytes();
        }
        for (byte, key_byte) in output.iter_mut().zip(keystream.iter()) {
            *byte ^= key_byte;
        }
    }
}

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

#[cfg(test)]
thread_local! {
    static WIPE_AUDIT: std::cell::RefCell<Vec<SensitiveKind>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record_wipe(kind: SensitiveKind, erased: bool) {
    assert!(erased, "{kind:?} owner was not erased while still live");
    WIPE_AUDIT.with(|audit| audit.borrow_mut().push(kind));
}

#[cfg(not(test))]
fn record_wipe(_: SensitiveKind, _: bool) {}

#[cfg(test)]
pub(super) fn take_wipe_audit() -> Vec<SensitiveKind> {
    WIPE_AUDIT.with(|audit| std::mem::take(&mut *audit.borrow_mut()))
}

#[cfg(test)]
pub(super) fn clear_wipe_audit() {
    WIPE_AUDIT.with(|audit| audit.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_secret_one() -> CanonicalSecret {
        let mut secret = CanonicalSecret::zeroed();
        secret.as_mut_bytes()[31] = 1;
        secret
    }

    #[test]
    fn nip44_padding_matches_spec_boundaries() {
        assert_eq!(nip44_padded_len(1), 32);
        assert_eq!(nip44_padded_len(32), 32);
        assert_eq!(nip44_padded_len(33), 64);
        assert_eq!(nip44_padded_len(256), 256);
        assert_eq!(nip44_padded_len(257), 320);
        assert_eq!(nip44_padded_len(MAX_NIP44_PLAINTEXT_LEN), 65_536);
    }

    #[test]
    fn canonical_owner_and_operation_path_exclude_nonsecure_key_types() {
        let secret = canonical_secret_one();
        let CanonicalSecret(owner) = secret;
        let Sensitive { value: _, kind: _ } = owner.as_ref();

        let production_source = include_str!("local_crypto.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        for forbidden in [
            "nostr::Keys::",
            "nostr::SecretKey::",
            "secp256k1::Keypair::",
            "Keys::",
            "SecretKey::",
            "Keypair::",
            "impl Erase for Keys",
            "sign_with_keys(",
            "nip44::encrypt(",
            "nip44::decrypt(",
        ] {
            assert!(
                !production_source.contains(forbidden),
                "operation path must not contain {forbidden}"
            );
        }
    }

    #[test]
    fn sensitive_drop_records_cleanup_on_unwind() {
        clear_wipe_audit();
        let result = std::panic::catch_unwind(|| {
            let _secret = Sensitive::new(SensitiveKind::SigningOperationSecret, [7u8; 32]);
            panic!("exercise unwind cleanup");
        });
        assert!(result.is_err());
        assert_eq!(
            take_wipe_audit(),
            vec![SensitiveKind::SigningOperationSecret]
        );
    }

    #[test]
    fn signing_and_nip44_owned_material_wipes_on_unwind() {
        let secret = canonical_secret_one();
        let peer = validate_and_public_key(&secret).expect("valid peer");
        clear_wipe_audit();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let operation = secret.operation_copy(SensitiveKind::SigningOperationSecret);
            let _scalar = sensitive_nonzero_scalar(&operation, SensitiveKind::SigningScalar)
                .expect("valid scalar");
            let conversation = derive_conversation_key(&secret, peer).expect("conversation key");
            let _message_keys = derive_message_keys(&conversation, &[1u8; 32]);
            panic!("exercise complete operation unwind cleanup");
        }));
        assert!(result.is_err());

        let audit = take_wipe_audit();
        for expected in [
            SensitiveKind::SigningOperationSecret,
            SensitiveKind::SigningScalar,
            SensitiveKind::Nip44OperationSecret,
            SensitiveKind::ConversationKey,
            SensitiveKind::MessageKeys,
        ] {
            assert!(
                audit.contains(&expected),
                "{expected:?} must wipe during unwind"
            );
        }
    }

    #[test]
    fn invalid_padding_wipes_decrypted_plaintext_before_refusal() {
        let secret = canonical_secret_one();
        let peer = validate_and_public_key(&secret).expect("valid peer");
        let nonce = [2u8; 32];
        let conversation = derive_conversation_key(&secret, peer).expect("conversation key");
        let message_keys = derive_message_keys(&conversation, &nonce);
        let mut encrypted = Sensitive::new(SensitiveKind::PaddedPlaintext, vec![0u8; 34]);
        chacha20_xor(&message_keys[..32], &message_keys[32..44], &mut encrypted);
        let mac = hmac_sha256(
            &message_keys[44..],
            &[&nonce, &encrypted],
            SensitiveKind::HashDigest,
        );
        let mut payload = vec![2u8];
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&encrypted);
        payload.extend_from_slice(&mac[..]);
        let ciphertext = BASE64.encode(payload);
        drop(mac);
        drop(encrypted);
        drop(message_keys);
        drop(conversation);
        clear_wipe_audit();

        assert_eq!(
            nip44_decrypt(&secret, peer, &ciphertext),
            Err(LocalCryptoError::InvalidPadding)
        );
        let audit = take_wipe_audit();
        assert!(audit.contains(&SensitiveKind::DecryptedPlaintext));
        assert!(audit.contains(&SensitiveKind::MessageKeys));
        assert!(audit.contains(&SensitiveKind::SymmetricCipher));
    }
}
