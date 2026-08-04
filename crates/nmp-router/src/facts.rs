//! Closed, protocol-neutral routing facts consumed by [`crate::Router`].
//!
//! The router is deliberately read-only. Protocol components may discover
//! facts, but the mutable capability that installs them belongs to the engine
//! assembly and is not part of this crate's public vocabulary.

use std::collections::{BTreeMap, BTreeSet};

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
/// All three sets are replaced together. Empty sets remain positive knowledge
/// and therefore differ from both [`AuthorRouteState::Unknown`] and
/// [`AuthorRouteState::Absent`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorRoutes {
    outbound: BTreeSet<RelayUrl>,
    inbound: BTreeSet<RelayUrl>,
    refused: BTreeSet<RelayUrl>,
}

impl AuthorRoutes {
    /// A fact whose every declared relay was admitted.
    pub fn new(
        outbound: impl IntoIterator<Item = RelayUrl>,
        inbound: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        Self {
            outbound: outbound.into_iter().collect(),
            inbound: inbound.into_iter().collect(),
            refused: BTreeSet::new(),
        }
    }

    /// Record the relays this author declared that admission did not admit.
    ///
    /// This is what keeps a refused relay distinguishable from an absent one
    /// (#1251). Without it, an author whose whole list is on their own LAN is
    /// byte-identical to an author who declared no relays, so the only honest
    /// thing a reader can say — "they have relays, we would not use them, and
    /// here is which" — cannot be said at all.
    #[must_use]
    pub fn with_refused(mut self, refused: impl IntoIterator<Item = RelayUrl>) -> Self {
        self.refused = refused.into_iter().collect();
        self
    }

    pub fn outbound(&self) -> &BTreeSet<RelayUrl> {
        &self.outbound
    }

    pub fn inbound(&self) -> &BTreeSet<RelayUrl> {
        &self.inbound
    }

    /// The relays this author declared that were not admitted. Routing never
    /// reads this — a refused relay is exactly as unroutable as one that was
    /// never declared — but a reader that must explain an empty route set
    /// cannot do it from the routable sets alone.
    pub fn refused(&self) -> &BTreeSet<RelayUrl> {
        &self.refused
    }

    /// Whether this author declared relays and NONE of them were admitted.
    ///
    /// The exact shape an app has to distinguish from "declared nothing":
    /// both have no destinations, and only one of them means the user should
    /// be told their own relays were turned away.
    #[must_use]
    pub fn every_declared_relay_was_refused(&self) -> bool {
        !self.refused.is_empty() && self.outbound.is_empty() && self.inbound.is_empty()
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

/// Static routing facts for router tests and pure callers.
///
/// This is a value snapshot, not the engine's mutable production store.
#[derive(Default, Clone)]
pub struct FixtureRoutingFacts {
    authors: BTreeMap<PublicKey, AuthorRouteState>,
    app: Vec<RelayUrl>,
    fallback: Vec<RelayUrl>,
}

impl FixtureRoutingFacts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_author_routes(
        mut self,
        author: PublicKey,
        outbound: impl IntoIterator<Item = RelayUrl>,
        inbound: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        self.authors.insert(
            author,
            AuthorRouteState::Present(AuthorRoutes::new(outbound, inbound)),
        );
        self
    }

    /// Add an outbound-only author fact to a static test snapshot.
    pub fn with_outbound_routes(
        self,
        author: PublicKey,
        outbound: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        self.with_author_routes(author, outbound, [])
    }

    /// Add an inbound-only author fact to a static test snapshot.
    pub fn with_inbound_routes(
        self,
        author: PublicKey,
        inbound: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        self.with_author_routes(author, [], inbound)
    }

    pub fn with_author_absent(mut self, author: PublicKey) -> Self {
        self.authors.insert(author, AuthorRouteState::Absent);
        self
    }

    pub fn with_operator_app(mut self, relays: impl IntoIterator<Item = RelayUrl>) -> Self {
        self.app.extend(relays);
        self
    }

    pub fn with_operator_fallback(mut self, relays: impl IntoIterator<Item = RelayUrl>) -> Self {
        self.fallback.extend(relays);
        self
    }

    pub fn disjoint_mailboxes(authors: &[PublicKey]) -> Self {
        let mut facts = Self::new();
        for (index, author) in authors.iter().enumerate() {
            facts = facts.with_author_routes(
                *author,
                [test_relay(index * 2), test_relay(index * 2 + 1)],
                [],
            );
        }
        facts
    }

    pub fn shared_pool_mailboxes(authors: &[PublicKey], pool: &[RelayUrl]) -> Self {
        let mut facts = Self::new();
        for author in authors {
            facts = facts.with_author_routes(*author, pool.iter().cloned(), []);
        }
        facts
    }

    pub fn prolific_author(author: PublicKey, n: usize) -> Self {
        Self::new().with_author_routes(author, (0..n).map(test_relay), [])
    }

    /// Decompose this static fixture for a headless engine falsifier.
    #[doc(hidden)]
    pub fn into_parts(
        self,
    ) -> (
        BTreeMap<PublicKey, AuthorRouteState>,
        Vec<RelayUrl>,
        Vec<RelayUrl>,
    ) {
        (self.authors, self.app, self.fallback)
    }
}

impl RoutingFacts for FixtureRoutingFacts {
    fn author_routes(&self, author: &PublicKey) -> AuthorRouteState {
        self.authors.get(author).cloned().unwrap_or_default()
    }

    fn operator_app_relays(&self) -> Vec<RelayUrl> {
        self.app.clone()
    }

    fn operator_fallback_relays(&self) -> Vec<RelayUrl> {
        self.fallback.clone()
    }
}

/// A deterministic fixture relay URL (`wss://relay{n}.example.com`).
pub fn test_relay(n: usize) -> RelayUrl {
    RelayUrl::parse(&format!("wss://relay{n}.example.com")).expect("valid test relay url")
}

#[cfg(test)]
mod tests {
    use nostr::Keys;

    use super::*;

    #[test]
    fn lookup_preserves_all_three_states() {
        let present = Keys::generate().public_key();
        let absent = Keys::generate().public_key();
        let unknown = Keys::generate().public_key();
        let facts = FixtureRoutingFacts::new()
            .with_author_routes(present, [], [])
            .with_author_absent(absent);

        assert_eq!(
            facts.author_routes(&present),
            AuthorRouteState::Present(AuthorRoutes::default())
        );
        assert_eq!(facts.author_routes(&absent), AuthorRouteState::Absent);
        assert_eq!(facts.author_routes(&unknown), AuthorRouteState::Unknown);
    }

    #[test]
    fn one_fact_carries_both_directions() {
        let author = Keys::generate().public_key();
        let outbound = test_relay(1);
        let inbound = test_relay(2);
        let facts = FixtureRoutingFacts::new().with_author_routes(
            author,
            [outbound.clone()],
            [inbound.clone()],
        );

        let AuthorRouteState::Present(routes) = facts.author_routes(&author) else {
            panic!("expected present routes");
        };
        assert_eq!(routes.outbound(), &BTreeSet::from([outbound]));
        assert_eq!(routes.inbound(), &BTreeSet::from([inbound]));
    }
}
