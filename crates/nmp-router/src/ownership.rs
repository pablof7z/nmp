//! Exact logical, physical, refusal, and diagnostic ownership indexes.

use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, ReadRouting, RelaySessionKey};
use nmp_store::CoverageKey;

use crate::plan::{DemandKey, SubId, WireDelta};
use crate::{PublicKey, RouteProvenance, Shortfall};

mod instrumentation;
mod refusals;
mod request_contributions;
mod request_indexes;
mod uncovered;

pub(crate) use refusals::{refresh_refusal_diagnostics, refused_session_class};

pub(crate) type RequestKey = (RelaySessionKey, SubId);
pub(crate) type ExactFilterKey = (RelaySessionKey, ReadRouting, ConcreteFilter);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RequestOwnerContribution {
    pub(crate) coverage_claims: BTreeSet<CoverageKey>,
    pub(crate) coverage_assignments: BTreeSet<(DemandKey, PublicKey)>,
    pub(crate) provenance: BTreeSet<RouteProvenance>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RequestContributionDelta {
    pub(crate) owner_added: bool,
    pub(crate) coverage_claims: BTreeSet<CoverageKey>,
    pub(crate) coverage_assignments: BTreeSet<(DemandKey, PublicKey)>,
    pub(crate) provenance: BTreeSet<RouteProvenance>,
}

/// Public diagnostics collapse multiple exact DemandKey-owned shortfalls for
/// one author to the strongest live deficit. A larger missing relay count
/// wins; equal deficits prefer intrinsic absence, then operator-cap loss,
/// then a naturally undersized candidate set. This is independent of hash or
/// DemandKey iteration order and is shared by full compile and delta updates.
fn strongest_shortfall<'a>(facts: impl IntoIterator<Item = &'a Shortfall>) -> Option<Shortfall> {
    let reason_priority = |fact: &Shortfall| match fact.reason {
        crate::ShortfallReason::NoCandidates => 3u8,
        crate::ShortfallReason::CapExhausted => 2,
        crate::ShortfallReason::FewerCandidatesThanK => 1,
    };
    facts.into_iter().copied().max_by_key(|fact| {
        (
            fact.requested_k.saturating_sub(fact.achieved),
            reason_priority(fact),
            fact.requested_k,
            std::cmp::Reverse(fact.achieved),
        )
    })
}

/// Reduce one exact `(DemandKey, author)` assignment state to its truthful
/// public shortfall. Supplemental app/fallback ownership never enters the
/// `achieved` count: callers derive it only from typed coverage assignments.
pub(crate) fn reduce_outbox_shortfall(
    intrinsic: Option<Shortfall>,
    achieved: usize,
    coverage_refused_by_local_limit: bool,
) -> Option<Shortfall> {
    const REQUIRED: usize = 2;
    if achieved >= REQUIRED {
        return None;
    }
    if let Some(mut fact) = intrinsic {
        fact.achieved = achieved;
        if achieved > 0 && fact.reason == crate::ShortfallReason::NoCandidates {
            fact.reason = crate::ShortfallReason::FewerCandidatesThanK;
        }
        return Some(fact);
    }
    coverage_refused_by_local_limit.then_some(Shortfall {
        requested_k: REQUIRED,
        achieved,
        reason: crate::ShortfallReason::CapExhausted,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefusalKind {
    RelayCap,
    SubscriptionBudget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefusalOwner {
    pub(crate) refusal_kind: RefusalKind,
    pub(crate) request: Option<SubId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefusedSessionClass {
    RelayCap,
    SubscriptionBudget,
}

/// Exact work performed by pending-only admission.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmissionWork {
    pub cohort_compiles: u64,
    pub incumbent_active_entries_visited: u64,
    pub incumbent_plan_requests_visited: u64,
    pub incumbent_limited_entries_visited: u64,
    pub incumbent_refusal_entries_visited: u64,
    pub active_entries_appended: u64,
    pub request_edges_appended: u64,
    pub metadata_entries_examined: u64,
}

/// Exact metadata reconciliation work performed by whole-plan compilation.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FullMetadataWork {
    pub requests_probed: u64,
    pub candidate_entries_examined: u64,
    pub owner_edges_visited: u64,
    pub assignment_edges_visited: u64,
    pub provenance_author_edges_visited: u64,
    pub diagnostic_provenance_edges_visited: u64,
}

/// Exact work performed by ordinary delta-driven withdrawal.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WithdrawalWork {
    pub dropped_atoms: u64,
    pub request_edges_touched: u64,
    pub metadata_owner_entries_touched: u64,
    pub metadata_claim_entries_touched: u64,
    pub metadata_assignment_entries_touched: u64,
    pub metadata_provenance_entries_touched: u64,
    pub plan_request_entries_visited: u64,
    pub requests_closed: u64,
    pub physical_coverage_edges_released: u64,
    pub diagnostic_rebuilds: u64,
    pub diagnostic_requests_visited: u64,
}

pub struct AdmissionOutcome {
    pub wire: WireDelta,
    pub changed_coverage: BTreeSet<CoverageKey>,
    pub diagnostics_changed: bool,
    pub request_metadata_updates: Vec<RequestMetadataUpdate>,
}

/// Read-only result of evaluating one pending cohort against the immutable
/// requests and residual capacity already owned by this router.
#[doc(hidden)]
pub struct AdmissionPreview {
    pub plan: crate::RelayPlan,
    pub work: AdmissionPreviewWork,
}

/// Exact candidate-local work performed by [`crate::Router::preview_admission`].
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmissionPreviewWork {
    pub candidate_atoms: u64,
    pub incumbent_demand_edges_visited: u64,
    pub incumbent_request_entries_visited: u64,
    pub coalesce_pair_attempts: u64,
}

pub struct CompileOutcome {
    pub wire: WireDelta,
    pub request_metadata_updates: Vec<RequestMetadataUpdate>,
    pub replacements: BTreeSet<RequestReplacement>,
}

/// One accepted-open-before-close transition in a full compile (#774).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestReplacement {
    pub session: RelaySessionKey,
    pub prior_sub_id: SubId,
    pub next_sub_id: SubId,
}

/// Metadata attached locally to one byte-identical incumbent request.
///
/// The transport request remains immutable. Core consumes this transition to
/// extend only the current execution generation and durable claim ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMetadataUpdate {
    pub session: RelaySessionKey,
    pub sub_id: SubId,
    pub filter_hash: nmp_grammar::DescriptorHash,
    pub added_coverage_claims: BTreeSet<CoverageKey>,
    pub added_owner_demands: BTreeSet<DemandKey>,
}

/// Local ownership removed from one still-running immutable request.
///
/// The wire filter and subscription id do not change. Core prunes the exact
/// current pending or accepted claim and owner membership, along with the
/// future/reconnect metadata, while leaving older overwritten generations
/// untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMetadataRemoval {
    pub session: RelaySessionKey,
    pub sub_id: SubId,
    pub filter_hash: nmp_grammar::DescriptorHash,
    pub removed_coverage_claims: BTreeSet<CoverageKey>,
    pub removed_owner_demands: BTreeSet<DemandKey>,
}

pub struct WithdrawalOutcome {
    pub wire: WireDelta,
    pub changed_coverage: BTreeSet<CoverageKey>,
    pub diagnostics_changed: bool,
    pub request_metadata_removals: Vec<RequestMetadataRemoval>,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RouterOwnershipCensus {
    pub active_demands: usize,
    pub requests_by_demand_keys: usize,
    pub requests_by_demand_edges: usize,
    pub active_by_request: usize,
    pub request_coverage_keys: usize,
    pub request_position_keys: usize,
    pub request_exact_filter_keys: usize,
    pub physical_request_claim_keys: usize,
    pub physical_claim_keys: usize,
    pub physical_claim_edges: usize,
    pub physical_request_contribution_keys: usize,
    pub physical_demand_keys: usize,
    pub physical_demand_edges: usize,
    pub request_owner_contribution_keys: usize,
    pub request_claim_owner_count_keys: usize,
    pub request_provenance_owner_count_keys: usize,
    pub request_demand_coverage_owner_count_keys: usize,
    pub coverage_assignment_keys: usize,
    pub coverage_assignment_edges: usize,
    pub refused_coverage_assignment_demands: usize,
    pub refused_coverage_assignment_authors: usize,
    pub active_outbox_authors: usize,
    pub refusal_demand_keys: usize,
    pub refusal_demand_edges: usize,
    pub refused_request_owner_keys: usize,
    pub refused_session_owner_keys: usize,
    pub diagnostic_author_session_keys: usize,
    pub diagnostic_author_edges: usize,
    pub uncovered_demand_keys: usize,
    pub uncovered_author_keys: usize,
    pub uncovered_author_refs: usize,
    pub plan_sessions: usize,
    pub plan_requests: usize,
    pub plan_limited_demands: usize,
    pub plan_refused_sessions: usize,
    pub plan_subscription_shortfalls: usize,
    pub diagnostic_sessions: usize,
    pub diagnostic_uncovered_authors: usize,
    pub diagnostic_sessions_refused_by_cap: usize,
    pub diagnostic_sessions_refused_by_subscription_budget: usize,
    pub diagnostic_dropped_merge_rules: usize,
}
