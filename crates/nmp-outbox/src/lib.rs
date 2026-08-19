//! The outbox model, learned via NIP-65.
//!
//! One [`AuthorRouteProvider`] implementation: it asks the operator's
//! indexer relays for each needed author's kind:10002 relay list, selects
//! the canonical replaceable winner, and hands the engine the directional
//! routes it parsed. Absence is reported only once every exact source has
//! settled without a record.
//!
//! Three names, three things, and this crate is the middle one:
//!
//! - [`AuthorRouteProvider`] — the interface, declared by `nmp-engine`.
//! - `nmp-outbox` (here) — this algorithm. An application that wants a
//!   different one names a different crate; nothing in `nmp`, `nmp-engine`
//!   or `nmp-runtime` mentions this crate's name.
//! - `nmp-nip65` — the kind:10002 vocabulary, engine-free, which this crate
//!   consumes and does not re-export.

use std::collections::BTreeSet;

use nmp_engine::core::{
    AuthorRouteProvider, AuthorRouteReplacement, AuthorRouteUpdate, ProviderReroot, RowDelta,
};
use nmp_grammar::LiveQuery;
use nostr::{PublicKey, RelayUrl};

/// The NIP-65 outbox algorithm as an installable provider.
///
/// `sources` are the operator's indexer relays — the exact relays asked for
/// relay lists. NMP supplies no defaults: constructed with none, this
/// provider opens no query and every author stays `Unknown`, which is an
/// honest "nobody was asked" rather than a fabricated route.
pub struct Nip65Outbox {
    coordinator: nmp_nip65::Nip65Coordinator,
    /// The revision a settlement must cite to count. Bumped by the
    /// coordinator on every re-root, so evidence from a retired query
    /// settles nothing.
    revision: u64,
}

impl Nip65Outbox {
    pub fn new(sources: impl IntoIterator<Item = RelayUrl>) -> Self {
        Self {
            coordinator: nmp_nip65::Nip65Coordinator::new(sources),
            revision: 0,
        }
    }
}

impl AuthorRouteProvider for Nip65Outbox {
    fn reroot(&mut self, needs: BTreeSet<PublicKey>) -> (ProviderReroot, Vec<AuthorRouteUpdate>) {
        if self.coordinator.authors() == &needs {
            return (ProviderReroot::Unchanged, Vec::new());
        }
        // This algorithm answers nothing without asking the network, so the
        // immediate-update half is always empty here. A static or cached
        // provider is exactly where it would not be.
        match self.coordinator.reroot(needs) {
            Some(query) => {
                self.revision = query.revision;
                (
                    ProviderReroot::Reopened(LiveQuery::single(query.demand)),
                    Vec::new(),
                )
            }
            None => (ProviderReroot::Closed, Vec::new()),
        }
    }

    fn observe_rows(&mut self, rows: &[RowDelta]) -> Vec<AuthorRouteUpdate> {
        let removed = rows
            .iter()
            .filter_map(|row| match row {
                RowDelta::Removed(id) => Some(*id),
                RowDelta::Added(_) | RowDelta::Updated(_) | RowDelta::SourcesGrew { .. } => None,
            })
            .collect::<Vec<_>>();
        let events = rows
            .iter()
            .filter_map(|delta| delta.row().and_then(|row| row.signed_event()))
            .collect::<Vec<_>>();
        updates(self.coordinator.observe_current_delta(removed, events))
    }

    fn observe_request_settled(&mut self, relay: &RelayUrl) -> Vec<AuthorRouteUpdate> {
        updates(self.coordinator.settle(self.revision, relay))
    }
}

fn updates(coordinator_updates: Vec<nmp_nip65::CoordinatorUpdate>) -> Vec<AuthorRouteUpdate> {
    coordinator_updates
        .into_iter()
        .map(|update| match update {
            nmp_nip65::CoordinatorUpdate::Present { author, routes } => AuthorRouteUpdate {
                author,
                replacement: AuthorRouteReplacement::Present(nmp_router::AuthorRoutes::new(
                    routes.outbound,
                    routes.inbound,
                )),
            },
            nmp_nip65::CoordinatorUpdate::Absent { author } => AuthorRouteUpdate {
                author,
                replacement: AuthorRouteReplacement::Absent,
            },
        })
        .collect()
}
