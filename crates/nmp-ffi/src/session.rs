//! Opaque decoded key and whole-session values at the native boundary.

use std::sync::Arc;

use k256::SecretKey as ZeroizingSecretKey;
use nostr::secp256k1::rand::{rngs::OsRng, RngCore};
use nostr::PublicKey;
use zeroize::Zeroizing;

use crate::convert::FfiError;

/// One validated decoded Nostr public key.
#[derive(Clone, uniffi::Object)]
pub struct FfiPublicKey {
    pub(crate) inner: PublicKey,
}

impl std::fmt::Debug for FfiPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPublicKey").finish_non_exhaustive()
    }
}

impl FfiPublicKey {
    pub(crate) fn from_public_key(inner: PublicKey) -> Arc<Self> {
        Arc::new(Self { inner })
    }
}

#[uniffi::export]
impl FfiPublicKey {
    #[uniffi::constructor]
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Arc<Self>, FfiError> {
        let inner = PublicKey::from_slice(&bytes).map_err(|_| FfiError::InvalidPublicKeyBytes)?;
        Ok(Arc::new(Self { inner }))
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.inner.to_bytes().to_vec()
    }
}

/// One validated decoded Nostr private key.
///
/// The object intentionally has no byte accessor. Its owned byte buffer is
/// wiped when the final native reference is released. No claim is made about
/// copies created by an app before construction or by the FFI transport.
#[derive(uniffi::Object)]
pub struct FfiPrivateKey {
    bytes: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for FfiPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPrivateKey")
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

impl FfiPrivateKey {
    pub(crate) fn secret_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }
}

/// Validate a secp256k1 scalar without constructing the pinned
/// `nostr::SecretKey`, whose underlying type is `Copy` and only performs
/// non-secure erasure. RustCrypto's `SecretKey` is non-`Copy` and implements
/// compiler-fenced `ZeroizeOnDrop`; the validation owner is dropped before
/// this function returns on the valid path.
fn is_valid_secret_scalar(bytes: &[u8; 32]) -> bool {
    match ZeroizingSecretKey::from_slice(bytes) {
        Ok(validated) => {
            drop(validated);
            true
        }
        Err(_) => false,
    }
}

#[uniffi::export]
impl FfiPrivateKey {
    #[uniffi::constructor]
    pub fn generate() -> Arc<Self> {
        loop {
            let mut bytes = Zeroizing::new([0; 32]);
            OsRng.fill_bytes(bytes.as_mut());
            if is_valid_secret_scalar(&bytes) {
                return Arc::new(Self { bytes });
            }
        }
    }

    #[uniffi::constructor]
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Arc<Self>, FfiError> {
        let transport = Zeroizing::new(bytes);
        let mut bytes = Zeroizing::new([0; 32]);
        if transport.len() != bytes.len() {
            return Err(FfiError::InvalidPrivateKeyBytes);
        }
        bytes.copy_from_slice(transport.as_slice());
        if !is_valid_secret_scalar(&bytes) {
            return Err(FfiError::InvalidPrivateKeyBytes);
        }
        Ok(Arc::new(Self { bytes }))
    }
}

/// Opaque canonical bytes representing one complete engine session.
#[derive(uniffi::Object)]
pub struct FfiSessionPayload {
    bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for FfiSessionPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiSessionPayload")
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

impl FfiSessionPayload {
    pub(crate) fn from_payload(payload: nmp::SessionPayload) -> Arc<Self> {
        Arc::new(Self {
            bytes: Zeroizing::new(payload.into_bytes()),
        })
    }

    pub(crate) fn payload(&self) -> nmp::SessionPayload {
        nmp::SessionPayload::from_bytes(self.bytes.as_slice().to_vec())
    }
}

#[uniffi::export]
impl FfiSessionPayload {
    #[uniffi::constructor]
    pub fn from_bytes(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            bytes: Zeroizing::new(bytes),
        })
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.as_slice().to_vec()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FfiSessionProviderKind {
    LocalKey,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FfiCapabilityAvailability {
    Unsupported,
    Available,
    Unavailable { reason: String },
}

#[derive(Debug, uniffi::Object)]
pub struct FfiSessionAccount {
    pub(crate) inner: nmp::SessionAccount,
}

impl FfiSessionAccount {
    pub(crate) fn from_account(inner: nmp::SessionAccount) -> Arc<Self> {
        Arc::new(Self { inner })
    }
}

#[uniffi::export]
impl FfiSessionAccount {
    pub fn public_key(&self) -> Arc<FfiPublicKey> {
        FfiPublicKey::from_public_key(self.inner.public_key)
    }

    pub fn provider(&self) -> Option<FfiSessionProviderKind> {
        self.inner.provider.map(|provider| match provider {
            nmp::SessionProvider::LocalKey => FfiSessionProviderKind::LocalKey,
        })
    }

    pub fn signing_availability(&self) -> FfiCapabilityAvailability {
        match &self.inner.signing {
            nmp::SigningAvailability::Unsupported => FfiCapabilityAvailability::Unsupported,
            nmp::SigningAvailability::Available => FfiCapabilityAvailability::Available,
            nmp::SigningAvailability::Unavailable { reason } => {
                FfiCapabilityAvailability::Unavailable {
                    reason: reason.clone(),
                }
            }
        }
    }
}

#[derive(Debug, uniffi::Record)]
pub struct FfiSessionSnapshot {
    pub accounts: Vec<Arc<FfiSessionAccount>>,
    pub current_public_key: Option<Arc<FfiPublicKey>>,
}

impl From<nmp::SessionSnapshot> for FfiSessionSnapshot {
    fn from(snapshot: nmp::SessionSnapshot) -> Self {
        Self {
            accounts: snapshot
                .accounts
                .into_iter()
                .map(FfiSessionAccount::from_account)
                .collect(),
            current_public_key: snapshot.current_pubkey.map(FfiPublicKey::from_public_key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_key_values_validate_length_and_scalar() {
        assert_eq!(
            FfiPublicKey::from_bytes(vec![0; 31]).unwrap_err(),
            FfiError::InvalidPublicKeyBytes
        );
        assert_eq!(
            FfiPrivateKey::from_bytes(vec![0; 32]).unwrap_err(),
            FfiError::InvalidPrivateKeyBytes
        );
        assert_eq!(
            FfiPrivateKey::from_bytes(vec![0xff; 32]).unwrap_err(),
            FfiError::InvalidPrivateKeyBytes
        );
        let private = FfiPrivateKey::from_bytes({
            let mut bytes = vec![0; 32];
            bytes[31] = 1;
            bytes
        })
        .expect("one is a valid secp256k1 scalar");
        assert_eq!(private.secret_bytes()[31], 1);
        assert!(FfiPrivateKey::generate()
            .secret_bytes()
            .iter()
            .any(|byte| *byte != 0));
    }

    #[test]
    fn sensitive_values_have_redacted_debug_output() {
        let private = FfiPrivateKey::from_bytes({
            let mut bytes = vec![0; 32];
            bytes[31] = 1;
            bytes
        })
        .unwrap();
        let payload = FfiSessionPayload::from_bytes(vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(
            format!("{private:?}"),
            "FfiPrivateKey { bytes: \"[REDACTED]\" }"
        );
        assert_eq!(
            format!("{payload:?}"),
            "FfiSessionPayload { bytes: \"[REDACTED]\" }"
        );
    }
}
