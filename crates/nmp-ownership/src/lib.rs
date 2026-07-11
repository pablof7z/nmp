//! `nmp-ownership` -- the shared, dependency-light vocabulary for relay
//! routing + kind ownership (`docs/design/routing-and-ownership.md` Parts
//! B/C). Every future `nmp-mod-*` protocol crate depends on this crate to
//! declare a [`KindClaim`] (+ optional [`RoutePolicy`]) WITHOUT linking
//! the whole router -- the modularity north star (owner-resolved Q7,
//! `docs/design/routing-build-plan.md` §7.1: putting these types in
//! `nmp-router` would force every module to depend on the router).
//!
//! Types only: no routing logic, no relay directory, no engine wiring.
//! Zero dependencies on purpose -- not even `nostr` (kinds are plain
//! `u16` here, same as the wire).
//!
//! Adoption -- wiring these types into `nmp-router`/`nmp-engine`, or
//! building a real `nmp-mod-*` crate against them -- is Units E/F/G, not
//! this crate.

mod kind_claim;
mod kind_scope;
mod module_id;
mod relay_source;
mod route_class;
mod route_policy;

pub use kind_claim::KindClaim;
pub use kind_scope::KindScope;
pub use module_id::ModuleId;
pub use relay_source::{PinnedLane, RelaySource};
pub use route_class::RouteClass;
pub use route_policy::{AppLanes, FailMode, RoutePolicy};
