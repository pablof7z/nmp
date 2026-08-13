use std::collections::BTreeSet;
use std::fmt;

use nmp_local_signer::LocalKeySigner;
use nmp_signer::{SigningCapability, SigningProviderDescriptor};
use nostr::PublicKey;

const MAGIC: &[u8; 8] = b"NMPSESS\0";
const SESSION_VERSION: u16 = 1;
const LOCAL_KEY_PROVIDER_VERSION: u16 = 1;

/// Opaque canonical bytes representing one complete identity session.
#[derive(PartialEq, Eq)]
pub struct SessionPayload(Vec<u8>);

impl Drop for SessionPayload {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}

impl SessionPayload {
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl fmt::Debug for SessionPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionPayload")
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionProvider {
    LocalKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningAvailability {
    Unsupported,
    Available,
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAccount {
    pub public_key: PublicKey,
    pub provider: Option<SessionProvider>,
    pub signing: SigningAvailability,
}

impl SessionAccount {
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.public_key
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub accounts: Vec<SessionAccount>,
    pub current_pubkey: Option<PublicKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMutationError {
    EngineClosed,
    InvalidSecretKey,
    AccountNotFound { public_key: PublicKey },
    CapabilityRegistryFull { limit: usize },
    CapabilityInstanceExhausted,
}

impl fmt::Display for SessionMutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EngineClosed => f.write_str("engine already shut down"),
            Self::InvalidSecretKey => f.write_str("invalid local secret key"),
            Self::AccountNotFound { public_key } => {
                write!(f, "session account {public_key} does not exist")
            }
            Self::CapabilityRegistryFull { limit } => {
                write!(f, "signing capability registry is full at {limit} entries")
            }
            Self::CapabilityInstanceExhausted => {
                f.write_str("signing capability instance space exhausted")
            }
        }
    }
}

impl std::error::Error for SessionMutationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRestoreError {
    MalformedPayload,
    UnsupportedVersion {
        found: u16,
    },
    UnsupportedProvider {
        id: String,
    },
    UnsupportedProviderVersion {
        provider: SessionProvider,
        found: u16,
    },
    DuplicateAccount {
        public_key: PublicKey,
    },
    CurrentAccountMissing {
        public_key: PublicKey,
    },
    ProviderPayloadInvalid {
        provider: SessionProvider,
    },
    ProviderPublicKeyMismatch {
        account: PublicKey,
        provider_public_key: PublicKey,
    },
    CapabilityRegistryFull {
        limit: usize,
    },
    EngineStartFailed {
        reason: String,
    },
}

impl fmt::Display for SessionRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedPayload => f.write_str("malformed session payload"),
            Self::UnsupportedVersion { found } => {
                write!(f, "unsupported session payload version {found}")
            }
            Self::UnsupportedProvider { id } => {
                write!(f, "unsupported session provider {id}")
            }
            Self::UnsupportedProviderVersion { provider, found } => {
                write!(f, "unsupported {provider:?} provider version {found}")
            }
            Self::DuplicateAccount { public_key } => {
                write!(f, "duplicate session account {public_key}")
            }
            Self::CurrentAccountMissing { public_key } => {
                write!(f, "current session account {public_key} is missing")
            }
            Self::ProviderPayloadInvalid { provider } => {
                write!(f, "invalid {provider:?} provider payload")
            }
            Self::ProviderPublicKeyMismatch {
                account,
                provider_public_key,
            } => write!(
                f,
                "provider public key {provider_public_key} does not match session account {account}"
            ),
            Self::CapabilityRegistryFull { limit } => {
                write!(f, "signing capability registry is full at {limit} entries")
            }
            Self::EngineStartFailed { reason } => write!(f, "engine start failed: {reason}"),
        }
    }
}

impl std::error::Error for SessionRestoreError {}

pub(crate) struct RestoredAccount {
    pub public_key: PublicKey,
    pub signer: Option<LocalKeySigner>,
}

pub(crate) struct RestoredSession {
    pub accounts: Vec<RestoredAccount>,
    pub current_pubkey: Option<PublicKey>,
}

impl RestoredSession {
    pub(crate) fn empty() -> Self {
        Self {
            accounts: Vec::new(),
            current_pubkey: None,
        }
    }

    pub(crate) fn provider_count(&self) -> usize {
        self.accounts
            .iter()
            .filter(|account| account.signer.is_some())
            .count()
    }

    pub(crate) fn snapshot(&self) -> SessionSnapshot {
        let mut accounts = self
            .accounts
            .iter()
            .map(|account| SessionAccount {
                public_key: account.public_key,
                provider: account.signer.as_ref().map(|_| SessionProvider::LocalKey),
                signing: if account.signer.is_some() {
                    SigningAvailability::Available
                } else {
                    SigningAvailability::Unsupported
                },
            })
            .collect::<Vec<_>>();
        accounts.sort_by_key(|account| account.public_key.to_bytes());
        SessionSnapshot {
            accounts,
            current_pubkey: self.current_pubkey,
        }
    }

    pub(crate) fn encode(&self) -> SessionPayload {
        let descriptors = self
            .accounts
            .iter()
            .filter_map(|account| {
                account
                    .signer
                    .as_ref()
                    .and_then(SigningCapability::persistence_descriptor)
                    .map(|descriptor| (account.public_key, descriptor))
            })
            .collect();
        encode(&self.snapshot(), descriptors)
    }
}

pub(crate) fn decode(payload: &SessionPayload) -> Result<RestoredSession, SessionRestoreError> {
    let mut cursor = Cursor::new(payload.as_bytes());
    if cursor.take(MAGIC.len())? != MAGIC {
        return Err(SessionRestoreError::MalformedPayload);
    }
    let version = cursor.u16()?;
    if version != SESSION_VERSION {
        return Err(SessionRestoreError::UnsupportedVersion { found: version });
    }
    let count = cursor.u32()? as usize;
    // Every account requires at least 33 bytes (public key + provider tag).
    // Reject hostile count prefixes before allocating from untrusted bytes.
    if count > cursor.remaining() / 33 {
        return Err(SessionRestoreError::MalformedPayload);
    }
    let mut seen = BTreeSet::new();
    let mut accounts = Vec::with_capacity(count);
    for _ in 0..count {
        let public_key = public_key(cursor.take(32)?)?;
        if !seen.insert(public_key.to_bytes()) {
            return Err(SessionRestoreError::DuplicateAccount { public_key });
        }
        let signer = match cursor.u8()? {
            0 => None,
            1 => {
                let id_length = cursor.u16()? as usize;
                let id = std::str::from_utf8(cursor.take(id_length)?)
                    .map_err(|_| SessionRestoreError::MalformedPayload)?;
                if id != nmp_local_signer::LOCAL_KEY_PROVIDER_ID.as_str() {
                    return Err(SessionRestoreError::UnsupportedProvider { id: id.to_string() });
                }
                let version = cursor.u16()?;
                if version != LOCAL_KEY_PROVIDER_VERSION {
                    return Err(SessionRestoreError::UnsupportedProviderVersion {
                        provider: SessionProvider::LocalKey,
                        found: version,
                    });
                }
                let length = cursor.u32()? as usize;
                let signer =
                    LocalKeySigner::from_secret_bytes(cursor.take(length)?).map_err(|_| {
                        SessionRestoreError::ProviderPayloadInvalid {
                            provider: SessionProvider::LocalKey,
                        }
                    })?;
                let provider_public_key = signer
                    .public_key()
                    .and_then(|key| PublicKey::from_slice(key.as_bytes()).ok())
                    .ok_or(SessionRestoreError::ProviderPayloadInvalid {
                        provider: SessionProvider::LocalKey,
                    })?;
                if provider_public_key != public_key {
                    return Err(SessionRestoreError::ProviderPublicKeyMismatch {
                        account: public_key,
                        provider_public_key,
                    });
                }
                Some(signer)
            }
            _ => return Err(SessionRestoreError::MalformedPayload),
        };
        accounts.push(RestoredAccount { public_key, signer });
    }
    let current_pubkey = match cursor.u8()? {
        0 => None,
        1 => Some(public_key(cursor.take(32)?)?),
        _ => return Err(SessionRestoreError::MalformedPayload),
    };
    if !cursor.is_empty() {
        return Err(SessionRestoreError::MalformedPayload);
    }
    if let Some(public_key) = current_pubkey {
        if !seen.contains(&public_key.to_bytes()) {
            return Err(SessionRestoreError::CurrentAccountMissing { public_key });
        }
    }
    Ok(RestoredSession {
        accounts,
        current_pubkey,
    })
}

pub(crate) fn encode(
    snapshot: &SessionSnapshot,
    mut descriptors: Vec<(PublicKey, SigningProviderDescriptor)>,
) -> SessionPayload {
    descriptors.sort_by_key(|(public_key, _)| public_key.to_bytes());
    let mut accounts = snapshot.accounts.clone();
    accounts.sort_by_key(|account| account.public_key.to_bytes());
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&SESSION_VERSION.to_be_bytes());
    bytes.extend_from_slice(&(accounts.len() as u32).to_be_bytes());
    for account in accounts {
        bytes.extend_from_slice(&account.public_key.to_bytes());
        match account.provider {
            None => bytes.push(0),
            Some(SessionProvider::LocalKey) => {
                bytes.push(1);
                let descriptor = descriptors
                    .iter()
                    .find(|(public_key, _)| *public_key == account.public_key)
                    .map(|(_, descriptor)| descriptor)
                    .expect("local-key session account must retain its provider descriptor");
                debug_assert_eq!(
                    descriptor.provider(),
                    nmp_local_signer::LOCAL_KEY_PROVIDER_ID
                );
                let provider_id = descriptor.provider().as_str().as_bytes();
                bytes.extend_from_slice(&(provider_id.len() as u16).to_be_bytes());
                bytes.extend_from_slice(provider_id);
                bytes.extend_from_slice(&descriptor.version().to_be_bytes());
                bytes.extend_from_slice(&(descriptor.payload().len() as u32).to_be_bytes());
                bytes.extend_from_slice(descriptor.payload());
            }
        }
    }
    match snapshot.current_pubkey {
        None => bytes.push(0),
        Some(public_key) => {
            bytes.push(1);
            bytes.extend_from_slice(&public_key.to_bytes());
        }
    }
    SessionPayload(bytes)
}

fn public_key(bytes: &[u8]) -> Result<PublicKey, SessionRestoreError> {
    PublicKey::from_slice(bytes).map_err(|_| SessionRestoreError::MalformedPayload)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], SessionRestoreError> {
        let end = self
            .offset
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(SessionRestoreError::MalformedPayload)?;
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, SessionRestoreError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, SessionRestoreError> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| SessionRestoreError::MalformedPayload)?;
        Ok(u16::from_be_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, SessionRestoreError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| SessionRestoreError::MalformedPayload)?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    type RawProvider<'a> = Option<(&'a str, u16, Vec<u8>)>;
    type RawAccount<'a> = (PublicKey, RawProvider<'a>);

    fn secret_bytes(keys: &Keys) -> [u8; 32] {
        keys.secret_key().to_secret_bytes()
    }

    fn raw_payload(accounts: &[RawAccount<'_>], current: Option<PublicKey>) -> SessionPayload {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&SESSION_VERSION.to_be_bytes());
        bytes.extend_from_slice(&(accounts.len() as u32).to_be_bytes());
        for (public_key, provider) in accounts {
            bytes.extend_from_slice(&public_key.to_bytes());
            match provider {
                None => bytes.push(0),
                Some((id, version, material)) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&(id.len() as u16).to_be_bytes());
                    bytes.extend_from_slice(id.as_bytes());
                    bytes.extend_from_slice(&version.to_be_bytes());
                    bytes.extend_from_slice(&(material.len() as u32).to_be_bytes());
                    bytes.extend_from_slice(material);
                }
            }
        }
        match current {
            None => bytes.push(0),
            Some(public_key) => {
                bytes.push(1);
                bytes.extend_from_slice(&public_key.to_bytes());
            }
        }
        SessionPayload::from_bytes(bytes)
    }

    #[test]
    fn canonical_round_trip_restores_key_and_public_key_only_accounts() {
        let signer_keys = Keys::generate();
        let public_only = Keys::generate().public_key();
        let signer = LocalKeySigner::from_secret_bytes(&secret_bytes(&signer_keys)).unwrap();
        let descriptor = signer.persistence_descriptor().unwrap();
        let snapshot = SessionSnapshot {
            accounts: vec![
                SessionAccount {
                    public_key: public_only,
                    provider: None,
                    signing: SigningAvailability::Unsupported,
                },
                SessionAccount {
                    public_key: signer_keys.public_key(),
                    provider: Some(SessionProvider::LocalKey),
                    signing: SigningAvailability::Available,
                },
            ],
            current_pubkey: Some(public_only),
        };
        let payload = encode(&snapshot, vec![(signer_keys.public_key(), descriptor)]);
        let restored = decode(&payload).expect("canonical payload restores");
        assert_eq!(restored.current_pubkey, Some(public_only));
        assert_eq!(restored.accounts.len(), 2);
        assert!(restored
            .accounts
            .iter()
            .any(|account| account.public_key == public_only && account.signer.is_none()));
        assert!(restored.accounts.iter().any(|account| {
            account.public_key == signer_keys.public_key() && account.signer.is_some()
        }));
    }

    #[test]
    fn hostile_count_and_truncation_are_refused_before_allocation() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&SESSION_VERSION.to_be_bytes());
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            decode(&SessionPayload::from_bytes(bytes)),
            Err(SessionRestoreError::MalformedPayload)
        ));

        let truncated = SessionPayload::from_bytes(MAGIC[..4].to_vec());
        assert!(matches!(
            decode(&truncated),
            Err(SessionRestoreError::MalformedPayload)
        ));
    }

    #[test]
    fn every_restore_refusal_is_reachable_and_typed() {
        let keys = Keys::generate();
        let other = Keys::generate();
        let public_key = keys.public_key();

        let mut unsupported_version = MAGIC.to_vec();
        unsupported_version.extend_from_slice(&(SESSION_VERSION + 1).to_be_bytes());
        assert_eq!(
            decode(&SessionPayload::from_bytes(unsupported_version)).err(),
            Some(SessionRestoreError::UnsupportedVersion {
                found: SESSION_VERSION + 1
            })
        );

        assert_eq!(
            decode(&raw_payload(
                &[(public_key, Some(("future-provider", 1, vec![])))],
                None
            ))
            .err(),
            Some(SessionRestoreError::UnsupportedProvider {
                id: "future-provider".to_string()
            })
        );
        assert_eq!(
            decode(&raw_payload(
                &[(
                    public_key,
                    Some((
                        nmp_local_signer::LOCAL_KEY_PROVIDER_ID.as_str(),
                        LOCAL_KEY_PROVIDER_VERSION + 1,
                        secret_bytes(&keys).to_vec(),
                    )),
                )],
                None,
            ))
            .err(),
            Some(SessionRestoreError::UnsupportedProviderVersion {
                provider: SessionProvider::LocalKey,
                found: LOCAL_KEY_PROVIDER_VERSION + 1,
            })
        );
        assert_eq!(
            decode(&raw_payload(
                &[(public_key, None), (public_key, None)],
                None
            ))
            .err(),
            Some(SessionRestoreError::DuplicateAccount { public_key })
        );
        assert_eq!(
            decode(&raw_payload(
                &[(public_key, None)],
                Some(other.public_key())
            ))
            .err(),
            Some(SessionRestoreError::CurrentAccountMissing {
                public_key: other.public_key()
            })
        );
        assert_eq!(
            decode(&raw_payload(
                &[(
                    public_key,
                    Some((
                        nmp_local_signer::LOCAL_KEY_PROVIDER_ID.as_str(),
                        LOCAL_KEY_PROVIDER_VERSION,
                        vec![0; 31],
                    )),
                )],
                None,
            ))
            .err(),
            Some(SessionRestoreError::ProviderPayloadInvalid {
                provider: SessionProvider::LocalKey
            })
        );
        assert_eq!(
            decode(&raw_payload(
                &[(
                    public_key,
                    Some((
                        nmp_local_signer::LOCAL_KEY_PROVIDER_ID.as_str(),
                        LOCAL_KEY_PROVIDER_VERSION,
                        secret_bytes(&other).to_vec(),
                    )),
                )],
                None,
            ))
            .err(),
            Some(SessionRestoreError::ProviderPublicKeyMismatch {
                account: public_key,
                provider_public_key: other.public_key(),
            })
        );
    }

    #[test]
    fn payload_and_provider_debug_never_print_secret_material() {
        let keys = Keys::generate();
        let secret = hex::encode(secret_bytes(&keys));
        let signer = LocalKeySigner::from_secret_bytes(&secret_bytes(&keys)).unwrap();
        let descriptor = signer.persistence_descriptor().unwrap();
        let payload = raw_payload(
            &[(
                keys.public_key(),
                Some((
                    nmp_local_signer::LOCAL_KEY_PROVIDER_ID.as_str(),
                    LOCAL_KEY_PROVIDER_VERSION,
                    secret_bytes(&keys).to_vec(),
                )),
            )],
            None,
        );
        assert!(!format!("{payload:?}").contains(&secret));
        assert!(!format!("{descriptor:?}").contains(&secret));
        assert!(format!("{payload:?}").contains("REDACTED"));
        assert!(format!("{descriptor:?}").contains("REDACTED"));
    }
}
