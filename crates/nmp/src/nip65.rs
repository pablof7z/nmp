//! Optional Rust NIP-65 facade assembly.
//!
//! Protocol values stay engine-free in `nmp-nip65`; this module binds their
//! ordinary query/write values to the core engine and privately converts
//! coordinator output into the one neutral author-route writer.

use std::collections::BTreeSet;

use nmp_grammar::LiveQuery;
use nostr::{PublicKey, RelayUrl};

use crate::core::{
    AuthorRouteReplacement, Effect, EngineCore, EngineMsg, ObservationEvidence, ObservationFact,
    ObservationId, RowDelta,
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
        effects.extend(core.handle(EngineMsg::Subscribe(LiveQuery::single(query.demand))));
        self.handle = effects.iter().find_map(|effect| match effect {
            Effect::EmitRows(handle, ..) => Some(*handle),
            _ => None,
        });
        effects
    }

    pub(crate) fn consume_rows<S: nmp_store::EventStore>(
        &mut self,
        core: &mut EngineCore<S>,
        handle: ObservationId,
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
        handle: ObservationId,
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
    use crate::core::{ReceiptId, Row};
    use crate::publish_queue::{RelayState, RelayWaiting, WriteFact};
    use nmp_grammar::{
        AccessContext, Identity, RelaySessionKey, WriteIntent, WritePayload, WriteRouting,
    };
    use nmp_store::{EventStore, MemoryStore, RedbStore};
    use nmp_transport::{HandoffResult, RelayFrame, RelayHandle};
    use nostr::{EventBuilder, EventId, Keys, Kind, RelayMessage, Tag, Timestamp};

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn relay(port: u16) -> RelayUrl {
        RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("valid test relay")
    }

    fn publish_auto<S: EventStore>(
        core: &mut EngineCore<S>,
        author: &Keys,
        created_at: u64,
        content: &str,
    ) -> (ReceiptId, EventId, Vec<Effect>) {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: content.to_string(),
                created_at: Some(Timestamp::from(created_at)),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
        let (receipt, generation, unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("accepted write requests its frozen signature");
        let signed = unsigned
            .sign_with_keys(author)
            .expect("test author signs the accepted write");
        let event_id = signed.id;
        let effects = core.handle(EngineMsg::SignerCompleted(receipt, generation, Ok(signed)));
        (receipt, event_id, effects)
    }

    /// The park a write sits in while nothing is known about where it goes:
    /// an EMPTY destination set that is still OPEN. Distinct from
    /// `WriteOutcome::NoDestination`, which is knowledge exhausted, and the
    /// distinction is the whole point -- one waits forever, the other is a
    /// terminal.
    fn assert_parked_on_unknown_route<S: EventStore>(core: &mut EngineCore<S>, receipt: ReceiptId) {
        let replay = core.reattach_receipt(receipt);
        assert!(replay.is_attached(), "the durable receipt must reattach");
        assert!(
            replay.facts.iter().any(|fact| matches!(
                fact,
                WriteFact::Destinations {
                    relays,
                    complete: false
                } if relays.is_empty()
            )),
            "the receipt must remain visibly parked on route knowledge: {:?}",
            replay.facts
        );
        assert!(
            !replay
                .facts
                .iter()
                .any(|fact| matches!(fact, WriteFact::Outcome(_))),
            "a park is not a terminal: {:?}",
            replay.facts
        );
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

    /// `ROUTING-COLDSTARTPARK-003`: a cold-recovered unresolved `Auto`
    /// obligation and a freshly accepted one are the same lifecycle state.
    /// A signed kind:10002 learned after both exist must wake both exact event
    /// ids; recovery may not grant the older row a stronger survival path.
    #[test]
    fn fresh_and_recovered_auto_writes_share_one_later_author_route() {
        let author = Keys::generate();
        let source = RelayUrl::parse("wss://indexer.example").unwrap();
        let outbox = RelayUrl::parse("wss://outbox.example").unwrap();
        let dir = tempfile::tempdir().expect("temporary redb directory");
        let path = dir.path().join("cold-start-park.redb");

        let (recovered_receipt, recovered_event) = {
            let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 8);
            core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
            let (receipt, event, effects) = publish_auto(&mut core, &author, 1, "from before");
            assert!(
                effects.iter().any(|effect| matches!(
                    effect,
                    Effect::EmitReceipt(
                        id,
                        WriteFact::Destinations {
                            relays,
                            complete: false
                        }
                    ) if *id == receipt && relays.is_empty()
                )),
                "a first-install Auto write must park instead of dying: {effects:?}"
            );
            (receipt, event)
        };

        let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 8);
        let booted = core.recover_on_boot();
        assert!(
            booted.iter().any(|effect| matches!(
                effect,
                Effect::AuthorRouteNeedsChanged(needs) if needs == &BTreeSet::from([author.public_key()])
            )),
            "boot must name the exact author route the park is waiting on: {booted:?}"
        );
        assert_parked_on_unknown_route(&mut core, recovered_receipt);

        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        let (fresh_receipt, fresh_event, fresh_effects) =
            publish_auto(&mut core, &author, 2, "from now");
        assert!(
            fresh_effects.iter().any(|effect| matches!(
                effect,
                Effect::EmitReceipt(
                    id,
                    WriteFact::Destinations {
                        relays,
                        complete: false
                    }
                ) if *id == fresh_receipt && relays.is_empty()
            )),
            "the fresh write must enter the same visible park: {fresh_effects:?}"
        );
        assert_parked_on_unknown_route(&mut core, fresh_receipt);

        let relay_list = EventBuilder::new(Kind::RelayList, "")
            .tag(
                Tag::parse(["r".to_string(), outbox.to_string(), "write".to_string()])
                    .expect("valid write-marked relay tag"),
            )
            .custom_created_at(Timestamp::from(3u64))
            .sign_with_keys(&author)
            .expect("truthful kind:10002 fixture signs");
        let row = RowDelta::Added(Row {
            event: relay_list,
            sources: BTreeSet::from([source.clone()]),
        });
        let mut assembly = RuntimeAssembly::new([source]);
        let opened = assembly.sync(&mut core, BTreeSet::from([author.public_key()]));
        let handle = assembly
            .handle
            .expect("the exact author-route need opens one provider query");
        assert!(
            opened
                .iter()
                .any(|effect| matches!(effect, Effect::EmitRows(id, ..) if *id == handle)),
            "provider acquisition must run through the ordinary query owner"
        );
        let route_effects = assembly
            .consume_rows(&mut core, handle, std::slice::from_ref(&row))
            .expect("the current provider row belongs to this assembly");
        let routed_receipts = route_effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::EmitReceipt(
                    id,
                    WriteFact::Destinations {
                        relays,
                        complete: true,
                    },
                ) if relays == &BTreeSet::from([outbox.clone()]) => Some(*id),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            routed_receipts,
            BTreeSet::from([recovered_receipt, fresh_receipt]),
            "the learned route must rewrite both open obligations: {route_effects:?}"
        );

        for receipt in [recovered_receipt, fresh_receipt] {
            let replay = core.reattach_receipt(receipt);
            assert!(replay.is_attached());
            assert!(
                replay.facts.iter().any(|fact| matches!(
                    fact,
                    WriteFact::Relay { relay, state: RelayState::Waiting(RelayWaiting::NotConnected) } if relay == &outbox
                )),
                "receipt {receipt:?} must own exactly the newly learned lane: {:?}",
                replay.facts
            );
        }

        let transport = RelayHandle {
            slot: 0,
            generation: 1,
        };
        let session =
            RelaySessionKey::new(outbox.clone(), AccessContext::Nip42(author.public_key()));
        let mut delivery = core.handle(EngineMsg::RelayConnected(transport, session.clone()));
        delivery.extend(core.handle(EngineMsg::RelayInformationResolved(outbox.clone(), None)));
        delivery.extend(core.handle(EngineMsg::AuthProbeReleased(transport, session.clone())));
        let mut published = BTreeSet::new();
        loop {
            let attempts = std::mem::take(&mut delivery)
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::PublishEvent(actual_session, event, correlation)
                        if actual_session == session =>
                    {
                        Some((event.id, correlation))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if attempts.is_empty() {
                break;
            }
            for (event_id, correlation) in attempts {
                assert!(
                    published.insert(event_id),
                    "one obligation may start only one attempt before its OK"
                );
                delivery.extend(
                    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written)),
                );
                delivery.extend(core.handle(EngineMsg::RelayFrame(
                    transport,
                    session.clone(),
                    RelayFrame::from(RelayMessage::ok(event_id, true, "saved")),
                )));
            }
        }
        assert_eq!(
            published,
            BTreeSet::from([recovered_event, fresh_event]),
            "one later author route must release both exact obligations on its one lane"
        );
        for receipt in [recovered_receipt, fresh_receipt] {
            let replay = core.reattach_receipt(receipt);
            assert!(
                replay.facts.contains(&WriteFact::Relay {
                    relay: outbox.clone(),
                    state: RelayState::Published
                }),
                "receipt {receipt:?} must retain delivery to the one learned lane: {:?}",
                replay.facts
            );
        }
    }
}
