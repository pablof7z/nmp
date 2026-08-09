//! `nmp-router` — the per-relay compiler + router + coalescing + diagnostics
//! (M2). See `docs/plans/M2-compiler-router-plan.md` for the full spec this
//! crate implements.
//!
//! The compiler is a **pure function of `(demand set, neutral routing
//! facts)`** — this crate depends only on `nmp-grammar` (for
//! `ConcreteFilter`/`DescriptorHash`) and `nostr` (for `RelayUrl` and
//! `Filter::match_event`). It does NOT depend on `nmp-resolver` or
//! `nmp-store` in its library; `nmp-resolver` is a dev-dependency used only
//! by the integration tests (differential oracle, Drop-nit, kill
//! measurement) that wire the real resolver into the router.
//!
//! Module layout:
//! - `facts` — the closed neutral route vocabulary, the read-only
//!   `RoutingFacts` view, and static test facts.
//! - `budget` — `CompileBudget`: the whole-demand relay ceiling plus each
//!   relay's own advertised NIP-11 limits, the bounds `compile` plans within.
//! - `route` — atom classification (outbox vs pinned) + candidate assembly +
//!   pinned-route lookup.
//! - `solver` — the 2-relay-min + cap coverage solver (greedy set-cover) +
//!   shortfall reporting.
//! - `component` — the structural component model of a `ConcreteFilter`,
//!   shared by `coalesce` (what may merge) and `wire_id` (what continues what).
//! - `coalesce` — exact-canonical dedup + the widen-only `MergeRule` registry.
//! - `plan` — `RelayPlan`, `WireReq`, `SubId`, `WireOp`/`WireDelta`, plan
//!   diffing.
//! - `deliver` — the local re-filter + the headless delivery model used by
//!   the differential oracle.
//! - `diag` — `Diagnostics`: the four-lane, reverse-coverage, exact-filter
//!   read-only projection of a compiled plan.
//! - `wire_id` — structural-signature matching: which previously-allocated
//!   wire subscription token a newly-compiled filter continues (#899).
//! - `router` / `admission` — `Router`: whole-demand invalidation plus
//!   pending-only immutable admission, owning the running plan, diagnostics,
//!   and wire-token mint counter.

mod admission;
mod budget;
mod coalesce;
mod component;
mod deliver;
mod diag;
mod facts;
mod plan;
mod route;
mod router;
mod solver;
mod wire_id;

pub use budget::{AdvertisedRelayLimits, CompileBudget, WIRE_SUB_ID_CHARS};
pub use coalesce::{
    DiscardSecondOperand, MergeRule, RuleRegistry, StructuralUnion, MAX_IDS_PER_FILTER,
    MAX_TAG_VALUES_PER_FILTER,
};
pub use deliver::deliver;
pub use diag::{Diagnostics, RelayDiagnostics};
pub use facts::{
    test_relay, AuthorRouteState, AuthorRoutes, FixtureRoutingFacts, Lane, LanedRelay, PublicKey,
    RelayUrl, RoutingFacts,
};
pub use plan::{
    diff_plans, BudgetShortfall, DemandKey, RelayPlan, SubId, WireDelta, WireOp, WireReq,
};
pub use route::{RouteKind, RouteProvenance, Skeleton};
pub use router::{Router, WithdrawalOutcome, WithdrawalWork};
pub use solver::{solve, Coverage, CoverageInput, Shortfall, ShortfallReason};
