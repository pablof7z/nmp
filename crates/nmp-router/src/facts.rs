//! Closed, protocol-neutral routing facts consumed by [`crate::Router`].
//!
//! The router is deliberately read-only. Protocol components may discover
//! facts, but the mutable capability that installs them belongs to the engine
//! assembly and is not part of this crate's public vocabulary.

use std::collections::BTreeSet;

pub use nostr::{PublicKey, RelayUrl};

/// The neutral source that caused a relay to enter a route.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Lane {
    /// A relay in the event author's authoritative outbound set.
    AuthorOutbound,
    /// A relay carried by a selector hint.
    Hint,
    /// A relay where matching events were observed previously.
    Provenance,
    /// Operator policy that supplements every route.
    OperatorApp,
    /// Operator policy for settled thin tagged-pubkey coverage.
    OperatorFallback,
    /// An exact relay declared by the operation/query itself.
    Exact,
}

/// A relay tagged with the neutral source that supplied it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LanedRelay {
    pub url: RelayUrl,
    pub lane: Lane,
}

impl LanedRelay {
    pub fn new(url: RelayUrl, lane: Lane) -> Self {
        Self { url, lane }
    }
}

/// One author's complete, authoritative directional routing fact.
///
/// Both sets are replaced together. Empty sets remain positive knowledge
/// and therefore differ from both [`AuthorRouteState::Unknown`] and
/// [`AuthorRouteState::Absent`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorRoutes {
    outbound: BTreeSet<RelayUrl>,
    inbound: BTreeSet<RelayUrl>,
}

impl AuthorRoutes {
    pub fn new(
        outbound: impl IntoIterator<Item = RelayUrl>,
        inbound: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        Self {
            outbound: outbound.into_iter().collect(),
            inbound: inbound.into_iter().collect(),
        }
    }

    pub fn outbound(&self) -> &BTreeSet<RelayUrl> {
        &self.outbound
    }

    pub fn inbound(&self) -> &BTreeSet<RelayUrl> {
        &self.inbound
    }
}

/// What the current process knows about one author's routes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AuthorRouteState {
    /// Cold start: no exact discovery sources have settled and no record was
    /// admitted.
    #[default]
    Unknown,
    /// A positive authoritative record, including a record with two empty
    /// sets.
    Present(AuthorRoutes),
    /// Every exact source carrying this session's question settled without a
    /// record. This state is memory-only and must not survive restart.
    Absent,
}

/// The complete read-only fact surface used by generic routing.
pub trait RoutingFacts {
    /// Total lookup: a missing key is [`AuthorRouteState::Unknown`].
    fn author_routes(&self, author: &PublicKey) -> AuthorRouteState;

    /// Operator-configured relays added independently to every non-exact
    /// route.
    fn operator_app_relays(&self) -> Vec<RelayUrl>;

    /// Operator-configured fallback relays. The write resolver decides when a
    /// settled thin tagged-pubkey contribution makes these eligible.
    fn operator_fallback_relays(&self) -> Vec<RelayUrl>;
}

// Static-fixture test facts and the `test_relay` helper live in
// `nmp-router-testkit` (#1667), not here: this crate has no way to gate a
// plain `pub` item, and that dev-only capability must never be part of what
// a default-features consumer of `nmp-router` can reach.
//
// This crate's OWN `#[cfg(test)]` unit tests (route.rs, admission_delta_tests.rs,
// admission/preview_tests.rs) cannot use that shared testkit crate for
// anything that crosses the `RoutingFacts` trait boundary: `nmp-router-testkit`
// depends on `nmp-router`, and a crate's `--cfg test` build is a distinct
// compilation from the plain build such a dev-dependency links against, so
// `impl RoutingFacts for FixtureRoutingFacts` (compiled against the plain
// build) never satisfies a `RoutingFacts` bound resolved against the
// under-test build ("multiple different versions of crate `nmp_router` in
// the dependency graph"). `test_relay` has no such conflict -- it returns a
// plain `nostr::RelayUrl`, no local trait involved -- so those same test
// modules keep using it from `nmp-router-testkit` directly.
//
// `LocalFacts` below is the minimal in-crate stand-in those three files use
// instead. It is not a copy of `FixtureRoutingFacts`'s public surface and
// must not grow into one; it exists for exactly the handful of call sites
// that need *some* `RoutingFacts` value to hand a router function, most of
// which just want the empty case.

