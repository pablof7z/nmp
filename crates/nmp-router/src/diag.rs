//! [`Diagnostics`] — the acceptance-test-made-visible, read-only projection
//! of a compiled plan (M2 plan §2.6): per-relay sub counts, lane counts,
//! reverse coverage (authors served), the exact filters sent, uncovered
//! authors, dropped merge rules, and what each relay advertised about its own
//! limits.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ConcreteFilter, RelaySessionKey};

use crate::budget::CompileBudget;
use crate::facts::{Lane, PublicKey};
use crate::plan::RelayPlan;
use crate::solver::Shortfall;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayDiagnostics {
    pub session: RelaySessionKey,
    /// Concurrent subscriptions currently open on this session. THE durable
    /// contract of the subscription programme (#931): without a per-session
    /// count in diagnostics and in the acceptance suite, the next axis that
    /// escapes coalescing regresses silently and the whole exercise repeats.
    pub wire_sub_count: usize,
    pub by_lane: BTreeMap<Lane, usize>,
    /// Reverse coverage: distinct authors this relay covers.
    pub authors_served: usize,
    /// The EXACT filters sent to this relay.
    pub filters: Vec<ConcreteFilter>,
    /// What this relay advertised as `limitation.max_subscriptions`. `None`
    /// means it advertised nothing and is therefore UNBUDGETED — a
    /// distinction this never collapses into a fabricated number.
    pub subscription_budget: Option<usize>,
    /// Subscriptions this compile removed to stay inside
    /// `subscription_budget`. Every one of them is also reported as
    /// `limited` coverage, so the demand is refused visibly, never silently.
    pub subscriptions_refused: usize,
    /// What this relay advertised as `limitation.max_subid_length`.
    pub subid_length_limit: Option<usize>,
    /// True iff that advertised length is SHORTER than the 64-character ids
    /// NMP sends, i.e. this relay rejects every REQ we put on its socket.
    /// Diagnostic only — nothing here may ever reach id derivation.
    pub subid_length_rejects_our_ids: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub per_session: BTreeMap<RelaySessionKey, RelayDiagnostics>,
    pub uncovered_authors: BTreeMap<PublicKey, Shortfall>,
    /// Distinct candidates rejected by the one whole-demand relay ceiling.
    /// They are absent from `per_session` by construction.
    pub sessions_refused_by_cap: usize,
    /// Distinct sessions refused OUTRIGHT by a relay advertising zero
    /// concurrent subscriptions. Counted apart from the relay ceiling
    /// because the two answer different questions — "the operator's plan was
    /// too wide" versus "this relay will hold nothing open" — and a reader
    /// that conflated them could not tell which bound to relax. Also absent
    /// from `per_session`; a session merely TRIMMED by its budget is present
    /// there with a non-zero `subscriptions_refused`.
    pub sessions_refused_by_subscription_budget: usize,
    pub dropped_merge_rules: Vec<&'static str>,
}

pub(crate) fn build(
    plan: &RelayPlan,
    budget: &CompileBudget,
    uncovered_authors: BTreeMap<PublicKey, Shortfall>,
    dropped_merge_rules: Vec<&'static str>,
) -> Diagnostics {
    let mut per_session = BTreeMap::new();
    for (session, reqs) in &plan.reqs {
        let mut by_lane: BTreeMap<Lane, usize> = BTreeMap::new();
        let mut authors_served: BTreeSet<PublicKey> = BTreeSet::new();
        let mut filters = Vec::new();
        for req in reqs {
            filters.push(req.filter.clone());
            for prov in &req.provenance {
                *by_lane.entry(prov.lane).or_insert(0) += 1;
                authors_served.extend(prov.covers_authors.iter().cloned());
            }
        }
        per_session.insert(
            session.clone(),
            RelayDiagnostics {
                session: session.clone(),
                wire_sub_count: reqs.len(),
                by_lane,
                authors_served: authors_served.len(),
                filters,
                subscription_budget: budget.max_subscriptions(&session.relay),
                subscriptions_refused: plan
                    .subscription_shortfalls
                    .get(session)
                    .map_or(0, |shortfall| shortfall.refused),
                subid_length_limit: budget.max_subid_length(&session.relay),
                subid_length_rejects_our_ids: budget.rejects_our_subscription_ids(&session.relay),
            },
        );
    }
    let refused_by_budget = plan
        .refused_sessions
        .iter()
        .filter(|session| plan.subscription_shortfalls.contains_key(*session))
        .count();
    Diagnostics {
        per_session,
        uncovered_authors,
        sessions_refused_by_cap: plan.refused_sessions.len() - refused_by_budget,
        sessions_refused_by_subscription_budget: refused_by_budget,
        dropped_merge_rules,
    }
}

/// Refresh only sessions whose physical request set changed during ordinary
/// delta withdrawal. Global refusal counts are scalar projections over the
/// bounded refusal maps; untouched session diagnostics are retained byte for
/// byte.
pub(crate) fn refresh_sessions(
    diagnostics: &mut Diagnostics,
    plan: &RelayPlan,
    budget: &CompileBudget,
    touched: &BTreeSet<RelaySessionKey>,
) {
    for session in touched {
        let Some(reqs) = plan.reqs.get(session) else {
            diagnostics.per_session.remove(session);
            continue;
        };
        let mut by_lane: BTreeMap<Lane, usize> = BTreeMap::new();
        let mut authors_served: BTreeSet<PublicKey> = BTreeSet::new();
        let mut filters = Vec::new();
        for req in reqs {
            filters.push(req.filter.clone());
            for provenance in &req.provenance {
                *by_lane.entry(provenance.lane).or_insert(0) += 1;
                authors_served.extend(provenance.covers_authors.iter().cloned());
            }
        }
        diagnostics.per_session.insert(
            session.clone(),
            RelayDiagnostics {
                session: session.clone(),
                wire_sub_count: reqs.len(),
                by_lane,
                authors_served: authors_served.len(),
                filters,
                subscription_budget: budget.max_subscriptions(&session.relay),
                subscriptions_refused: plan
                    .subscription_shortfalls
                    .get(session)
                    .map_or(0, |shortfall| shortfall.refused),
                subid_length_limit: budget.max_subid_length(&session.relay),
                subid_length_rejects_our_ids: budget.rejects_our_subscription_ids(&session.relay),
            },
        );
    }
    let refused_by_budget = plan
        .refused_sessions
        .iter()
        .filter(|session| plan.subscription_shortfalls.contains_key(*session))
        .count();
    diagnostics.sessions_refused_by_cap = plan.refused_sessions.len() - refused_by_budget;
    diagnostics.sessions_refused_by_subscription_budget = refused_by_budget;
}
