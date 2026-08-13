//! Canonical NIP-11 values for the supported `nmp` facade.
//!
//! Acquisition and caching stay in [`crate::relay_information_service`]. These
//! types are the single authority for cache policy, freshness, errors,
//! documents, limitations, and snapshots. There is no second facade copy.

use std::collections::BTreeMap;
use std::sync::Arc;

use nostr::RelayUrl;

/// Whether a one-shot read may use a still-fresh cached result or must
/// revalidate/refetch it. Concurrent reads of either kind still share one
/// in-flight request per canonical relay URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayInformationCachePolicy {
    UseCache,
    Refresh,
}

/// Freshness of the returned last-good document at the instant it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayInformationFreshness {
    Fresh,
    Stale,
}

/// A typed acquisition failure. HTTP and parse failures are deliberately
/// values; they are never represented as an empty relay document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayInformationError {
    ServiceClosed,
    /// Relay URL credentials are rejected before an HTTP request is
    /// constructed; reqwest otherwise converts them into a Basic
    /// `Authorization` header.
    CredentialedRelayUrl,
    Http {
        reason: String,
    },
    ResponseTooLarge {
        limit_bytes: u64,
    },
    InvalidDocument {
        reason: String,
    },
}

impl std::fmt::Display for RelayInformationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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

/// Presentation and capability fields NMP understands today. `raw_json` on
/// [`RelayInformationSnapshot`] remains the forward-compatible authority;
/// unknown fields are not discarded just because this typed projection has
/// not learned them yet.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayInformationDocument {
    pub name: Option<String>,
    pub description: Option<String>,
    pub banner: Option<String>,
    pub icon: Option<String>,
    pub pubkey: Option<String>,
    pub self_pubkey: Option<String>,
    pub contact: Option<String>,
    /// `None` means the relay did not advertise a list. `Some(empty)` is an
    /// explicit advertisement that no NIPs are supported.
    pub supported_nips: Option<Vec<u16>>,
    pub software: Option<String>,
    pub version: Option<String>,
    pub terms_of_service: Option<String>,
    /// Advisory limits claimed by the relay. These are never runtime proof
    /// and a planner may only consume them when it can remain exact or
    /// surface an explicit shortfall.
    pub limitation: RelayInformationLimitations,
    /// Exact JSON fragments for structured fields whose schema evolves
    /// independently (`limitation`, `fees`, ...).
    pub structured: BTreeMap<String, String>,
}

/// The current well-known NIP-11 limitation fields. Every field is optional
/// because omission is unknown, never an implicit zero/false claim. The
/// enclosing document's `structured["limitation"]` retains the exact object,
/// including fields this projection does not yet understand.
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

/// One last-good NIP-11 document plus acquisition metadata.
///
/// Cloning this value is deliberately shallow. The exact raw body, parsed
/// document (including structured maps), and revision live in one immutable
/// payload shared by the cache, a refreshing worker, every waiter, and the
/// runtime's capability projection. Metadata-only transitions such as 304
/// revalidation and stale-on-error create another immutable version that
/// cites the same payload.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayInformationSnapshot {
    inner: Arc<RelayInformationSnapshotVersion>,
}

#[derive(Debug, PartialEq)]
struct RelayInformationSnapshotVersion {
    payload: Arc<RelayInformationSnapshotPayload>,
    fetched_at: u64,
    fresh_until: u64,
    freshness: RelayInformationFreshness,
    etag: Option<String>,
    last_modified: Option<String>,
    cache_control: Option<String>,
    expires: Option<String>,
    last_error: Option<RelayInformationError>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct RelayInformationSnapshotPayload {
    relay: RelayUrl,
    document: RelayInformationDocument,
    raw_json: String,
    /// Stable BLAKE3 identity of the exact received JSON representation.
    /// Capability facts cite this revision rather than an unscoped boolean.
    document_revision: String,
}

impl RelayInformationSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        relay: RelayUrl,
        document: RelayInformationDocument,
        raw_json: String,
        document_revision: String,
        fetched_at: u64,
        fresh_until: u64,
        freshness: RelayInformationFreshness,
        etag: Option<String>,
        last_modified: Option<String>,
        cache_control: Option<String>,
        expires: Option<String>,
        last_error: Option<RelayInformationError>,
    ) -> Self {
        Self {
            inner: Arc::new(RelayInformationSnapshotVersion {
                payload: Arc::new(RelayInformationSnapshotPayload {
                    relay,
                    document,
                    raw_json,
                    document_revision,
                }),
                fetched_at,
                fresh_until,
                freshness,
                etag,
                last_modified,
                cache_control,
                expires,
                last_error,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_metadata(
        &self,
        fetched_at: u64,
        fresh_until: u64,
        freshness: RelayInformationFreshness,
        etag: Option<String>,
        last_modified: Option<String>,
        cache_control: Option<String>,
        expires: Option<String>,
        last_error: Option<RelayInformationError>,
    ) -> Self {
        Self {
            inner: Arc::new(RelayInformationSnapshotVersion {
                payload: Arc::clone(&self.inner.payload),
                fetched_at,
                fresh_until,
                freshness,
                etag,
                last_modified,
                cache_control,
                expires,
                last_error,
            }),
        }
    }

    pub(crate) fn with_read_state(
        &self,
        freshness: RelayInformationFreshness,
        last_error: Option<RelayInformationError>,
    ) -> Self {
        self.with_metadata(
            self.fetched_at(),
            self.fresh_until(),
            freshness,
            self.etag().map(str::to_owned),
            self.last_modified().map(str::to_owned),
            self.cache_control().map(str::to_owned),
            self.expires().map(str::to_owned),
            last_error,
        )
    }

    #[must_use]
    pub fn relay(&self) -> &RelayUrl {
        &self.inner.payload.relay
    }

    #[must_use]
    pub fn document(&self) -> &RelayInformationDocument {
        &self.inner.payload.document
    }

    #[must_use]
    pub fn raw_json(&self) -> &str {
        &self.inner.payload.raw_json
    }

    #[must_use]
    pub fn document_revision(&self) -> &str {
        &self.inner.payload.document_revision
    }

    #[must_use]
    pub fn fetched_at(&self) -> u64 {
        self.inner.fetched_at
    }

    #[must_use]
    pub fn fresh_until(&self) -> u64 {
        self.inner.fresh_until
    }

    #[must_use]
    pub fn freshness(&self) -> RelayInformationFreshness {
        self.inner.freshness
    }

    #[must_use]
    pub fn etag(&self) -> Option<&str> {
        self.inner.etag.as_deref()
    }

    #[must_use]
    pub fn last_modified(&self) -> Option<&str> {
        self.inner.last_modified.as_deref()
    }

    #[must_use]
    pub fn cache_control(&self) -> Option<&str> {
        self.inner.cache_control.as_deref()
    }

    #[must_use]
    pub fn expires(&self) -> Option<&str> {
        self.inner.expires.as_deref()
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&RelayInformationError> {
        self.inner.last_error.as_ref()
    }

    /// Advertisement only. This never creates behavioral capability proof.
    #[must_use]
    pub fn advertises_nip(&self, nip: u16) -> Option<bool> {
        self.document()
            .supported_nips
            .as_ref()
            .map(|nips| nips.contains(&nip))
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(crate) fn payload_identity_value(&self) -> usize {
        Arc::as_ptr(&self.inner.payload) as usize
    }

    #[cfg(test)]
    pub(crate) fn payload_identity(&self) -> usize {
        self.payload_identity_value()
    }

    #[cfg(test)]
    pub(crate) fn payload_weak(&self) -> std::sync::Weak<RelayInformationSnapshotPayload> {
        Arc::downgrade(&self.inner.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #852: the service re-exports these types; it does not define a second
    /// family. If a mirror returns, this assignment stops compiling.
    #[test]
    fn service_reexports_the_same_nip11_value_types() {
        fn same_type<T>(_: T, _: T) {}
        same_type(
            RelayInformationCachePolicy::UseCache,
            crate::relay_information_service::RelayInformationCachePolicy::UseCache,
        );
        same_type(
            RelayInformationFreshness::Fresh,
            crate::relay_information_service::RelayInformationFreshness::Fresh,
        );
        same_type(
            RelayInformationError::ServiceClosed,
            crate::relay_information_service::RelayInformationError::ServiceClosed,
        );
    }
}
