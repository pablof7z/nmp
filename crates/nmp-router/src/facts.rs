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

// This crate ships no fixture implementation of `RoutingFacts`. It has no way
// to gate a plain `pub` item, so anything it named here would be part of what
// a default-features consumer can reach.

