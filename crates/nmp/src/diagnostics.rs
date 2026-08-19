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
//! ([`AuthDiagnosticsSnapshot`], deferred by #8 Waves 2–3): the ENGINE's
//! `DiagnosticsSnapshot.auth_sessions` field stays `#[doc(hidden)]` at the
//! engine layer, while THIS mirror carries the supported, documented
//! `auth_sessions` projection.
//!
//! A mirror controls which engine FIELDS are documented; it is not a licence
//! to redeclare the closed VOCABULARY one of those fields is written in.
//! [`crate::AuthDiagnosticsPhase`] is therefore re-exported from the engine
//! rather than mirrored (#1616) — exactly as the sibling scoped
//! [`crate::AuthPhase`] already is. The byte-identical copy that used to
//! live here is what let an FFI `match` quietly collapse `AwaitingSend` into
//! `AwaitingRelayAck`, so a direct-Rust app and a native app read different
//! phases for the same session.

use nmp_engine::core::AuthDiagnosticsPhase;
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
    pub authenticate_as: Option<nostr::PublicKey>,
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
}

impl RelayDiagnosticsSnapshot {
    fn from_engine(value: nmp_engine::core::RelayDiagnosticsSnapshot) -> Self {
        // Exhaustive destructure: a new engine diagnostics fact cannot be
        // silently dropped by this mirror — adding a field there breaks
        // this conversion until the mirror carries it too.
        let nmp_engine::core::RelayDiagnosticsSnapshot {
            relay,
            authenticate_as,
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
        } = value;
        Self {
            relay,
            authenticate_as,
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
        }
    }
}

/// Bounded, session-scoped AUTH reducer facts (#8) — the documented
/// projection of the engine's per-session AUTH read-out. Raw challenges and
/// opaque capability identities are deliberately absent; the challenge is
/// exposed only as a stable BLAKE3 descriptor. The engine's `transport_slot`
/// (a connection-pool allocator index) is deliberately absent too — see the
/// discard in [`AuthDiagnosticsSnapshot::from_engine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthDiagnosticsSnapshot {
    pub relay: RelayUrl,
    /// Frozen access identity of the protected session this row describes.
    pub authenticate_as: Option<nostr::PublicKey>,
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
}

impl AuthDiagnosticsSnapshot {
    fn from_engine(value: nmp_engine::core::AuthDiagnosticsSnapshot) -> Self {
        let nmp_engine::core::AuthDiagnosticsSnapshot {
            relay,
            authenticate_as,
            // `transport_slot` is deliberately absent from the facade: it is
            // a connection-pool allocator index (which physical slot
            // currently holds this session), meaningful only to the
            // transport pool's own bookkeeping. No documented capability
            // reads it, and exposing pool layout would make apps able to
            // depend on it. The exhaustive destructure still forces this
            // decision to be revisited if the engine ever repurposes the
            // field.
            transport_slot: _,
            transport_generation,
            epoch_sequence,
            challenge_hash,
            phase,
            policy_bound,
            signer_bound,
            auth_event_id,
        } = value;
        Self {
            relay,
            authenticate_as,
            transport_generation,
            epoch_sequence,
            challenge_hash,
            phase,
            policy_bound,
            signer_bound,
            auth_event_id,
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
    /// the mutable current account.
    Unsignable,
    /// Destinations exist and none of them is working.
    Undeliverable,
}

impl StalledWriteStage {
    fn from_engine(value: nmp_engine::core::StalledWriteStage) -> Self {
        match value {
            nmp_engine::core::StalledWriteStage::Unroutable => Self::Unroutable,
            nmp_engine::core::StalledWriteStage::Unsignable => Self::Unsignable,
            nmp_engine::core::StalledWriteStage::Undeliverable => Self::Undeliverable,
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
    /// [`StalledWriteStage::Unroutable`] this is the only place the reason
    /// is stated at all: the receipt itself says only that its destination
    /// set is empty and still open. Never empty.
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
    fn from_engine(value: nmp_engine::core::StalledWrite) -> Self {
        let nmp_engine::core::StalledWrite {
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
    fn from_engine(value: nmp_engine::core::StalledWriteTotals) -> Self {
        let nmp_engine::core::StalledWriteTotals {
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
    /// Relay session candidates refused by the single whole-demand ceiling,
    /// plus any defense-in-depth dial refusal at the transport boundary.
    pub sessions_rejected_over_cap: u64,
    /// Relay sessions refused outright because the relay advertised ZERO
    /// concurrent subscriptions. Kept apart from
    /// `sessions_rejected_over_cap`: one says the plan was too wide for the
    /// operator's ceiling, the other says this relay will hold nothing open.
    pub sessions_refused_by_subscription_budget: u64,
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
    pub(crate) fn from_engine(value: nmp_engine::core::DiagnosticsSnapshot) -> Self {
        let nmp_engine::core::DiagnosticsSnapshot {
            relays,
            auth_sessions,
            uncovered_author_count,
            dropped_merge_rules,
            sessions_rejected_over_cap,
            sessions_refused_by_subscription_budget,
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
            sessions_rejected_over_cap,
            sessions_refused_by_subscription_budget,
            transport_degraded,
            stalled_writes: stalled_writes
                .into_iter()
                .map(StalledWrite::from_engine)
                .collect(),
            stalled_write_totals: StalledWriteTotals::from_engine(stalled_write_totals),
        }
    }
}

