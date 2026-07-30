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
use nostr::{EventId, RelayUrl, Timestamp};

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
    fn from_engine(value: crate::core::FilterCoverageEntry) -> Self {
        let crate::core::FilterCoverageEntry { filter, coverage } = value;
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
    /// This relay's advertised concurrent-subscription budget (NIP-11
    /// `limitation.max_subscriptions`). `None` means the relay advertised
    /// nothing, and an unadvertised relay is UNBUDGETED — never a
    /// fabricated default.
    pub subscription_budget: Option<usize>,
    /// Subscriptions that budget removed from the plan. Non-zero means real
    /// demand did not reach the wire, and the affected queries say so
    /// through their own acquisition evidence.
    pub subscriptions_refused: usize,
    /// This relay's advertised `limitation.max_subid_length`.
    pub subid_length_limit: Option<usize>,
    /// True iff that advertised length is shorter than the 64-character
    /// subscription ids NMP sends — this relay rejects every REQ.
    pub subid_length_rejects_our_ids: bool,
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
    fn from_engine(value: crate::core::RelayDiagnosticsSnapshot) -> Self {
        // Exhaustive destructure: a new engine diagnostics fact cannot be
        // silently dropped by this mirror — adding a field there breaks
        // this conversion until the mirror carries it too.
        let crate::core::RelayDiagnosticsSnapshot {
            relay,
            access,
            wire_sub_count,
            subscription_budget,
            subscriptions_refused,
            subid_length_limit,
            subid_length_rejects_our_ids,
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
            subscription_budget,
            subscriptions_refused,
            subid_length_limit,
            subid_length_rejects_our_ids,
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
    fn from_engine(value: crate::core::AuthDiagnosticsSnapshot) -> Self {
        let crate::core::AuthDiagnosticsSnapshot {
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
    fn from_engine(value: crate::core::AuthDiagnosticsPhase) -> Self {
        match value {
            crate::core::AuthDiagnosticsPhase::AwaitingChallenge => Self::AwaitingChallenge,
            crate::core::AuthDiagnosticsPhase::AwaitingPolicy => Self::AwaitingPolicy,
            crate::core::AuthDiagnosticsPhase::AwaitingSignature => Self::AwaitingSignature,
            crate::core::AuthDiagnosticsPhase::AwaitingSend => Self::AwaitingSend,
            crate::core::AuthDiagnosticsPhase::AwaitingRelayAck => Self::AwaitingRelayAck,
            crate::core::AuthDiagnosticsPhase::Ready => Self::Ready,
            crate::core::AuthDiagnosticsPhase::Denied => Self::Denied,
            crate::core::AuthDiagnosticsPhase::Error => Self::Error,
        }
    }
}

/// Where a durable write obligation is stuck (#756/#968) — the documented
/// projection of the engine's own stall vocabulary. Three stages, because
/// an app that has to look in three places to answer one question looks in
/// none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StalledWriteStage {
    /// No destination could be computed.
    Unroutable,
    /// No signer answers for the author this write was FROZEN to — never
    /// the mutable active account.
    Unsignable,
    /// Destinations exist and none of them is working.
    Undeliverable,
}

impl StalledWriteStage {
    fn from_engine(value: crate::core::StalledWriteStage) -> Self {
        match value {
            crate::core::StalledWriteStage::Unroutable => Self::Unroutable,
            crate::core::StalledWriteStage::Unsignable => Self::Unsignable,
            crate::core::StalledWriteStage::Undeliverable => Self::Undeliverable,
        }
    }
}

/// One durable write obligation that cannot currently progress.
///
/// Evidence, not a workload noun: nothing here cancels, retries, prunes or
/// acknowledges a write, and nothing here is a receipt id. Cancellation
/// remains the typed receipt door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledWrite {
    /// A stable, restart-reproducible BLAKE3 descriptor of this exact
    /// obligation — NOT a [`ReceiptId`](crate::ReceiptId) and not parseable
    /// back into one. It exists to tell two rows apart and to recognise the
    /// same row across snapshots, so an app can watch a write leave the
    /// list; making it round-trippable would turn this bounded view of
    /// active obligations into an undecided receipt-discovery door.
    pub id: String,
    pub stage: StalledWriteStage,
    /// What this write is waiting for. For
    /// [`StalledWriteStage::Unroutable`] it is the receipt's OWN park
    /// reason, verbatim, so an operator holding both never has to decide
    /// whether two differently-worded sentences are the same fact. Never
    /// empty.
    pub detail: String,
    /// When this obligation was ACCEPTED, replayed verbatim across
    /// restarts. The age is `now - stalled_since`; NMP reports the instant
    /// rather than a duration because a duration baked into a snapshot goes
    /// stale exactly while nothing is happening, and NMP draws no
    /// conclusion from either number — deciding a write has waited long
    /// enough is the app's or the person's, never a timer's.
    ///
    /// **Known imprecision.** This is when the OBLIGATION was accepted, not
    /// when the stall began. The two coincide for
    /// [`StalledWriteStage::Unroutable`] and
    /// [`StalledWriteStage::Unsignable`], which are attempted immediately;
    /// for [`StalledWriteStage::Undeliverable`] it is EARLIER, so a write
    /// that delivered happily for a week before its relay went down an hour
    /// ago still reads as accepted a week ago and an app subtracting will
    /// over-report the outage. The park instant itself has no durable home
    /// yet, and holding it in memory instead is what makes a restart reset
    /// it — so this field takes the durable over-estimate rather than a
    /// number that lies after every reopen.
    pub stalled_since: Timestamp,
}

impl StalledWrite {
    fn from_engine(value: crate::core::StalledWrite) -> Self {
        let crate::core::StalledWrite {
            id,
            stage,
            detail,
            stalled_since,
        } = value;
        Self {
            id,
            stage: StalledWriteStage::from_engine(stage),
            detail,
            stalled_since,
        }
    }
}

/// The exact census behind [`DiagnosticsSnapshot::stalled_writes`]'s bounded
/// detail window. Totals count every stalled obligation, including the ones
/// no detail row was emitted for: a bound on memory is never a lie about how
/// much is stuck, and moving a write into or out of the window changes no
/// total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StalledWriteTotals {
    pub unroutable: u64,
    pub unsignable: u64,
    pub undeliverable: u64,
    /// Stalled obligations with no detail row in this snapshot.
    pub omitted_details: u64,
    /// The detail-window bound this snapshot was built under.
    pub detail_limit: u64,
}

impl StalledWriteTotals {
    fn from_engine(value: crate::core::StalledWriteTotals) -> Self {
        let crate::core::StalledWriteTotals {
            unroutable,
            unsignable,
            undeliverable,
            omitted_details,
            detail_limit,
        } = value;
        Self {
            unroutable,
            unsignable,
            undeliverable,
            omitted_details,
            detail_limit,
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
    /// Network-derived relay candidates rejected by the engine's SSRF
    /// admission policy (issue #121) before they could become router
    /// candidates or neutral route facts. This is a monotonic rejection-
    /// occurrence tally, not a distinct-host or per-direction count. A
    /// provider callback rejection counts once before directional projection;
    /// rejected selector evidence counts once when that exact
    /// `(selection, evidence)` first becomes current.
    pub discovered_private_relays_rejected: u64,
    /// Relay session candidates refused by the single whole-demand ceiling,
    /// plus any defense-in-depth dial refusal at the transport boundary.
    pub sessions_rejected_over_cap: u64,
    /// Relay sessions refused outright because the relay advertised ZERO
    /// concurrent subscriptions. Kept apart from
    /// `sessions_rejected_over_cap`: one says the plan was too wide for the
    /// operator's ceiling, the other says this relay will hold nothing open.
    pub sessions_refused_by_subscription_budget: u64,
    /// `Some(message)` once an ingest/read store door has degraded the local
    /// cache to read-only (issue #122). Observer-visible only — never a
    /// routing input.
    pub store_degraded: Option<String>,
    /// Latest transport acceptance/verifier failure surfaced by the pool.
    /// Observational only; it never changes routing or trust policy.
    pub transport_degraded: Option<String>,
    /// Every durable write obligation that cannot progress, bounded to
    /// [`StalledWriteTotals::detail_limit`] rows in a deterministic display
    /// order (stage, then acceptance instant, then descriptor) that no
    /// scheduler reads.
    ///
    /// A receipt answers "what happened to THIS write", which needs someone
    /// still holding it. Nobody holds the receipt for the message composed
    /// on a train three weeks ago, and that is exactly the write worth
    /// surfacing. Reading this list changes nothing: no retry, no wake, no
    /// receipt retained, no transport or signer kept alive.
    pub stalled_writes: Vec<StalledWrite>,
    /// Exact counts behind that window. See [`StalledWriteTotals`].
    pub stalled_write_totals: StalledWriteTotals,
}

impl DiagnosticsSnapshot {
    pub(crate) fn from_engine(value: crate::core::DiagnosticsSnapshot) -> Self {
        let crate::core::DiagnosticsSnapshot {
            relays,
            auth_sessions,
            uncovered_author_count,
            dropped_merge_rules,
            discovered_private_relays_rejected,
            sessions_rejected_over_cap,
            sessions_refused_by_subscription_budget,
            store_degraded,
            transport_degraded,
            stalled_writes,
            stalled_write_totals,
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
            sessions_refused_by_subscription_budget,
            store_degraded,
            transport_degraded,
            stalled_writes: stalled_writes
                .into_iter()
                .map(StalledWrite::from_engine)
                .collect(),
            stalled_write_totals: StalledWriteTotals::from_engine(stalled_write_totals),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_auth_session(
        phase: crate::core::AuthDiagnosticsPhase,
    ) -> crate::core::AuthDiagnosticsSnapshot {
        crate::core::AuthDiagnosticsSnapshot {
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
        use crate::core::AuthDiagnosticsPhase as EnginePhase;
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
        let engine = crate::core::DiagnosticsSnapshot {
            relays: vec![crate::core::RelayDiagnosticsSnapshot {
                relay: relay.clone(),
                access: AccessContext::Public,
                wire_sub_count: 2,
                subscription_budget: Some(20),
                subscriptions_refused: 1,
                subid_length_limit: Some(32),
                subid_length_rejects_our_ids: true,
                authors_served: 1,
                by_lane: vec![(Lane::OperatorApp, 2)],
                filters: vec!["{\"kinds\":[9999]}".to_string()],
                events_by_kind: vec![(9999, 3)],
                coverage: vec![
                    crate::core::FilterCoverageEntry {
                        filter: "proven".to_string(),
                        coverage: Some(CoverageInterval {
                            from: nostr::Timestamp::from(4),
                            through: nostr::Timestamp::from(9),
                        }),
                    },
                    crate::core::FilterCoverageEntry {
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
                crate::core::AuthDiagnosticsPhase::AwaitingRelayAck,
            )],
            uncovered_author_count: 7,
            dropped_merge_rules: vec!["limit"],
            discovered_private_relays_rejected: 5,
            sessions_rejected_over_cap: 6,
            sessions_refused_by_subscription_budget: 2,
            store_degraded: Some("read-only".to_string()),
            transport_degraded: Some("verifier unavailable".to_string()),
            stalled_writes: vec![crate::core::StalledWrite {
                id: "descriptor".to_string(),
                stage: crate::core::StalledWriteStage::Undeliverable,
                detail: "no destination is reachable: wss://nowhere.example".to_string(),
                stalled_since: nostr::Timestamp::from(1_700_000_000u64),
            }],
            stalled_write_totals: crate::core::StalledWriteTotals {
                unroutable: 1,
                unsignable: 2,
                undeliverable: 3,
                omitted_details: 5,
                detail_limit: 64,
            },
        };

        let facade = DiagnosticsSnapshot::from_engine(engine);
        assert_eq!(facade.relays.len(), 1);
        let row = &facade.relays[0];
        assert_eq!(row.relay, relay);
        assert_eq!(row.access, AccessContext::Public);
        assert_eq!(row.wire_sub_count, 2);
        assert_eq!(row.subscription_budget, Some(20));
        assert_eq!(row.subscriptions_refused, 1);
        assert_eq!(row.subid_length_limit, Some(32));
        assert!(row.subid_length_rejects_our_ids);
        assert_eq!(row.authors_served, 1);
        assert_eq!(row.by_lane, vec![(Lane::OperatorApp, 2)]);
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
        assert_eq!(facade.sessions_refused_by_subscription_budget, 2);
        assert_eq!(facade.store_degraded.as_deref(), Some("read-only"));
        assert_eq!(
            facade.transport_degraded.as_deref(),
            Some("verifier unavailable")
        );

        assert_eq!(facade.stalled_writes.len(), 1);
        let stalled = &facade.stalled_writes[0];
        assert_eq!(stalled.id, "descriptor");
        assert_eq!(stalled.stage, StalledWriteStage::Undeliverable);
        assert_eq!(
            stalled.detail,
            "no destination is reachable: wss://nowhere.example"
        );
        assert_eq!(stalled.stalled_since, Timestamp::from(1_700_000_000u64));
        assert_eq!(
            facade.stalled_write_totals,
            StalledWriteTotals {
                unroutable: 1,
                unsignable: 2,
                undeliverable: 3,
                omitted_details: 5,
                detail_limit: 64,
            }
        );
        for (engine_stage, facade_stage) in [
            (
                crate::core::StalledWriteStage::Unroutable,
                StalledWriteStage::Unroutable,
            ),
            (
                crate::core::StalledWriteStage::Unsignable,
                StalledWriteStage::Unsignable,
            ),
            (
                crate::core::StalledWriteStage::Undeliverable,
                StalledWriteStage::Undeliverable,
            ),
        ] {
            assert_eq!(StalledWriteStage::from_engine(engine_stage), facade_stage);
        }
    }
}
