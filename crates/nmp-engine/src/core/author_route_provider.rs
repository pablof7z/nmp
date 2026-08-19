//! The construction-time adapter seam for author-route discovery.
//!
//! [`RoutingFacts`](nmp_router::RoutingFacts) is the PULL side: the reducer
//! reads it synchronously while compiling routes, so it stays engine-owned
//! and concrete — foreign code inside the deterministic reducer is exactly
//! what this crate's manifest exists to forbid. This module is the PUSH
//! side: an application-supplied algorithm, driven by the runtime loop (the
//! async edge, where foreign code belongs), feeding the reducer through its
//! one neutral writer,
//! [`CoreState::replace_author_routes`](super::CoreState::replace_author_routes).
//!
//! The three moments below are the whole contract. Nothing here carries a
//! handle, an id, or a lifecycle: a provider is fixed for the engine's life,
//! chosen at construction, and there is deliberately no way to register,
//! replace, or unregister one. "Swap algorithms" is spelled: drop the
//! engine, construct it with the other provider.
//!
//! Exactly ONE provider, by construction — the slot is an `Option`, not a
//! `Vec`. `replace_author_routes` replaces the complete directional fact for
//! an author in one call, so two providers would silently last-write-win
//! with no merge rule anyone could state. An application that wants to
//! combine algorithms writes a combinator provider; composition is
//! provider-author policy, refused as engine policy.

use std::collections::BTreeSet;

use nmp_grammar::LiveQuery;
use nostr::PublicKey;

use nostr::RelayUrl;

use super::{AuthorRouteReplacement, RowDelta};

/// What the loop must do to the provider's observation after the reducer's
/// need set changed.
///
/// Three variants rather than `Option<LiveQuery>`, deliberately. "The author
/// set did not change" and "the author set changed but asks no question" are
/// different instructions: the first must leave the current observation
/// alone, the second must close it. Collapsing them into one `None` would
/// skip the close in the second case and leak the observation.
pub enum ProviderReroot {
    /// The needed author set is unchanged for this provider. Touch nothing.
    Unchanged,
    /// Re-rooted onto an author set that asks no question. Close the current
    /// observation and open nothing.
    Closed,
    /// Re-rooted. Close the current observation, then open this ordinary
    /// query — the same door `Handle::subscribe` uses.
    Reopened(LiveQuery),
}

/// One neutral author-route replacement the loop applies through
/// [`CoreState::replace_author_routes`](super::CoreState::replace_author_routes).
///
/// The replacement is TOTAL per author: both directions at once. A provider
/// cannot express "merge this relay in", so the fact store never holds a
/// blend whose provenance nobody can state.
/// [`AuthorRouteReplacement::Absent`] is a settled negative, which is a
/// different fact from "nothing known yet" — and it is memory-only, so every
/// provider is re-asked after every boot.
pub struct AuthorRouteUpdate {
    pub author: PublicKey,
    pub replacement: AuthorRouteReplacement,
}

/// A construction-time source of neutral author routes, driven by the
/// runtime loop.
///
/// Implement this to supply a different routing algorithm — NMP's own
/// NIP-65 outbox model is `nmp-outbox`, which has no privileges this trait
/// does not give every other implementor. The trait is synchronous: a
/// provider answers from what it already knows and/or asks the engine to
/// observe an ordinary [`LiveQuery`] on its behalf. It never performs I/O
/// itself, and it never sees an observation handle — the loop owns that
/// bookkeeping, so a provider cannot mint, keep, or confuse one.
///
/// Nothing here converts ignorance into a verdict. A provider that does not
/// know an author's routes says nothing, the author stays
/// [`AuthorRouteState::Unknown`](nmp_router::AuthorRouteState::Unknown), and
/// pending work parks on knowledge rather than on a clock.
pub trait AuthorRouteProvider: Send {
    /// The reducer's author-route need set changed. The provider may answer
    /// immediately from what it already holds (a static table, a cache, an
    /// app-curated directory) and/or ask for an observation.
    fn reroot(&mut self, needs: BTreeSet<PublicKey>) -> (ProviderReroot, Vec<AuthorRouteUpdate>);

    /// Rows delivered on the observation this provider asked for. Never
    /// called for a provider that opened none.
    fn observe_rows(&mut self, rows: &[RowDelta]) -> Vec<AuthorRouteUpdate>;

    /// One source relay of the observation this provider asked for answered
    /// its REQ in full. This is how a provider learns that an author's routes
    /// are genuinely absent rather than merely unseen: absence is only
    /// reportable once every source it asked has settled.
    fn observe_request_settled(&mut self, relay: &RelayUrl) -> Vec<AuthorRouteUpdate>;
}
