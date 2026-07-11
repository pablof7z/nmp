//! `nmp-router` — the per-relay compiler + router + coalescing + diagnostics
//! (M2). See `docs/plans/M2-compiler-router-plan.md` for the full spec this
//! crate implements.
//!
//! The compiler is a **pure function of `(demand set, injected relay
//! facts)`** — this crate depends only on `nmp-grammar` (for
//! `ConcreteFilter`/`DescriptorHash`) and `nostr` (for `RelayUrl` and
//! `Filter::match_event`). It does NOT depend on `nmp-resolver` or
//! `nmp-store` in its library; `nmp-resolver` is a dev-dependency used only
//! by the integration tests (differential oracle, Drop-nit, kill
//! measurement) that wire the real resolver into the router.
//!
//! Module layout:
//! - `facts` — `Lane`, `LanedRelay`, `RelayDirectory` trait, `FixtureDirectory`,
//!   `RelayLimits`, `DiscoveryKinds`.
//! - `route` — atom classification (outbox vs pinned) + candidate assembly +
//!   pinned-route lookup.
//! - `solver` — the 2-relay-min + cap coverage solver (greedy set-cover) +
//!   shortfall reporting.
//! - `coalesce` — exact-canonical dedup + the widen-only `MergeRule` registry.
//! - `plan` — `RelayPlan`, `WireReq`, `SubId`, `WireOp`/`WireDelta`, plan
//!   diffing.
//! - `deliver` — the local re-filter + the headless delivery model used by
//!   the differential oracle.
//! - `diag` — `Diagnostics`: the four-lane, reverse-coverage, exact-filter
//!   read-only projection of a compiled plan.
//! - `router` — `Router`: `compile(demand, dir) -> WireDelta`, owning
//!   `prev_plan` + diagnostics.

mod coalesce;
mod deliver;
mod diag;
mod facts;
mod plan;
mod route;
mod router;
mod solver;

pub use coalesce::{AuthorUnion, DiscardSecondOperand, KindUnion, MergeRule, RuleRegistry};
pub use deliver::deliver;
pub use diag::{Diagnostics, RelayDiagnostics};
pub use facts::{
    test_relay, DiscoveryKinds, FixtureDirectory, Lane, LanedRelay, LiveDirectory, PubkeyHex,
    RelayDirectory, RelayLimits, RelayUrl,
};
pub use plan::{diff_plans, RelayPlan, SubId, WireDelta, WireOp, WireReq};
pub use route::{RouteKind, RouteProvenance, Skeleton};
pub use router::Router;
pub use solver::{solve, Coverage, CoverageInput, Shortfall, ShortfallReason};
