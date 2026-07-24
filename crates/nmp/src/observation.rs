//! Facade-owned observation execution evidence.
//!
//! The engine and resolver own production of these facts. This module mirrors
//! them once at the supported facade boundary so a direct-Rust application
//! never depends on mechanism-crate types or reconstructs causality from
//! engine-global diagnostics.

use nmp_grammar::{AccessContext, IdentityField};
use nostr::JsonUtil;

/// One ordered fact from a live observation's real execution.
///
/// `kind` is one of `reactive_input`, `derived_set`, `concrete_filter`,
/// `relay_request`, `relay_eose`, `relay_closed`, `relay_refused`, `withdrawn`,
/// or `overflow`. Resolver facts carry exact public wire values in `values`;
/// relay requests carry their canonical NIP-01 filter JSON there. Additional
/// scalar correlation fields are ordered key/value `attributes`: `field`,
/// `relay`, `access`, `transport_generation`, `request_revision`, `replay`,
/// `observed_at`, `reason`, `first_sequence`, `last_sequence`, and `dropped`
/// when applicable. `access` is `public` or `nip42:<hex-pubkey>`.
#[derive(Debug, Clone)]
pub struct ObservationEvidence {
    /// Monotonic within this observation.
    pub sequence: u64,
    pub kind: &'static str,
    pub path: Option<String>,
    pub revision: Option<u64>,
    pub values: Vec<String>,
    pub fingerprint: Option<String>,
    pub attributes: Vec<(String, String)>,
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

fn identity_field_string(field: IdentityField) -> &'static str {
    match field {
        IdentityField::ActivePubkey => "active_pubkey",
    }
}

fn access_string(access: AccessContext) -> String {
    match access {
        AccessContext::Public => "public".to_owned(),
        AccessContext::Nip42(public_key) => format!("nip42:{}", public_key.to_hex()),
    }
}

fn attribute(key: &str, value: impl ToString) -> (String, String) {
    (key.to_owned(), value.to_string())
}

impl ObservationEvidence {
    pub(crate) fn from_engine(value: nmp_engine::core::ObservationEvidence) -> Self {
        let nmp_engine::core::ObservationEvidence { sequence, fact } = value;
        let mut evidence = Self {
            sequence,
            kind: "",
            path: None,
            revision: None,
            values: vec![],
            fingerprint: None,
            attributes: vec![],
        };
        match fact {
            nmp_engine::core::ObservationFact::ReactiveInput {
                path,
                field,
                revision,
                values,
                fingerprint,
                cause: _,
            } => {
                evidence.kind = "reactive_input";
                evidence.path = Some(path);
                evidence.revision = Some(revision);
                evidence.values = values.into_iter().map(resolved_value_string).collect();
                evidence.fingerprint = Some(fingerprint);
                evidence
                    .attributes
                    .push(attribute("field", identity_field_string(field)));
            }
            nmp_engine::core::ObservationFact::DerivedSet {
                path,
                revision,
                values,
                fingerprint,
                cause: _,
            } => {
                evidence.kind = "derived_set";
                evidence.path = Some(path);
                evidence.revision = Some(revision);
                evidence.values = values.into_iter().map(resolved_value_string).collect();
                evidence.fingerprint = Some(fingerprint);
            }
            nmp_engine::core::ObservationFact::ConcreteFilter {
                path,
                revision,
                filters,
                fingerprint,
                cause: _,
            } => {
                evidence.kind = "concrete_filter";
                evidence.path = Some(path);
                evidence.revision = Some(revision);
                evidence.values = filters
                    .into_iter()
                    .map(|filter| filter.to_nostr().as_json())
                    .collect();
                evidence.fingerprint = Some(fingerprint);
            }
            nmp_engine::core::ObservationFact::RelayRequest {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                filter,
                replay,
            } => {
                evidence.kind = "relay_request";
                evidence.path = Some(path);
                evidence.revision = Some(filter_revision);
                evidence.values = vec![filter.to_nostr().as_json()];
                evidence.attributes = vec![
                    attribute("relay", relay),
                    attribute("access", access_string(access)),
                    attribute("transport_generation", transport_generation),
                    attribute("request_revision", request_revision),
                    attribute("replay", replay),
                ];
            }
            nmp_engine::core::ObservationFact::RelayEose {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                observed_at,
            } => {
                evidence.kind = "relay_eose";
                evidence.path = Some(path);
                evidence.revision = Some(filter_revision);
                evidence.attributes = vec![
                    attribute("relay", relay),
                    attribute("access", access_string(access)),
                    attribute("transport_generation", transport_generation),
                    attribute("request_revision", request_revision),
                    attribute("observed_at", observed_at.as_secs()),
                ];
            }
            nmp_engine::core::ObservationFact::RelayClosed {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                reason,
            } => {
                evidence.kind = "relay_closed";
                evidence.path = Some(path);
                evidence.revision = Some(filter_revision);
                evidence.attributes = vec![
                    attribute("relay", relay),
                    attribute("access", access_string(access)),
                    attribute("transport_generation", transport_generation),
                ];
                if let Some(request_revision) = request_revision {
                    evidence
                        .attributes
                        .push(attribute("request_revision", request_revision));
                }
                evidence.attributes.push(attribute("reason", reason));
            }
            nmp_engine::core::ObservationFact::RelayRefused {
                path,
                filter_revision,
                relay,
                access,
                transport_generation,
                request_revision,
                reason,
            } => {
                evidence.kind = "relay_refused";
                evidence.path = Some(path);
                evidence.revision = Some(filter_revision);
                evidence.attributes = vec![
                    attribute("relay", relay),
                    attribute("access", access_string(access)),
                ];
                if let Some(transport_generation) = transport_generation {
                    evidence
                        .attributes
                        .push(attribute("transport_generation", transport_generation));
                }
                evidence
                    .attributes
                    .push(attribute("request_revision", request_revision));
                evidence.attributes.push(attribute("reason", reason));
            }
            nmp_engine::core::ObservationFact::Withdrawn => {
                evidence.kind = "withdrawn";
            }
            nmp_engine::core::ObservationFact::Overflow {
                first_sequence,
                last_sequence,
                dropped,
            } => {
                evidence.kind = "overflow";
                evidence.attributes = vec![
                    attribute("first_sequence", first_sequence),
                    attribute("last_sequence", last_sequence),
                    attribute("dropped", dropped),
                ];
            }
        }
        evidence
    }
}
