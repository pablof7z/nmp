//! Facade-owned observation execution evidence.
//!
//! The engine and resolver own production of these facts. This module mirrors
//! them once at the supported facade boundary so a direct-Rust application
//! never depends on mechanism-crate types or reconstructs causality from
//! engine-global diagnostics.

use nmp_grammar::{AccessContext, IdentityField};
use nostr::{JsonUtil, RelayUrl, Timestamp};

/// Ordered execution evidence for one live observation.
#[derive(Debug, Clone)]
pub struct ObservationEvidence {
    /// Monotonic within this observation.
    pub sequence: u64,
    pub fact: ObservationFact,
}

impl ObservationEvidence {
    pub(crate) fn from_engine(value: nmp_engine::core::ObservationEvidence) -> Self {
        let nmp_engine::core::ObservationEvidence { sequence, fact } = value;
        Self {
            sequence,
            fact: ObservationFact::from_engine(fact),
        }
    }
}

fn resolved_value_string(value: nmp_engine::core::ResolvedBindingValue) -> String {
    match value {
        nmp_engine::core::ResolvedBindingValue::Scalar(value) => value,
        nmp_engine::core::ResolvedBindingValue::AddressCoordinate {
            kind,
            author,
            identifier,
        } => format!("{kind}:{author}:{identifier}"),
    }
}

/// Authoritative facts emitted by the owners of resolution and wire state.
#[derive(Debug, Clone)]
pub enum ObservationFact {
    ReactiveInput {
        path: String,
        field: IdentityField,
        revision: u64,
        /// Exact strings destined for a dependent wire-filter field.
        /// Address coordinates use canonical `kind:author:identifier` form.
        values: Vec<String>,
        fingerprint: String,
    },
    DerivedSet {
        path: String,
        revision: u64,
        /// Exact strings destined for a dependent wire-filter field.
        /// Address coordinates use canonical `kind:author:identifier` form.
        values: Vec<String>,
        fingerprint: String,
    },
    ConcreteFilter {
        path: String,
        revision: u64,
        /// Exact canonical NIP-01 filter JSON, in resolver atom order.
        filters: Vec<String>,
        fingerprint: String,
    },
    RelayRequest {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        access: AccessContext,
        transport_generation: u64,
        request_revision: u64,
        /// Exact canonical NIP-01 filter JSON accepted by this transport.
        filter: String,
        replay: bool,
    },
    RelayEose {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        access: AccessContext,
        transport_generation: u64,
        request_revision: u64,
        observed_at: Timestamp,
    },
    RelayClosed {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        access: AccessContext,
        transport_generation: u64,
        request_revision: Option<u64>,
        reason: String,
    },
    RelayRefused {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        access: AccessContext,
        transport_generation: Option<u64>,
        request_revision: u64,
        reason: String,
    },
    Withdrawn,
    /// The bounded delivery mailbox discarded an exact contiguous sequence
    /// range. Loss is visible and never masquerades as a complete trace.
    Overflow {
        first_sequence: u64,
        last_sequence: u64,
        dropped: u64,
    },
}

impl ObservationFact {
    fn from_engine(value: nmp_engine::core::ObservationFact) -> Self {
        match value {
            nmp_engine::core::ObservationFact::ReactiveInput {
                path,
                field,
                revision,
                values,
                fingerprint,
                cause: _,
            } => Self::ReactiveInput {
                path,
                field,
                revision,
                values: values.into_iter().map(resolved_value_string).collect(),
                fingerprint,
            },
            nmp_engine::core::ObservationFact::DerivedSet {
                path,
                revision,
                values,
                fingerprint,
                cause: _,
            } => Self::DerivedSet {
                path,
                revision,
                values: values.into_iter().map(resolved_value_string).collect(),
                fingerprint,
            },
            nmp_engine::core::ObservationFact::ConcreteFilter {
                path,
                revision,
                filters,
                fingerprint,
                cause: _,
            } => Self::ConcreteFilter {
                path,
                revision,
                filters: filters
                    .into_iter()
                    .map(|filter| filter.to_nostr().as_json())
                    .collect(),
                fingerprint,
            },
            nmp_engine::core::ObservationFact::RelayRequest {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                filter,
                replay,
            } => Self::RelayRequest {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                filter: filter.to_nostr().as_json(),
                replay,
            },
            nmp_engine::core::ObservationFact::RelayEose {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                observed_at,
            } => Self::RelayEose {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                observed_at,
            },
            nmp_engine::core::ObservationFact::RelayClosed {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                reason,
            } => Self::RelayClosed {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                reason,
            },
            nmp_engine::core::ObservationFact::RelayRefused {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                reason,
            } => Self::RelayRefused {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                reason,
            },
            nmp_engine::core::ObservationFact::Withdrawn => Self::Withdrawn,
            nmp_engine::core::ObservationFact::Overflow {
                first_sequence,
                last_sequence,
                dropped,
            } => Self::Overflow {
                first_sequence,
                last_sequence,
                dropped,
            },
        }
    }
}
