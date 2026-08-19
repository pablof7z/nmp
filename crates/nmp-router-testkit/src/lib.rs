//! Static routing-fact fixtures for `nmp-router`'s own tests, and for any
//! other crate's tests that need a value snapshot of [`RoutingFacts`]
//! instead of the engine's mutable production store.
//!
//! This crate exists so these fixtures are never part of `nmp-router`'s own
//! shipped public API (#1667): `nmp-router` has no way to gate a plain `pub`
//! item, and `nmp-router`'s own integration tests under `tests/` compile as
//! a separate crate, so they cannot reach anything gated with
//! `#[cfg(test)]` inside `nmp-router`'s `src/`. A dedicated dev-dependency
//! crate is the only boundary that both excludes these fixtures from
//! `nmp-router`'s production surface and stays reachable from every
//! consumer's tests.

use std::collections::BTreeMap;

use nmp_router::{AuthorRouteState, AuthorRoutes, PublicKey, RelayUrl, RoutingFacts};

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

