//! Facade-owned NIP-11 values.
//!
//! The engine mechanism owns acquisition and caching. These values keep the
//! supported `nmp` API self-contained instead of leaking mechanism-crate type
//! paths through the public facade.

use std::collections::BTreeMap;

use nostr::RelayUrl;

/// Whether a one-shot read may use a fresh cached result or must refresh it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayInformationCachePolicy {
    UseCache,
    Refresh,
}

impl RelayInformationCachePolicy {
    pub(crate) fn into_engine(self) -> nmp_engine::relay_information::RelayInformationCachePolicy {
        match self {
            Self::UseCache => nmp_engine::relay_information::RelayInformationCachePolicy::UseCache,
            Self::Refresh => nmp_engine::relay_information::RelayInformationCachePolicy::Refresh,
        }
    }
}

/// Freshness of the returned last-good document at the instant it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayInformationFreshness {
    Fresh,
    Stale,
}

impl RelayInformationFreshness {
    fn from_engine(value: nmp_engine::relay_information::RelayInformationFreshness) -> Self {
        match value {
            nmp_engine::relay_information::RelayInformationFreshness::Fresh => Self::Fresh,
            nmp_engine::relay_information::RelayInformationFreshness::Stale => Self::Stale,
        }
    }
}

/// Typed failure of one bounded NIP-11 acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayInformationError {
    ExecutorSaturated { capacity: usize },
    WaiterSaturated { capacity: usize },
    ThreadUnavailable { reason: String },
    ServiceClosed,
    CredentialedRelayUrl,
    Http { reason: String },
    ResponseTooLarge { limit_bytes: u64 },
    InvalidDocument { reason: String },
}

impl RelayInformationError {
    pub(crate) fn from_engine(value: nmp_engine::relay_information::RelayInformationError) -> Self {
        match value {
            nmp_engine::relay_information::RelayInformationError::ExecutorSaturated {
                capacity,
            } => Self::ExecutorSaturated { capacity },
            nmp_engine::relay_information::RelayInformationError::WaiterSaturated { capacity } => {
                Self::WaiterSaturated { capacity }
            }
            nmp_engine::relay_information::RelayInformationError::ThreadUnavailable { reason } => {
                Self::ThreadUnavailable { reason }
            }
            nmp_engine::relay_information::RelayInformationError::ServiceClosed => {
                Self::ServiceClosed
            }
            nmp_engine::relay_information::RelayInformationError::CredentialedRelayUrl => {
                Self::CredentialedRelayUrl
            }
            nmp_engine::relay_information::RelayInformationError::Http { reason } => {
                Self::Http { reason }
            }
            nmp_engine::relay_information::RelayInformationError::ResponseTooLarge {
                limit_bytes,
            } => Self::ResponseTooLarge { limit_bytes },
            nmp_engine::relay_information::RelayInformationError::InvalidDocument { reason } => {
                Self::InvalidDocument { reason }
            }
        }
    }
}

impl std::fmt::Display for RelayInformationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExecutorSaturated { capacity } => write!(
                f,
                "NIP-11 acquisition refused: native task capacity {capacity} is full"
            ),
            Self::WaiterSaturated { capacity } => write!(
                f,
                "NIP-11 acquisition refused: per-relay waiter capacity {capacity} is full"
            ),
            Self::ThreadUnavailable { reason } => {
                write!(f, "NIP-11 acquisition thread unavailable: {reason}")
            }
            Self::ServiceClosed => f.write_str("NIP-11 acquisition service is closed"),
            Self::CredentialedRelayUrl => {
                f.write_str("NIP-11 acquisition refuses relay URL userinfo")
            }
            Self::Http { reason } => write!(f, "NIP-11 HTTP request failed: {reason}"),
            Self::ResponseTooLarge { limit_bytes } => {
                write!(f, "NIP-11 response exceeds {limit_bytes} bytes")
            }
            Self::InvalidDocument { reason } => write!(f, "invalid NIP-11 document: {reason}"),
        }
    }
}

impl std::error::Error for RelayInformationError {}

/// Presentation and capability fields NMP understands today.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayInformationDocument {
    pub name: Option<String>,
    pub description: Option<String>,
    pub banner: Option<String>,
    pub icon: Option<String>,
    pub pubkey: Option<String>,
    pub self_pubkey: Option<String>,
    pub contact: Option<String>,
    pub supported_nips: Option<Vec<u16>>,
    pub software: Option<String>,
    pub version: Option<String>,
    pub terms_of_service: Option<String>,
    pub limitation: RelayInformationLimitations,
    pub structured: BTreeMap<String, String>,
}

impl RelayInformationDocument {
    fn from_engine(value: nmp_engine::relay_information::RelayInformationDocument) -> Self {
        Self {
            name: value.name,
            description: value.description,
            banner: value.banner,
            icon: value.icon,
            pubkey: value.pubkey,
            self_pubkey: value.self_pubkey,
            contact: value.contact,
            supported_nips: value.supported_nips,
            software: value.software,
            version: value.version,
            terms_of_service: value.terms_of_service,
            limitation: RelayInformationLimitations::from_engine(value.limitation),
            structured: value.structured,
        }
    }
}

/// Typed optional NIP-11 limitation claims. Omission remains unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayInformationLimitations {
    pub max_message_length: Option<u64>,
    pub max_subscriptions: Option<u64>,
    pub max_filters: Option<u64>,
    pub max_limit: Option<u64>,
    pub max_subid_length: Option<u64>,
    pub max_event_tags: Option<u64>,
    pub max_content_length: Option<u64>,
    pub min_pow_difficulty: Option<u64>,
    pub auth_required: Option<bool>,
    pub payment_required: Option<bool>,
    pub created_at_lower_limit: Option<u64>,
    pub created_at_upper_limit: Option<u64>,
}

impl RelayInformationLimitations {
    fn from_engine(value: nmp_engine::relay_information::RelayInformationLimitations) -> Self {
        Self {
            max_message_length: value.max_message_length,
            max_subscriptions: value.max_subscriptions,
            max_filters: value.max_filters,
            max_limit: value.max_limit,
            max_subid_length: value.max_subid_length,
            max_event_tags: value.max_event_tags,
            max_content_length: value.max_content_length,
            min_pow_difficulty: value.min_pow_difficulty,
            auth_required: value.auth_required,
            payment_required: value.payment_required,
            created_at_lower_limit: value.created_at_lower_limit,
            created_at_upper_limit: value.created_at_upper_limit,
        }
    }
}

/// One last-good NIP-11 representation plus acquisition metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayInformationSnapshot {
    pub relay: RelayUrl,
    pub document: RelayInformationDocument,
    pub raw_json: String,
    pub document_revision: String,
    pub fetched_at: u64,
    pub fresh_until: u64,
    pub freshness: RelayInformationFreshness,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cache_control: Option<String>,
    pub expires: Option<String>,
    pub last_error: Option<RelayInformationError>,
}

impl RelayInformationSnapshot {
    /// Advertisement only. This never creates behavioral capability proof.
    #[must_use]
    pub fn advertises_nip(&self, nip: u16) -> Option<bool> {
        self.document
            .supported_nips
            .as_ref()
            .map(|nips| nips.contains(&nip))
    }
}

impl RelayInformationSnapshot {
    pub(crate) fn from_engine(
        value: nmp_engine::relay_information::RelayInformationSnapshot,
    ) -> Self {
        Self {
            relay: value.relay,
            document: RelayInformationDocument::from_engine(value.document),
            raw_json: value.raw_json,
            document_revision: value.document_revision,
            fetched_at: value.fetched_at,
            fresh_until: value.fresh_until,
            freshness: RelayInformationFreshness::from_engine(value.freshness),
            etag: value.etag,
            last_modified: value.last_modified,
            cache_control: value.cache_control,
            expires: value.expires,
            last_error: value.last_error.map(RelayInformationError::from_engine),
        }
    }
}
