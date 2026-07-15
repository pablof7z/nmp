//! `nmp-audit` -- the routing-and-ownership.md §4.2 layer-2 workspace audit
//! (routing-build-plan.md §4), the "load-bearing layer" of kind-ownership
//! enforcement.
//!
//! This crate carries no production code. Its entire purpose is
//! `tests/workspace_audit.rs`, an integration test that:
//! - dev-depends on every claim-bearing module crate in the workspace and
//!   proves that set is exactly right against `cargo metadata` (a new
//!   module crate that forgets to enroll is a red build, not a silent
//!   gap);
//! - folds every enrolled module's `KindClaim`s through
//!   `nmp_ownership::ClaimSet::build` and asserts the fold succeeds (no
//!   exclusive-scope overlap anywhere in the workspace, including modules
//!   no app currently links);
//! - asserts route authority is a subset of ownership (every claim with a
//!   `route_policy` is `exclusive`);
//! - asserts every claim whose scope intersects `nmp_router::DiscoveryKinds`
//!   sets `discovery_ack: true`, and vice versa;
//! - falsifies the above properties against synthetic fixture claims (per
//!   routing-build-plan.md §4.3/§4.4), since there are zero real
//!   `nmp-mod-*` crates at this milestone (routing-build-plan.md §4.4).
//!
//! See `docs/design/routing-and-ownership.md` §4.2 and
//! `docs/design/routing-build-plan.md` §4 for the full spec this crate
//! enforces.
