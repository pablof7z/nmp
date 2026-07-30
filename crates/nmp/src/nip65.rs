//! Optional Rust NIP-65 facade assembly.
//!
//! Protocol values stay engine-free in `nmp-nip65`; this module binds their
//! ordinary query/write values to the core engine and privately converts
//! coordinator output into the one neutral author-route writer.

use std::collections::BTreeSet;

use nmp_resolver::{HandleId, LiveQuery};
use nostr::{PublicKey, RelayUrl};

use crate::core::{
    AuthorRouteReplacement, Effect, EngineCore, EngineMsg, ObservationEvidence, ObservationFact,
    RowDelta,
};
use crate::{Engine, EngineError, ReceiptStream};

pub use nmp_nip65::{
    relay_list_demand, BootstrapRelayList, BootstrapRelayListError, CoordinatorQuery,
    CoordinatorUpdate, ParsedAuthorRoutes, RelayListEntry, RelayUsage, RELAY_LIST_KIND,
};

/// Engine binding for the pure bootstrap value.
pub trait Nip65Operations {
    fn publish_relay_list_bootstrap(
        &self,
        request: BootstrapRelayList,
    ) -> Result<ReceiptStream, EngineError>;
}

impl Nip65Operations for Engine {
    fn publish_relay_list_bootstrap(
        &self,
        request: BootstrapRelayList,
    ) -> Result<ReceiptStream, EngineError> {
        self.publish_tracked(request.into_write_intent())
    }
}

pub(crate) struct RuntimeAssembly {
    coordinator: nmp_nip65::Nip65Coordinator,
    handle: Option<HandleId>,
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

    pub(crate) fn sync<S: nmp_store::EventStore>(
        &mut self,
        core: &mut EngineCore<S>,
        needs: BTreeSet<PublicKey>,
    ) -> Vec<Effect> {
        if self.coordinator.authors() == &needs {
            return Vec::new();
        }
        let query = self.coordinator.reroot(needs);
        let mut effects = Vec::new();
        if let Some(handle) = self.handle.take() {
            effects.extend(core.handle(EngineMsg::Unsubscribe(handle)));
        }
        let Some(query) = query else {
            return effects;
        };
        self.revision = query.revision;
        effects.extend(core.handle(EngineMsg::Subscribe(LiveQuery(query.demand))));
        self.handle = effects.iter().find_map(|effect| match effect {
            Effect::EmitRows(handle, ..) => Some(*handle),
            _ => None,
        });
        effects
    }

    pub(crate) fn consume_rows<S: nmp_store::EventStore>(
        &mut self,
        core: &mut EngineCore<S>,
        handle: HandleId,
        rows: &[RowDelta],
    ) -> Option<Vec<Effect>> {
        if self.handle != Some(handle) {
            return None;
        }
        let removed = rows
            .iter()
            .filter_map(|row| match row {
                RowDelta::Removed(id) => Some(*id),
                RowDelta::Added(_) | RowDelta::SourcesGrew { .. } => None,
            })
            .collect::<Vec<_>>();
        let events = rows
            .iter()
            .filter_map(RowDelta::event)
            .cloned()
            .collect::<Vec<_>>();
        let updates = self
            .coordinator
            .observe_current_delta(removed, events, |relay| core.admits_discovered_route(relay));
        Some(apply_updates(core, updates))
    }

    pub(crate) fn consume_evidence<S: nmp_store::EventStore>(
        &mut self,
        core: &mut EngineCore<S>,
        handle: HandleId,
        evidence: &[ObservationEvidence],
    ) -> Option<Vec<Effect>> {
        if self.handle != Some(handle) {
            return None;
        }
        let mut updates = Vec::new();
        for item in evidence {
            if let ObservationFact::RequestSettled { relay, .. } = &item.fact {
                updates.extend(self.coordinator.settle(self.revision, relay));
            }
        }
        Some(apply_updates(core, updates))
    }
}

fn apply_updates<S: nmp_store::EventStore>(
    core: &mut EngineCore<S>,
    updates: Vec<CoordinatorUpdate>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    for update in updates {
        match update {
            CoordinatorUpdate::Present { author, routes } => {
                core.replace_author_routes(
                    author,
                    AuthorRouteReplacement::Present(nmp_router::AuthorRoutes::new(
                        routes.outbound,
                        routes.inbound,
                    )),
                    &mut effects,
                );
            }
            CoordinatorUpdate::Absent { author } => {
                core.replace_author_routes(author, AuthorRouteReplacement::Absent, &mut effects);
            }
        }
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Row;
    use nmp_store::MemoryStore;
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn relay(port: u16) -> RelayUrl {
        RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("valid test relay")
    }

    #[test]
    fn needs_open_one_exact_query_noop_when_unchanged_and_close_when_empty() {
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        let mut assembly = RuntimeAssembly::new([relay(19_870), relay(19_871)]);
        let needs = BTreeSet::from([author()]);

        let opened = assembly.sync(&mut core, needs.clone());
        assert!(
            opened
                .iter()
                .any(|effect| matches!(effect, Effect::EmitRows(..))),
            "ordinary subscribe must expose the internal query handle"
        );
        assert!(assembly.handle.is_some(), "the internal query stays owned");

        assert!(
            assembly.sync(&mut core, needs).is_empty(),
            "an unchanged need set must not reopen the query"
        );

        let _closed = assembly.sync(&mut core, BTreeSet::new());
        assert!(
            assembly.handle.is_none(),
            "empty needs must release the internal query"
        );
    }

    #[test]
    fn zero_sources_leave_needs_unknown_without_opening_a_query() {
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        let mut assembly = RuntimeAssembly::new([]);

        assert!(
            assembly
                .sync(&mut core, BTreeSet::from([author()]))
                .is_empty(),
            "without operator-selected sources there is no exact query to ask"
        );
        assert!(assembly.handle.is_none());
    }

    #[test]
    fn provider_rejections_increment_diagnostics_once_per_relay_list_tag() {
        let keys = Keys::generate();
        let author = keys.public_key();
        let source = RelayUrl::parse("wss://nip65-source.example").unwrap();
        let rejected = relay(19_872);
        let event = EventBuilder::new(Kind::RelayList, "")
            .tag(
                Tag::parse(["r".to_string(), rejected.to_string()])
                    .expect("valid unmarked relay-list tag"),
            )
            .custom_created_at(Timestamp::from(1))
            .sign_with_keys(&keys)
            .expect("relay-list fixture signs");
        let row = RowDelta::Added(Row {
            event,
            sources: BTreeSet::from([source.clone()]),
        });
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        let mut assembly = RuntimeAssembly::new([source]);

        let _opened = assembly.sync(&mut core, BTreeSet::from([author]));
        let handle = assembly.handle.expect("provider query handle");
        let _effects = assembly
            .consume_rows(&mut core, handle, std::slice::from_ref(&row))
            .expect("current provider rows are consumed");

        assert_eq!(
            core.diagnostics_snapshot()
                .discovered_private_relays_rejected,
            1,
            "one rejected unmarked tag counts once before it projects to both directions"
        );

        let _effects = assembly
            .consume_rows(&mut core, handle, std::slice::from_ref(&row))
            .expect("an unchanged current winner is still recognized");
        assert_eq!(
            core.diagnostics_snapshot()
                .discovered_private_relays_rejected,
            1,
            "re-delivering the unchanged current winner must not recount it"
        );
    }
}
