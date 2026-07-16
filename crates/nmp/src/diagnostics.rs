//! Facade-owned diagnostics values (#8 Wave 5, the `bc8fb97` NIP-11
//! pattern).
//!
//! The engine mechanism owns diagnostics assembly; these values keep the
//! supported `nmp` read-out self-contained instead of re-exporting
//! mechanism-crate types through the public facade. Field names, field
//! types, and variant names deliberately match the engine originals so an
//! app honoring the "depend on `nmp` alone" contract (proved by
//! `nmp-consumer-check`) compiles unchanged; every value is converted
//! exactly once, at the [`crate::DiagnosticsSubscription`] delivery
//! boundary, and is never recomputed or estimated at this layer.
//!
//! This is also where the documented per-session AUTH read-out lands
//! ([`AuthDiagnosticsSnapshot`]/[`AuthDiagnosticsPhase`], deferred by #8
//! Waves 2–3): the ENGINE's `DiagnosticsSnapshot.auth_sessions` field stays
//! `#[doc(hidden)]` at the engine layer, while THIS mirror carries the
//! supported, documented `auth_sessions` projection.

use nmp_grammar::AccessContext;
use nmp_router::Lane;
use nmp_store::CoverageInterval;
use nostr::{EventId, RelayUrl};

/// One filter's proven coverage state at one relay (parallel to
/// [`RelayDiagnosticsSnapshot::filters`] — same order, same rendering).
/// Diagnostics is engine-global and unscoped BY DESIGN — deliberately
/// distinct from the *query* surface's scoped
/// [`AcquisitionEvidence`](crate::AcquisitionEvidence): this carries the
/// exact per-(relay, filter) proven interval (or its absence), never a
/// query-level verdict.
#[derive(Debug, Clone)]
pub struct FilterCoverageEntry {
    /// The exact wire JSON this coverage state is for — identical rendering
    /// to the corresponding entry in [`RelayDiagnosticsSnapshot::filters`].
    pub filter: String,
    /// `Some(interval)` -- this relay has a proven `[from, through]` row for
    /// this exact filter's shape; `None` -- unproven ("no row = not
    /// covered", unchanged from the store's own rule).
    pub coverage: Option<CoverageInterval>,
}

impl FilterCoverageEntry {
    fn from_engine(value: nmp_engine::core::FilterCoverageEntry) -> Self {
        let nmp_engine::core::FilterCoverageEntry { filter, coverage } = value;
        Self { filter, coverage }
    }
}

/// One SESSION's full diagnostics: wire-sub count, lane breakdown, reverse
/// coverage (authors served), the exact filters currently sent, events
/// actually received per kind, and per-filter coverage state. One relay URL
/// planned under several access contexts yields several rows — physical
/// sessions never share a diagnostics row (#8).
#[derive(Debug, Clone)]
pub struct RelayDiagnosticsSnapshot {
    pub relay: RelayUrl,
    /// Frozen access identity of the physical session this row describes.
    pub access: AccessContext,
    pub wire_sub_count: usize,
    /// Reverse coverage: distinct authors this relay covers.
    pub authors_served: usize,
    pub by_lane: Vec<(Lane, usize)>,
    /// The EXACT wire JSON of every filter currently sent to this relay —
    /// never fabricated/derived.
    pub filters: Vec<String>,
    /// Events actually received FROM this relay, counted by kind.
    pub events_by_kind: Vec<(u16, u64)>,
    /// Per-filter coverage, same order/count as `filters`.
    pub coverage: Vec<FilterCoverageEntry>,
    /// Latest advertised NIP list. `None` is unknown/not advertised.
    pub nip11_supported_nips: Option<Vec<u16>>,
    /// BLAKE3 revision of the exact document that supplied the advertisement.
    pub nip11_document_revision: Option<String>,
    /// `fresh` or `stale` for the cited document; `None` when unknown.
    pub nip11_freshness: Option<&'static str>,
    /// Most recent refresh failure retained beside stale last-good evidence.
    pub nip11_last_error: Option<String>,
    /// `unknown`, `advertised_supported`, or `advertised_unsupported`.
    pub nip77_advertisement: &'static str,
    /// `unknown`, `probing`, `behaviorally_proven`, or
    /// `behaviorally_rejected`. Kept separate from advertisement evidence.
    pub nip77_behavior: &'static str,
    /// Current gap-free live-first handoff phase.
    pub nip77_handoff: &'static str,
}

impl RelayDiagnosticsSnapshot {
    fn from_engine(value: nmp_engine::core::RelayDiagnosticsSnapshot) -> Self {
        // Exhaustive destructure: a new engine diagnostics fact cannot be
        // silently dropped by this mirror — adding a field there breaks
        // this conversion until the mirror carries it too.
        let nmp_engine::core::RelayDiagnosticsSnapshot {
            relay,
            access,
            wire_sub_count,
            authors_served,
            by_lane,
            filters,
            events_by_kind,
            coverage,
            nip11_supported_nips,
            nip11_document_revision,
            nip11_freshness,
            nip11_last_error,
            nip77_advertisement,
            nip77_behavior,
            nip77_handoff,
        } = value;
        Self {
            relay,
            access,
            wire_sub_count,
            authors_served,
            by_lane,
            filters,
            events_by_kind,
            coverage: coverage
                .into_iter()
                .map(FilterCoverageEntry::from_engine)
                .collect(),
            nip11_supported_nips,
            nip11_document_revision,
            nip11_freshness,
            nip11_last_error,
            nip77_advertisement,
            nip77_behavior,
            nip77_handoff,
        }
    }
}

/// Bounded, session-scoped AUTH reducer facts (#8) — the documented
/// projection of the engine's per-session AUTH read-out. Raw challenges and
/// opaque capability identities are deliberately absent; the challenge is
/// exposed only as a stable BLAKE3 descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthDiagnosticsSnapshot {
    pub relay: RelayUrl,
    /// Frozen access identity of the protected session this row describes.
    pub access: AccessContext,
    pub transport_slot: u32,
    pub transport_generation: u64,
    /// The current challenge epoch's engine-global sequence; `None` before
    /// the session's first challenge.
    pub epoch_sequence: Option<u64>,
    /// Stable BLAKE3 descriptor of the exact current challenge bytes.
    pub challenge_hash: Option<String>,
    pub phase: AuthDiagnosticsPhase,
    /// Whether a policy capability instance is bound to the current epoch.
    pub policy_bound: bool,
    /// Whether a signer capability instance is bound to the current epoch.
    pub signer_bound: bool,
    /// The frozen kind:22242 event id awaiting/holding relay correlation.
    pub auth_event_id: Option<EventId>,
    pub send_handoff_accepted: bool,
    pub relay_ok_accepted: bool,
}

impl AuthDiagnosticsSnapshot {
    fn from_engine(value: nmp_engine::core::AuthDiagnosticsSnapshot) -> Self {
        let nmp_engine::core::AuthDiagnosticsSnapshot {
            relay,
            access,
            transport_slot,
            transport_generation,
            epoch_sequence,
            challenge_hash,
            phase,
            policy_bound,
            signer_bound,
            auth_event_id,
            send_handoff_accepted,
            relay_ok_accepted,
        } = value;
        Self {
            relay,
            access,
            transport_slot,
            transport_generation,
            epoch_sequence,
            challenge_hash,
            phase: AuthDiagnosticsPhase::from_engine(phase),
            policy_bound,
            signer_bound,
            auth_event_id,
            send_handoff_accepted,
            relay_ok_accepted,
        }
    }
}

/// Where one protected session currently sits in its AUTH lifecycle (#8's
/// ratified vocabulary — see the issue's "Reducer vocabulary refinement").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDiagnosticsPhase {
    AwaitingChallenge,
    AwaitingPolicy,
    AwaitingSignature,
    AwaitingSend,
    AwaitingRelayAck,
    Ready,
    Denied,
    Error,
}

impl AuthDiagnosticsPhase {
    fn from_engine(value: nmp_engine::core::AuthDiagnosticsPhase) -> Self {
        match value {
            nmp_engine::core::AuthDiagnosticsPhase::AwaitingChallenge => Self::AwaitingChallenge,
            nmp_engine::core::AuthDiagnosticsPhase::AwaitingPolicy => Self::AwaitingPolicy,
            nmp_engine::core::AuthDiagnosticsPhase::AwaitingSignature => Self::AwaitingSignature,
            nmp_engine::core::AuthDiagnosticsPhase::AwaitingSend => Self::AwaitingSend,
            nmp_engine::core::AuthDiagnosticsPhase::AwaitingRelayAck => Self::AwaitingRelayAck,
            nmp_engine::core::AuthDiagnosticsPhase::Ready => Self::Ready,
            nmp_engine::core::AuthDiagnosticsPhase::Denied => Self::Denied,
            nmp_engine::core::AuthDiagnosticsPhase::Error => Self::Error,
        }
    }
}

/// The engine-global diagnostics snapshot — "the acceptance test rendered
/// on screen, permanently." One snapshot covers every currently-planned
/// relay session; there is no separate per-query diagnostics (that is
/// [`AcquisitionEvidence`](crate::AcquisitionEvidence), already delivered
/// alongside every observation [`Frame`](crate::Frame)).
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsSnapshot {
    pub relays: Vec<RelayDiagnosticsSnapshot>,
    /// At most one entry per currently connected protected session — the
    /// documented per-session AUTH read-out (#8). Empty while no protected
    /// session is connected; never a claim of aggregate health,
    /// completeness, or authoritative emptiness.
    pub auth_sessions: Vec<AuthDiagnosticsSnapshot>,
    pub uncovered_author_count: usize,
    pub dropped_merge_rules: Vec<&'static str>,
    /// DISCOVERED relays rejected by the engine's relay admission policy
    /// (issue #121) before they could become routable lanes. Counted PER
    /// LANE, not per host — a rejection-event tally for a diagnostics
    /// screen, not a distinct-host count.
    pub discovered_private_relays_rejected: u64,
    /// Relay session candidates refused by the single whole-demand ceiling,
    /// plus any defense-in-depth dial refusal at the transport boundary.
    pub sessions_rejected_over_cap: u64,
    /// `Some(message)` once an ingest/read store door has degraded the local
    /// cache to read-only (issue #122). Observer-visible only — never a
    /// routing input.
    pub store_degraded: Option<String>,
    /// Latest transport acceptance/verifier failure surfaced by the pool.
    /// Observational only; it never changes routing or trust policy.
    pub transport_degraded: Option<String>,
}

impl DiagnosticsSnapshot {
    pub(crate) fn from_engine(value: nmp_engine::core::DiagnosticsSnapshot) -> Self {
        let nmp_engine::core::DiagnosticsSnapshot {
            relays,
            auth_sessions,
            uncovered_author_count,
            dropped_merge_rules,
            discovered_private_relays_rejected,
            sessions_rejected_over_cap,
            store_degraded,
            transport_degraded,
        } = value;
        Self {
            relays: relays
                .into_iter()
                .map(RelayDiagnosticsSnapshot::from_engine)
                .collect(),
            auth_sessions: auth_sessions
                .into_iter()
                .map(AuthDiagnosticsSnapshot::from_engine)
                .collect(),
            uncovered_author_count,
            dropped_merge_rules,
            discovered_private_relays_rejected,
            sessions_rejected_over_cap,
            store_degraded,
            transport_degraded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_auth_session(
        phase: nmp_engine::core::AuthDiagnosticsPhase,
    ) -> nmp_engine::core::AuthDiagnosticsSnapshot {
        nmp_engine::core::AuthDiagnosticsSnapshot {
            relay: RelayUrl::parse("wss://auth.example.com").unwrap(),
            access: AccessContext::Nip42(
                "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
                    .parse()
                    .unwrap(),
            ),
            transport_slot: 3,
            transport_generation: 7,
            epoch_sequence: Some(11),
            challenge_hash: Some("blake3:abc".to_string()),
            phase,
            policy_bound: true,
            signer_bound: false,
            auth_event_id: None,
            send_handoff_accepted: false,
            relay_ok_accepted: false,
        }
    }

    /// Every engine snapshot fact — including the engine-hidden
    /// `auth_sessions` read-out and each of the eight phases — must survive
    /// the facade mirror conversion exactly. The `from_engine` bodies use
    /// exhaustive destructuring, so a NEW engine field is a compile error
    /// here rather than a silently dropped diagnostic fact.
    #[test]
    fn mirror_conversion_preserves_every_engine_fact_and_phase() {
        use nmp_engine::core::AuthDiagnosticsPhase as EnginePhase;
        let phases = [
            (
                EnginePhase::AwaitingChallenge,
                AuthDiagnosticsPhase::AwaitingChallenge,
            ),
            (
                EnginePhase::AwaitingPolicy,
                AuthDiagnosticsPhase::AwaitingPolicy,
            ),
            (
                EnginePhase::AwaitingSignature,
                AuthDiagnosticsPhase::AwaitingSignature,
            ),
            (
                EnginePhase::AwaitingSend,
                AuthDiagnosticsPhase::AwaitingSend,
            ),
            (
                EnginePhase::AwaitingRelayAck,
                AuthDiagnosticsPhase::AwaitingRelayAck,
            ),
            (EnginePhase::Ready, AuthDiagnosticsPhase::Ready),
            (EnginePhase::Denied, AuthDiagnosticsPhase::Denied),
            (EnginePhase::Error, AuthDiagnosticsPhase::Error),
        ];
        for (engine_phase, facade_phase) in phases {
            assert_eq!(
                AuthDiagnosticsPhase::from_engine(engine_phase),
                facade_phase
            );
        }

        let relay = RelayUrl::parse("wss://mirror.example.com").unwrap();
        let engine = nmp_engine::core::DiagnosticsSnapshot {
            relays: vec![nmp_engine::core::RelayDiagnosticsSnapshot {
                relay: relay.clone(),
                access: AccessContext::Public,
                wire_sub_count: 2,
                authors_served: 1,
                by_lane: vec![(Lane::AppRelay, 2)],
                filters: vec!["{\"kinds\":[9999]}".to_string()],
                events_by_kind: vec![(9999, 3)],
                coverage: vec![
                    nmp_engine::core::FilterCoverageEntry {
                        filter: "proven".to_string(),
                        coverage: Some(CoverageInterval {
                            from: nostr::Timestamp::from(4),
                            through: nostr::Timestamp::from(9),
                        }),
                    },
                    nmp_engine::core::FilterCoverageEntry {
                        filter: "unproven".to_string(),
                        coverage: None,
                    },
                ],
                nip11_supported_nips: Some(vec![11, 42]),
                nip11_document_revision: Some("revision".to_string()),
                nip11_freshness: Some("fresh"),
                nip11_last_error: Some("timed out".to_string()),
                nip77_advertisement: "advertised_supported",
                nip77_behavior: "behaviorally_proven",
                nip77_handoff: "reconciling",
            }],
            auth_sessions: vec![engine_auth_session(
                nmp_engine::core::AuthDiagnosticsPhase::AwaitingRelayAck,
            )],
            uncovered_author_count: 7,
            dropped_merge_rules: vec!["limit"],
            discovered_private_relays_rejected: 5,
            sessions_rejected_over_cap: 6,
            store_degraded: Some("read-only".to_string()),
            transport_degraded: Some("verifier unavailable".to_string()),
        };

        let facade = DiagnosticsSnapshot::from_engine(engine);
        assert_eq!(facade.relays.len(), 1);
        let row = &facade.relays[0];
        assert_eq!(row.relay, relay);
        assert_eq!(row.access, AccessContext::Public);
        assert_eq!(row.wire_sub_count, 2);
        assert_eq!(row.authors_served, 1);
        assert_eq!(row.by_lane, vec![(Lane::AppRelay, 2)]);
        assert_eq!(row.filters, vec!["{\"kinds\":[9999]}".to_string()]);
        assert_eq!(row.events_by_kind, vec![(9999, 3)]);
        assert_eq!(row.coverage.len(), 2);
        assert_eq!(row.coverage[0].filter, "proven");
        assert_eq!(
            row.coverage[0].coverage,
            Some(CoverageInterval {
                from: nostr::Timestamp::from(4),
                through: nostr::Timestamp::from(9),
            })
        );
        assert_eq!(row.coverage[1].coverage, None);
        assert_eq!(row.nip11_supported_nips, Some(vec![11, 42]));
        assert_eq!(row.nip11_document_revision.as_deref(), Some("revision"));
        assert_eq!(row.nip11_freshness, Some("fresh"));
        assert_eq!(row.nip11_last_error.as_deref(), Some("timed out"));
        assert_eq!(row.nip77_advertisement, "advertised_supported");
        assert_eq!(row.nip77_behavior, "behaviorally_proven");
        assert_eq!(row.nip77_handoff, "reconciling");

        assert_eq!(facade.auth_sessions.len(), 1);
        let auth = &facade.auth_sessions[0];
        assert_eq!(auth.relay.to_string(), "wss://auth.example.com");
        assert!(matches!(auth.access, AccessContext::Nip42(_)));
        assert_eq!(auth.transport_slot, 3);
        assert_eq!(auth.transport_generation, 7);
        assert_eq!(auth.epoch_sequence, Some(11));
        assert_eq!(auth.challenge_hash.as_deref(), Some("blake3:abc"));
        assert_eq!(auth.phase, AuthDiagnosticsPhase::AwaitingRelayAck);
        assert!(auth.policy_bound);
        assert!(!auth.signer_bound);
        assert_eq!(auth.auth_event_id, None);
        assert!(!auth.send_handoff_accepted);
        assert!(!auth.relay_ok_accepted);

        assert_eq!(facade.uncovered_author_count, 7);
        assert_eq!(facade.dropped_merge_rules, vec!["limit"]);
        assert_eq!(facade.discovered_private_relays_rejected, 5);
        assert_eq!(facade.sessions_rejected_over_cap, 6);
        assert_eq!(facade.store_degraded.as_deref(), Some("read-only"));
        assert_eq!(
            facade.transport_degraded.as_deref(),
            Some("verifier unavailable")
        );
    }
}
