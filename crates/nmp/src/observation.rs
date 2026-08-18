//! Facade-owned observation execution evidence.
//!
//! The engine and resolver own production of these facts. This module mirrors
//! them once at the supported facade boundary so a direct-Rust application
//! never depends on mechanism-crate types or reconstructs causality from
//! engine-global diagnostics.

use nmp_grammar::{IdentityField};
use nostr::JsonUtil;

/// One ordered fact from a live observation's real execution.
///
/// `kind` is one of `reactive_input`, `derived_set`, `concrete_filter`,
/// `relay_request`, `request_settled`, `relay_closed`, `request_deferred`,
/// `withdrawn`, or `overflow`. Resolver facts carry exact public wire values
/// in `values`; relay requests carry their canonical NIP-01 filter JSON there.
/// Additional scalar correlation fields are ordered key/value `attributes`:
/// `field`, `relay`, `access`, `transport_generation`, `request_revision`,
/// `replay`, `observed_at`, `reason`, `first_sequence`, `last_sequence`, and
/// `dropped` when applicable. `access` is `public` or `nip42:<hex-pubkey>`.
#[derive(Debug, Clone)]
pub struct ObservationEvidence {
    /// Monotonic within this observation, across every branch of it.
    pub sequence: u64,
    /// The canonical branch index this fact came from, or `None` for a fact
    /// about the observation as a whole (withdrawal, mailbox overflow). Two
    /// branches may resolve identical values at identical paths, so the
    /// branch is the only thing that tells their traces apart.
    pub branch: Option<usize>,
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

fn identity_string(authenticate_as: Option<nostr::PublicKey>) -> String {
    match authenticate_as {
        None => "public".to_owned(),
        Some(public_key) => format!("nip42:{}", public_key.to_hex()),
    }
}

/// The routing lanes that put one REQ on the wire, as a stable
/// comma-separated list. Canonically ordered (the set already is), so a
/// trace assertion never depends on iteration order. Empty renders as
/// `"none"` rather than an empty string, so "no lane asked for this" is a
/// statement rather than a missing value — that is the honest answer for a
/// NIP-77 probe or reconciliation step, which carry no route of their own.
fn lanes_string(lanes: &std::collections::BTreeSet<nmp_router::Lane>) -> String {
    if lanes.is_empty() {
        return "none".to_owned();
    }
    lanes
        .iter()
        .map(|lane| match lane {
            nmp_router::Lane::AuthorOutbound => "author_outbound",
            nmp_router::Lane::Hint => "hint",
            nmp_router::Lane::Provenance => "provenance",
            nmp_router::Lane::OperatorApp => "operator_app",
            nmp_router::Lane::OperatorFallback => "operator_fallback",
            nmp_router::Lane::Exact => "exact",
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn attribute(key: &str, value: impl ToString) -> (String, String) {
    (key.to_owned(), value.to_string())
}

impl ObservationEvidence {
    pub(crate) fn from_engine(value: nmp_engine::core::ObservationEvidence) -> Self {
        let nmp_engine::core::ObservationEvidence {
            sequence,
            branch,
            fact,
        } = value;
        let mut evidence = Self {
            sequence,
            branch,
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
                authenticate_as,
                transport_generation,
                request_revision,
                filter,
                lanes,
                replay,
            } => {
                evidence.kind = "relay_request";
                evidence.path = Some(path);
                evidence.revision = Some(filter_revision);
                evidence.values = vec![filter.to_nostr().as_json()];
                evidence.attributes = vec![
                    attribute("relay", relay),
                    attribute("authenticate_as", identity_string(authenticate_as)),
                    attribute("transport_generation", transport_generation),
                    attribute("request_revision", request_revision),
                    // WHY this relay was asked. `ReadRouting::Auto` decides a
                    // route on the app's behalf, so the trace has to say
                    // which lane it decided on — otherwise a default that
                    // routes is indistinguishable from the filter-shape
                    // inference it replaced.
                    attribute("lanes", lanes_string(&lanes)),
                    attribute("replay", replay),
                ];
            }
            nmp_engine::core::ObservationFact::RequestSettled {
                path,
                filter_revision,
                relay,
                authenticate_as,
                transport_generation,
                request_revision,
                observed_at,
                terminal,
            } => {
                evidence.kind = "request_settled";
                evidence.path = Some(path);
                evidence.revision = Some(filter_revision);
                evidence.attributes = vec![
                    attribute("relay", relay),
                    attribute("authenticate_as", identity_string(authenticate_as)),
                    attribute("transport_generation", transport_generation),
                    attribute("request_revision", request_revision),
                    attribute("observed_at", observed_at.as_secs()),
                    attribute(
                        "terminal",
                        match terminal {
                            nmp_engine::core::RequestTerminal::Eose => "eose",
                            nmp_engine::core::RequestTerminal::Nip77 => "nip77",
                        },
                    ),
                ];
            }
            nmp_engine::core::ObservationFact::RelayClosed {
                path,
                filter_revision,
                relay,
                authenticate_as,
                transport_generation,
                request_revision,
                reason,
            } => {
                evidence.kind = "relay_closed";
                evidence.path = Some(path);
                evidence.revision = Some(filter_revision);
                evidence.attributes = vec![
                    attribute("relay", relay),
                    attribute("authenticate_as", identity_string(authenticate_as)),
                    attribute("transport_generation", transport_generation),
                ];
                if let Some(request_revision) = request_revision {
                    evidence
                        .attributes
                        .push(attribute("request_revision", request_revision));
                }
                evidence.attributes.push(attribute("reason", reason));
            }
            nmp_engine::core::ObservationFact::RequestDeferred {
                path,
                filter_revision,
                relay,
                authenticate_as,
                request_revision,
                retry_at,
                cause,
            } => {
                evidence.kind = "request_deferred";
                evidence.path = Some(path);
                evidence.revision = Some(filter_revision);
                evidence.attributes = vec![
                    attribute("relay", relay),
                    attribute("authenticate_as", identity_string(authenticate_as)),
                    attribute("request_revision", request_revision),
                    attribute("retry_at", retry_at.as_secs()),
                ];
                match cause {
                    nmp_engine::core::LocalSendRefusal::SessionUnavailable => {
                        evidence
                            .attributes
                            .push(attribute("cause", "session_unavailable"));
                    }
                    nmp_engine::core::LocalSendRefusal::WorkerAdmissionRefused { handle } => {
                        evidence
                            .attributes
                            .push(attribute("cause", "worker_admission_refused"));
                        evidence
                            .attributes
                            .push(attribute("transport_generation", handle.generation));
                    }
                }
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
