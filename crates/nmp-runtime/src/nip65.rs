//! NIP-65 route-source glue, beside the loop that drives it.
//!
//! Protocol values stay engine-free in `nmp-nip65`. This module converts
//! between those values and the loop's neutral vocabulary, and does nothing
//! else: it holds no `EngineCore`, issues no `EngineMsg`, and produces no
//! `Effect`. The loop owns every reducer call, so the reducer's mutation
//! doors stay `pub(crate)` rather than becoming a package API (#1142 §2).
//!
//! This module lives here rather than in `crate::nip65` because the facade
//! module sits ABOVE the engine: glue placed there makes the engine depend
//! upward, which is a package cycle rather than a policy violation. Beside
//! the loop, the only edge is `runtime -> nmp-nip65`, which is downward.
//! The facade keeps what is genuinely app-facing -- the bootstrap publish
//! door and the `nmp_nip65` re-exports.

use std::collections::BTreeSet;

use nmp_grammar::LiveQuery;
use nostr::{PublicKey, RelayUrl};

use nmp_engine::core::{ObservationEvidence, ObservationFact, ObservationId, RowDelta};

/// What the loop must do to the route-source observation after a re-root.
///
/// Three variants rather than `Option<LiveQuery>`, deliberately. "The author
/// set did not change" and "the author set changed but supplies no query" are
/// different instructions to the caller: the first must leave the current
/// observation alone, the second must close it. Collapsing them into one
/// `None` would skip the close in the second case and leak the observation.
pub(crate) enum Reroot {
    /// The needed author set is unchanged. Touch nothing.
    Unchanged,
    /// Re-rooted onto an author set that asks no question. Close the current
    /// observation and open nothing.
    Closed,
    /// Re-rooted. Close the current observation, then open this query.
    Reopened(LiveQuery),
}

/// One neutral routing replacement for the loop to apply. `None` is absence,
/// which is a different fact from "no routes yet" and is only reached once
/// every exact source has settled.
pub(crate) struct AuthorRouteUpdate {
    pub(crate) author: PublicKey,
    pub(crate) routes: Option<nmp_router::AuthorRoutes>,
}

/// Owner of NIP-65 demand, winner, marker and absence semantics, bound to one
/// observation the LOOP opened.
pub(crate) struct RuntimeAssembly {
    coordinator: nmp_nip65::Nip65Coordinator,
    /// The route-source observation, remembered only after the loop hands it
    /// over. This type cannot open one: it has no reducer to ask.
    handle: Option<ObservationId>,
    revision: u64,
}

impl RuntimeAssembly {
    pub(crate) fn new(sources: impl IntoIterator<Item = RelayUrl>) -> Self {
        Self {
            coordinator: nmp_nip65::Nip65Coordinator::new(sources),
            handle: None,
            revision: 0,
        }
    }

    pub(crate) fn reroot(&mut self, needs: BTreeSet<PublicKey>) -> Reroot {
        if self.coordinator.authors() == &needs {
            return Reroot::Unchanged;
        }
        match self.coordinator.reroot(needs) {
            Some(query) => {
                self.revision = query.revision;
                Reroot::Reopened(LiveQuery::single(query.demand))
            }
            None => Reroot::Closed,
        }
    }

    /// Forget the observation the loop is about to close, and return it so the
    /// loop can close it. Nothing else clears the handle.
    pub(crate) fn unbind(&mut self) -> Option<ObservationId> {
        self.handle.take()
    }

    /// Remember an observation the LOOP minted.
    pub(crate) fn bind(&mut self, handle: ObservationId) {
        self.handle = Some(handle);
    }

    /// Does this delivery belong to the route source rather than to an app
    /// subscription?
    pub(crate) fn owns(&self, handle: ObservationId) -> bool {
        self.handle == Some(handle)
    }

    #[cfg(test)]
    pub(crate) fn bound(&self) -> Option<ObservationId> {
        self.handle
    }

    pub(crate) fn observe_rows(&mut self, rows: &[RowDelta]) -> Vec<AuthorRouteUpdate> {
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

    pub(crate) fn observe_evidence(
        &mut self,
        evidence: &[ObservationEvidence],
    ) -> Vec<AuthorRouteUpdate> {
        let mut coordinator_updates = Vec::new();
        for item in evidence {
            if let ObservationFact::RequestSettled { relay, .. } = &item.fact {
                coordinator_updates.extend(self.coordinator.settle(self.revision, relay));
            }
        }
        updates(coordinator_updates)
    }
}

fn updates(coordinator_updates: Vec<nmp_nip65::CoordinatorUpdate>) -> Vec<AuthorRouteUpdate> {
    coordinator_updates
        .into_iter()
        .map(|update| match update {
            nmp_nip65::CoordinatorUpdate::Present { author, routes } => AuthorRouteUpdate {
                author,
                routes: Some(nmp_router::AuthorRoutes::new(
                    routes.outbound,
                    routes.inbound,
                )),
            },
            nmp_nip65::CoordinatorUpdate::Absent { author } => AuthorRouteUpdate {
                author,
                routes: None,
            },
        })
        .collect()
}
