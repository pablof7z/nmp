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

use nostr::{JsonUtil, RelayUrl};

use nmp_grammar::ConcreteFilter;
use nmp_router::{Diagnostics, Lane, RelayPlan};
use nmp_store::CoverageInterval;

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
    pub coverage: Option<CoverageInterval>,
}

/// One relay's full diagnostics: wire-sub count, lane breakdown, reverse
/// coverage (authors served), the exact filters currently sent, events
/// actually received per kind, and per-filter coverage state.
#[derive(Debug, Clone)]
pub struct RelayDiagnosticsSnapshot {
    pub relay: RelayUrl,
    pub wire_sub_count: usize,
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
}

/// The engine-global diagnostics snapshot (M5 plan §1.1) — "the acceptance
/// test rendered on screen, permanently." One snapshot covers every
/// currently-planned relay; there is no separate per-query diagnostics (that
/// is [`super::AcquisitionEvidence`], already delivered alongside every
/// `Effect::EmitRows`).
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsSnapshot {
    pub relays: Vec<RelayDiagnosticsSnapshot>,
    pub uncovered_author_count: usize,
    pub dropped_merge_rules: Vec<&'static str>,
}

/// Combine `diag` (subs/filters/lanes/authors_served — `nmp-router`-owned)
/// with `events_by_relay_kind` (this crate's own counter) and per-(relay,
/// filter) coverage (`get_coverage`, read from the store) into one
/// [`DiagnosticsSnapshot`]. Pure — no mutation, no I/O; called by
/// `EngineCore::diagnostics_snapshot`.
pub(crate) fn build(
    diag: &Diagnostics,
    plan: &RelayPlan,
    events_by_relay_kind: &HashMap<RelayUrl, BTreeMap<u16, u64>>,
    get_coverage: impl Fn(&RelayUrl, &ConcreteFilter) -> Option<CoverageInterval>,
) -> DiagnosticsSnapshot {
    let mut relays = Vec::new();
    for (relay, rd) in &diag.per_relay {
        let filters: Vec<String> = rd.filters.iter().map(|f| f.to_nostr().as_json()).collect();

        // `plan.reqs` (not `rd.filters`) is the source of the per-filter
        // coverage list: it carries the SAME filters (a `RelayDiagnostics`
        // is built straight off the same `RelayPlan`, `diag::build`'s own
        // per-relay loop), but iterating the plan directly needs no second
        // lookup to re-associate each filter with its `ConcreteFilter`
        // value for `get_coverage`.
        let coverage: Vec<FilterCoverageEntry> = plan
            .reqs
            .get(relay)
            .into_iter()
            .flatten()
            .map(|req| {
                let text = req.filter.to_nostr().as_json();
                FilterCoverageEntry {
                    filter: text,
                    coverage: get_coverage(relay, &req.filter),
                }
            })
            .collect();

        let events_by_kind: Vec<(u16, u64)> = events_by_relay_kind
            .get(relay)
            .into_iter()
            .flat_map(|m| m.iter().map(|(&k, &v)| (k, v)))
            .collect();

        relays.push(RelayDiagnosticsSnapshot {
            relay: relay.clone(),
            wire_sub_count: rd.wire_sub_count,
            authors_served: rd.authors_served,
            by_lane: rd.by_lane.iter().map(|(&l, &c)| (l, c)).collect(),
            filters,
            events_by_kind,
            coverage,
        });
    }

    DiagnosticsSnapshot {
        relays,
        uncovered_author_count: diag.uncovered_authors.len(),
        dropped_merge_rules: diag.dropped_merge_rules.clone(),
    }
}
