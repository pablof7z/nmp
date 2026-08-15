//! [`DiagnosticsSnapshot`] — the engine-owned, plane-neutral combination of
//! `nmp_router::Diagnostics` (per-relay wire-sub count, exact filters, lane
//! counts, reverse coverage) with the two facts `nmp-router` cannot see on
//! its own: events actually RECEIVED per (relay, kind) (this crate's own
//! counter, bumped by `EngineCore::on_relay_frame`'s `RelayMessage::Event`
//! arm) and per-(filter, relay) coverage (read from the store via
//! `EngineCore::get_coverage`). Read-only, off the data path: nothing here
//! ever influences routing/delivery — it is strictly an observer of the
//! other planes (M5 plan §1, VISION §5's "acceptance test made visible").
//!
//! Filters/coverage are rendered here to their EXACT wire JSON
//! (`ConcreteFilter::to_nostr().as_json()`) — the most legible form for a
//! diagnostics screen, and literally "what was actually asked/proven",
//! never an estimate or a derived summary (the plan's truth-anchor rule).

use std::collections::{BTreeMap, HashMap};

use nostr::{EventId, JsonUtil, RelayUrl, Timestamp};

use nmp_grammar::{AccessContext, RelaySessionKey};
use nmp_router::{Diagnostics, Lane, RelayPlan, WireReq};
use nmp_store::{CoverageInterval, CoverageKey, PersistenceError};

/// One filter's proven coverage state at one relay (parallel to
/// [`RelayDiagnosticsSnapshot::filters`] — same order, same rendering).
/// Diagnostics is engine-global and unscoped BY DESIGN (M5 plan §1) — it is
/// deliberately distinct from the *query* surface's scoped
/// [`super::AcquisitionEvidence`] (`docs/design/scoped-evidence-49-12-plan.md`
/// §4), so this no longer reuses that query-facing type: it keeps its own
/// diagnostics-local fact, the exact per-(relay, filter) proven interval
/// (or its absence), never a query-level verdict.
#[derive(Debug, Clone)]
pub struct FilterCoverageEntry {
    /// The exact wire JSON this coverage state is for — identical rendering
    /// to the corresponding entry in [`RelayDiagnosticsSnapshot::filters`].
    pub filter: String,
    /// `Some(interval)` -- this relay has a proven `[from, through]` row for
    /// this exact filter's shape; `None` -- unproven ("no row = not
    /// covered", unchanged from the store's own rule).
    ///
    /// `None` is NOT self-standing evidence that nothing is proven: if the
    /// store could not be read while this snapshot was built,
    /// [`DiagnosticsSnapshot::store_degraded`] is `Some` and every entry
    /// here is unknown rather than unproven (#763). Read the two together;
    /// they are one fact reported in the one place an app already looks for
    /// persistence health (#122/#745).
    pub coverage: Option<CoverageInterval>,
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
    /// This relay's own advertised concurrent-subscription budget (NIP-11
    /// `limitation.max_subscriptions`, #931). `None` means the relay
    /// advertised nothing and is therefore UNBUDGETED — never a fabricated
    /// default, because two of the eight major relays measured publish no
    /// document at all and dropping their demand over a guess would be a
    /// shortfall nobody asked for.
    pub subscription_budget: Option<usize>,
    /// Subscriptions this relay's advertised budget removed from the plan.
    /// Non-zero means real demand did not reach the wire — and every atom it
    /// carried also carries `ShortfallFact::LocalLimit`, so the app was told.
    pub subscriptions_refused: usize,
    /// This relay's advertised `limitation.max_subid_length`.
    pub subid_length_limit: Option<usize>,
    /// True iff that advertised length is shorter than the 64-character
    /// subscription ids NMP sends, i.e. this relay rejects every REQ. A
    /// diagnosis, never an input: shortening ids to fit would mean deriving
    /// identity from a document that refreshes.
    pub subid_length_rejects_our_ids: bool,
    /// Reverse coverage: distinct authors this relay covers.
    pub authors_served: usize,
    pub by_lane: Vec<(Lane, usize)>,
    /// The EXACT wire JSON of every filter currently sent to this relay
    /// (`ConcreteFilter::to_nostr().as_json()`) — never fabricated/derived.
    pub filters: Vec<String>,
    /// Events actually received FROM this relay, counted by kind — the one
    /// datum `nmp-router`'s own `Diagnostics` cannot see (it never observes
    /// inbound frames); bumped in `EngineCore::on_relay_frame`'s
    /// `RelayMessage::Event` arm.
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
    /// Current gap-free handoff phase: `none`, `awaiting_live_eose`,
    /// `reconciling`, `backfilling`, `fallback_backlog`, or `live`.
    pub nip77_handoff: &'static str,
}

/// Bounded, session-scoped AUTH reducer facts (#8 U4 — the engine-level
/// read-out deferred by Wave 2; the governed facade/FFI projection remains a
/// later wave). Raw challenges and opaque capability identities are
/// deliberately absent; the challenge is exposed only as a stable BLAKE3
/// descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthDiagnosticsSnapshot {
    pub relay: RelayUrl,
    pub access: AccessContext,
    pub transport_slot: u32,
    pub transport_generation: u64,
    pub epoch_sequence: Option<u64>,
    pub challenge_hash: Option<String>,
    pub phase: AuthDiagnosticsPhase,
    pub policy_bound: bool,
    pub signer_bound: bool,
    pub auth_event_id: Option<EventId>,
}

/// Where one protected session currently sits in its AUTH lifecycle (#8's
/// ratified vocabulary — see the issue's "Reducer vocabulary refinement").
///
/// This enum is the SOLE owner of that lifecycle: `crate::diagnostics`
/// re-exports this exact type rather than mirroring it (#1616), and every
/// surface — direct Rust, FFI, Swift, Kotlin — carries all eight members.
/// There is deliberately no companion boolean restating a phase an app can
/// already read: "transport took the AUTH event" is exactly
/// `AwaitingRelayAck | Ready`, and "the relay's OK was correlated" is
/// exactly `Ready`. Two fields owning one property is how the two surfaces
/// came to disagree in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDiagnosticsPhase {
    /// Connected on a protected session with no challenge received yet.
    AwaitingChallenge,
    /// A challenge is held and the registered AUTH policy has not answered.
    AwaitingPolicy,
    /// The policy approved and the signer has not returned the kind:22242
    /// event.
    AwaitingSignature,
    /// The signed AUTH event exists and NMP has not handed it to transport
    /// yet — NMP's own pending work, never the relay's.
    AwaitingSend,
    /// Transport accepted the AUTH event and the relay's OK has not been
    /// correlated — the relay's pending work, never NMP's.
    AwaitingRelayAck,
    /// The relay accepted the AUTH event: this session is authenticated.
    Ready,
    /// The relay rejected the AUTH event, or the policy refused.
    Denied,
    /// The negotiation failed for a reason that is neither a relay refusal
    /// nor a policy refusal.
    Error,
}

/// Where a durable write obligation is stuck (#756/#968). Three stages,
/// because an app that has to look in three places to answer one question
/// looks in none of them, and because the three are acted on differently:
/// nothing an app can do fixes an unroutable write except learning more
/// about the world, an unsignable one wants a signer, and an undeliverable
/// one wants a reachable relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StalledWriteStage {
    /// No destination could be computed. The write is parked on an EMPTY,
    /// still-OPEN destination set
    /// ([`crate::publish_queue::WriteFact::Destinations`] with
    /// `complete: false`) — distinct from
    /// [`crate::publish_queue::WriteOutcome::NoDestination`], which is
    /// knowledge exhausted and terminal.
    Unroutable,
    /// No signer answers for the author this obligation was FROZEN to — the
    /// [`crate::publish_queue::SigningState::AwaitingSigner`] park. Never the
    /// mutable current account.
    Unsignable,
    /// Destinations exist and none of them is working: every relay this
    /// intent still owns a live lane at is unreachable, unstarted, or
    /// route-persistence-blocked, and no attempt is in flight.
    Undeliverable,
}

/// One durable write obligation that cannot currently progress.
///
/// Read-only evidence, never a workload noun: there is no cancel, retry, or
/// prune verb here and no round-trippable receipt identity. Cancellation
/// remains the typed receipt door (`Handle::cancel_write`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledWrite {
    /// A stable, restart-reproducible BLAKE3 descriptor of this exact
    /// obligation — deliberately NOT a [`super::ReceiptId`] and deliberately
    /// not parseable back into one.
    ///
    /// Its only job is correlation: telling two rows of one snapshot apart,
    /// and recognising the same row across snapshots so an app can see a
    /// write leave the list. Making it round-trippable would turn this
    /// bounded active-obligation view into a receipt-discovery door, whose
    /// completeness, retention and access semantics nothing has decided
    /// (#756's identity question; #903 owns the reverse link).
    pub id: String,
    pub stage: StalledWriteStage,
    /// What this write is waiting for.
    ///
    /// For [`StalledWriteStage::Unroutable`] this list is the ONLY place the
    /// reason is stated at all: the receipt itself says only that its
    /// destination set is empty and still open. Never empty — a park that
    /// says only "stuck" is barely better than losing the write.
    pub detail: String,
    /// When this obligation was ACCEPTED — the durable
    /// `AcceptWrite::accepted_at`, replayed verbatim after a restart.
    ///
    /// The age is `now - stalled_since`, and NMP deliberately reports the
    /// instant rather than the duration: a snapshot is re-emitted only when
    /// engine state changes, so a duration baked into one would be stale
    /// exactly while nothing is happening — which is the whole population
    /// this section exists to describe. NMP draws no conclusion from either
    /// number; deciding that a write has waited long enough is the app's or
    /// the person's, never a timer's.
    ///
    /// **Known imprecision, stated rather than hidden.** This is when the
    /// OBLIGATION was accepted, not when the stall began. For
    /// [`StalledWriteStage::Unroutable`] and
    /// [`StalledWriteStage::Unsignable`] the two coincide — routing and
    /// signing are attempted immediately, so a write that is parked has been
    /// parked since acceptance. For [`StalledWriteStage::Undeliverable`] the
    /// instant is EARLIER than the stall: a write accepted last week and
    /// delivering happily until its relay went down an hour ago still reads
    /// as accepted last week, so an app subtracting will over-report how
    /// long delivery has been failing.
    ///
    /// The alternative — the instant the park itself began — is a fact the
    /// store has no door for, and keeping it in memory instead is what makes
    /// a restart reset it to the recovering process's clock. Between a
    /// durable over-estimate and a process-local number that lies after every
    /// reopen, this surface takes the durable one, because "stalled since
    /// before the restart" is a question it has to be able to answer at all.
    /// Persisting the park instant is tracked as issue #1024.
    pub stalled_since: Timestamp,
}

/// The exact census behind [`DiagnosticsSnapshot::stalled_writes`]'s bounded
/// detail window.
///
/// Totals count every stalled obligation the reducer owns, including the
/// ones no detail row was emitted for, so a bound on memory is never a lie
/// about how much is stuck. Moving a write into or out of the detail window
/// changes no total.
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

/// The engine-global diagnostics snapshot (M5 plan §1.1) — "the acceptance
/// test rendered on screen, permanently." One snapshot covers every
/// currently-planned relay; there is no separate per-query diagnostics (that
/// is [`super::AcquisitionEvidence`], already delivered alongside every
/// `Effect::EmitRows`).
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsSnapshot {
    pub relays: Vec<RelayDiagnosticsSnapshot>,
    /// At most one entry per currently connected protected session.
    ///
    /// `#[doc(hidden)]`: the ENGINE owns this per-session AUTH read-out (#8
    /// U4 — the capstone and runtime falsifiers consume it). The documented,
    /// supported projection is the `nmp` facade's OWN mirror
    /// (`nmp::DiagnosticsSnapshot.auth_sessions`, #8 Wave 5), converted at
    /// its `DiagnosticsSubscription` delivery boundary; this engine-level
    /// field stays hidden because the engine snapshot is mechanism, not the
    /// app contract.
    #[doc(hidden)]
    pub auth_sessions: Vec<AuthDiagnosticsSnapshot>,
    pub uncovered_author_count: usize,
    pub dropped_merge_rules: Vec<&'static str>,
    /// Relay candidates refused by the single whole-demand ceiling, plus
    /// any defense-in-depth dial refusal at the transport boundary. The
    /// router contributes the deterministic plan-time count; the runtime
    /// adds `nmp_transport::Pool::admission_rejections()` before delivery.
    /// A refused router candidate is absent from the executable plan and its
    /// affected query atom carries `ShortfallFact::LocalLimit`.
    pub sessions_rejected_over_cap: u64,
    /// Sessions refused outright by a relay advertising ZERO concurrent
    /// subscriptions (#931). Counted apart from `sessions_rejected_over_cap`
    /// because they answer different questions — "the operator's plan was
    /// too wide" versus "this relay will hold nothing open" — and a reader
    /// that conflated them could not tell which bound to relax. A session
    /// merely trimmed by its budget is NOT counted here: it is present in
    /// `relays` with a non-zero `subscriptions_refused`.
    pub sessions_refused_by_subscription_budget: u64,
    /// `Some(message)` once an ingest/read [`nmp_store::RedbStore`] door has
    /// returned a [`nmp_store::PersistenceError`] (issue #122): the local
    /// cache has degraded to read-only and stopped accepting fresh
    /// ingest/reads. `None` while persistence is healthy. Read-only, off the
    /// data path — an observer-visible signal, never a routing input.
    pub store_degraded: Option<String>,
    /// Latest transport acceptance/verifier failure surfaced by the pool.
    /// Observational only; it never changes routing or trust policy.
    pub transport_degraded: Option<String>,
    /// Every durable write obligation that cannot progress, bounded to
    /// [`StalledWriteTotals::detail_limit`] rows in a deterministic order
    /// (stage, then acceptance instant, then id) that no scheduler reads.
    ///
    /// A receipt answers "what happened to THIS write", which needs someone
    /// still holding it; nobody holds the receipt for the DM composed on a
    /// train three weeks ago, and that is exactly the write worth
    /// surfacing. Projected from the reducer state that transactionally owns
    /// the canonical durable facts — never a second retry ledger, and never
    /// a store scan at snapshot time.
    pub stalled_writes: Vec<StalledWrite>,
    /// Exact counts behind that window. See [`StalledWriteTotals`].
    pub stalled_write_totals: StalledWriteTotals,
}

/// Combine `diag` (subs/filters/lanes/authors_served — `nmp-router`-owned)
/// with `events_by_session_kind` (this crate's own counter) and per-(relay,
/// filter) coverage (`get_coverage`, read from the store) into one
/// [`DiagnosticsSnapshot`]. Called by `EngineCore::diagnostics_snapshot`.
///
/// Total by construction, because `degrade_store` builds a snapshot in order
/// to report a failure — so a failing coverage read cannot abort this. It
/// lands in [`DiagnosticsSnapshot::store_degraded`] instead, which is what
/// keeps the empty `coverage` entry beside it readable as "unknown" rather
/// than "nothing proven" (#763).
pub(crate) fn build(
    diag: &Diagnostics,
    plan: &RelayPlan,
    events_by_session_kind: &HashMap<RelaySessionKey, BTreeMap<u16, u64>>,
    get_coverage: impl Fn(&RelayUrl, CoverageKey) -> Result<Option<CoverageInterval>, PersistenceError>,
) -> DiagnosticsSnapshot {
    // A coverage read that could not answer is kept as the snapshot's own
    // store-degradation fact rather than rendered as `coverage: None`
    // (#763). This snapshot has to stay total -- `degrade_store` builds one
    // to REPORT a failure -- so the read failure travels in the field an app
    // already reads for exactly this (#122/#745), never as a second health
    // noun and never as absent coverage.
    let mut coverage_unreadable: Option<String> = None;
    let mut relays = Vec::new();
    for (session, rd) in &diag.per_session {
        let filters: Vec<String> = rd.filters.iter().map(|f| f.to_nostr().as_json()).collect();

        // `plan.reqs` (not `rd.filters`) is the source of the per-filter
        // coverage list: it carries the SAME filters (a `RelayDiagnostics`
        // is built straight off the same `RelayPlan`, `diag::build`'s own
        // per-session loop), but iterating the plan directly needs no second
        // lookup to re-associate each filter with its `ConcreteFilter`
        // value for `get_coverage`.
        let coverage: Vec<FilterCoverageEntry> = plan
            .reqs
            .get(session)
            .into_iter()
            .flatten()
            .map(|req| {
                let text = req.filter.to_nostr().as_json();
                let coverage = match request_coverage(&session.relay, req, &get_coverage) {
                    Ok(coverage) => coverage,
                    Err(error) => {
                        coverage_unreadable.get_or_insert_with(|| error.to_string());
                        None
                    }
                };
                FilterCoverageEntry {
                    filter: text,
                    coverage,
                }
            })
            .collect();

        let events_by_kind: Vec<(u16, u64)> = events_by_session_kind
            .get(session)
            .into_iter()
            .flat_map(|m| m.iter().map(|(&k, &v)| (k, v)))
            .collect();

        relays.push(RelayDiagnosticsSnapshot {
            relay: session.relay.clone(),
            access: session.access,
            wire_sub_count: rd.wire_sub_count,
            subscription_budget: rd.subscription_budget,
            subscriptions_refused: rd.subscriptions_refused,
            subid_length_limit: rd.subid_length_limit,
            subid_length_rejects_our_ids: rd.subid_length_rejects_our_ids,
            authors_served: rd.authors_served,
            by_lane: rd.by_lane.iter().map(|(&l, &c)| (l, c)).collect(),
            filters,
            events_by_kind,
            coverage,
            nip11_supported_nips: None,
            nip11_document_revision: None,
            nip11_freshness: None,
            nip11_last_error: None,
            nip77_advertisement: "unknown",
            nip77_behavior: "unknown",
            nip77_handoff: "none",
        });
    }

    DiagnosticsSnapshot {
        relays,
        auth_sessions: Vec::new(),
        uncovered_author_count: diag.uncovered_authors.len(),
        dropped_merge_rules: diag.dropped_merge_rules.clone(),
        sessions_rejected_over_cap: u64::try_from(diag.sessions_refused_by_cap).unwrap_or(u64::MAX),
        sessions_refused_by_subscription_budget: u64::try_from(
            diag.sessions_refused_by_subscription_budget,
        )
        .unwrap_or(u64::MAX),
        // A coverage read that failed WHILE building this snapshot is set
        // here; `EngineCore::diagnostics_snapshot` then lets the reducer's
        // own latched #122 error win if it holds one. Either way the field
        // is non-`None` whenever a `coverage` entry above is empty because
        // the store could not be read.
        store_degraded: coverage_unreadable,
        transport_degraded: None,
        // Filled in by `EngineCore::diagnostics_snapshot` from the reducer's
        // own pending-obligation set: `build` sees only router/store read
        // facts and has no notion of the write plane.
        stalled_writes: Vec::new(),
        stalled_write_totals: StalledWriteTotals::default(),
    }
}

/// How many stalled-write detail rows one snapshot may carry.
///
/// A bound on bytes, not a scheduler policy: which rows land inside it
/// changes nothing about retry order, wake deadlines, receipt retention, or
/// transport/signer lifetime, and [`StalledWriteTotals`] stays exact
/// regardless of where the cut falls.
pub(crate) const STALLED_WRITE_DETAIL_LIMIT: usize = 64;

/// The stable, non-round-trippable descriptor of one stalled obligation.
///
/// Domain-separated so it can never collide with another BLAKE3 descriptor
/// on this surface, and derived from two DURABLE facts (the store-allocated
/// intent id and the frozen body's id) so a crash/reopen reproduces it
/// exactly. Two receipts accepted for byte-identical events get different
/// intent ids and therefore different descriptors.
pub(crate) fn stalled_write_id(intent_id: u64, frozen: &EventId) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"nmp:stalled-write:v1\0");
    hasher.update(&intent_id.to_be_bytes());
    hasher.update(frozen.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// The exact common interval proven for a (possibly coalesced) wire request.
/// Attribution persists evidence under every narrow atom key in
/// `WireReq::coverage_claims`, never under the widened filter's own hash. A wide
/// coalesced request is therefore proven only over the
/// intersection shared by ALL coverage_claims atoms; an absent atom row or disjoint
/// intervals yields `None` rather than fabricating a wire-filter watermark.
fn request_coverage(
    relay: &RelayUrl,
    req: &WireReq,
    get_coverage: &impl Fn(&RelayUrl, CoverageKey) -> Result<Option<CoverageInterval>, PersistenceError>,
) -> Result<Option<CoverageInterval>, PersistenceError> {
    let mut keys = req.coverage_claims.iter().copied();
    let Some(first_key) = keys.next() else {
        return Ok(None);
    };
    let Some(mut common) = get_coverage(relay, first_key)? else {
        return Ok(None);
    };
    for key in keys {
        let Some(next) = get_coverage(relay, key)? else {
            return Ok(None);
        };
        let intersection = CoverageInterval {
            from: common.from.max(next.from),
            through: common.through.min(next.through),
        };
        if intersection.from > intersection.through {
            return Ok(None);
        }
        common = intersection;
    }
    Ok(Some(common))
}
