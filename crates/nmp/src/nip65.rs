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
        let updates = self.coordinator.observe_current_delta(removed, events);
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

/// Apply admission to one author's parsed relay list, here, where the
/// author's identity is known (#1251).
///
/// This is the whole point of the move out of `parse_relay_list`. The parser
/// holds one event; this function holds the engine, so it can ask the only
/// question that decides the answer — is this key one we can act as? — and it
/// keeps what it refused instead of dropping it on the floor, so an author
/// whose entire list was turned away never reads as an author with no relays.
fn admit_author_routes<S: nmp_store::EventStore>(
    core: &mut EngineCore<S>,
    author: PublicKey,
    routes: ParsedAuthorRoutes,
) -> nmp_router::AuthorRoutes {
    let declarer = core.relay_list_declarer(&author);
    let mut outbound = BTreeSet::new();
    let mut inbound = BTreeSet::new();
    let mut refused = BTreeSet::new();
    // One decision per DECLARED relay, not one per direction: an unmarked row
    // that names both directions is one refusal, counted once.
    let declared = routes
        .outbound
        .union(&routes.inbound)
        .cloned()
        .collect::<Vec<_>>();
    for relay in declared {
        if core.admits_relay(&relay, declarer).is_err() {
            refused.insert(relay);
            continue;
        }
        if routes.outbound.contains(&relay) {
            outbound.insert(relay.clone());
        }
        if routes.inbound.contains(&relay) {
            inbound.insert(relay);
        }
    }
    nmp_router::AuthorRoutes::new(outbound, inbound).with_refused(refused)
}

fn apply_updates<S: nmp_store::EventStore>(
    core: &mut EngineCore<S>,
    updates: Vec<CoordinatorUpdate>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    for update in updates {
        match update {
            CoordinatorUpdate::Present { author, routes } => {
                let admitted = admit_author_routes(core, author, routes);
                core.replace_author_routes(
                    author,
                    AuthorRouteReplacement::Present(admitted),
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
    use crate::delivery::WriteStatus;
    use nmp_grammar::{
        AccessContext, Durability, Identity, RelaySessionKey, WriteIntent, WritePayload,
        WriteRouting,
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
            durability: Durability::Durable,
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

    fn awaiting_route<S: EventStore>(core: &mut EngineCore<S>, receipt: ReceiptId) -> String {
        let replay = core.reattach_receipt(receipt);
        assert!(replay.is_attached(), "the durable receipt must reattach");
        replay
            .facts
            .iter()
            .find_map(|fact| match fact {
                WriteStatus::AwaitingRoute { detail } => Some(detail.clone()),
                _ => None,
            })
            .expect("the receipt must remain visibly parked on route knowledge")
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
                    Effect::EmitReceipt(id, WriteStatus::AwaitingRoute { .. })
                        if *id == receipt
                )),
                "a first-install Auto write must park instead of dying: {effects:?}"
            );
            (receipt, event)
        };

        let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 8);
        core.recover_on_boot();
        let recovered_detail = awaiting_route(&mut core, recovered_receipt);
        assert!(
            recovered_detail.contains(&author.public_key().to_hex()),
            "the recovered park names the exact missing author route: {recovered_detail}"
        );

        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        let (fresh_receipt, fresh_event, fresh_effects) =
            publish_auto(&mut core, &author, 2, "from now");
        assert!(
            fresh_effects.iter().any(|effect| matches!(
                effect,
                Effect::EmitReceipt(id, WriteStatus::AwaitingRoute { .. })
                    if *id == fresh_receipt
            )),
            "the fresh write must enter the same visible park: {fresh_effects:?}"
        );
        let fresh_detail = awaiting_route(&mut core, fresh_receipt);
        assert_eq!(
            recovered_detail, fresh_detail,
            "fresh and recovered obligations in the same unknown directory state must report the same park"
        );

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
                    WriteStatus::Routed {
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
                    WriteStatus::AwaitingRelay { relay } if relay == &outbox
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
                replay.facts.contains(&WriteStatus::Acked(outbox.clone())),
                "receipt {receipt:?} must retain delivery to the one learned lane: {:?}",
                replay.facts
            );
        }
    }
}

/// The governed provenance falsifiers (#1251).
///
/// Every test here drives the real assembly: a signed kind:10002 arrives
/// through the ordinary query, the coordinator picks the winner, and this
/// module decides admission with the engine's identity knowledge. What
/// changes between them is only WHOSE key signed the list.
#[cfg(test)]
mod provenance {
    use super::*;
    use crate::core::{
        Declarer, EngineMsg, OnionReachability, RelayAdmissionPolicy, Row, RowDelta,
    };
    use nmp_router::AuthorRouteState;
    use nmp_store::MemoryStore;
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    const INDEXER: &str = "wss://indexer.example";

    fn list_event(keys: &Keys, rows: &[(&str, Option<&str>)]) -> nostr::Event {
        let tags = rows.iter().map(|(url, marker)| {
            let mut row = vec!["r".to_string(), (*url).to_string()];
            if let Some(marker) = marker {
                row.push((*marker).to_string());
            }
            Tag::parse(row).expect("valid relay-list row")
        });
        EventBuilder::new(Kind::RelayList, "")
            .tags(tags)
            .custom_created_at(Timestamp::from(1u64))
            .sign_with_keys(keys)
            .expect("relay-list fixture signs")
    }

    /// Deliver `author`'s signed relay list through the ordinary assembly and
    /// return the neutral author fact it produced.
    fn learn_routes(
        core: &mut EngineCore<MemoryStore>,
        keys: &Keys,
        rows: &[(&str, Option<&str>)],
    ) -> AuthorRouteState {
        let source = RelayUrl::parse(INDEXER).expect("valid indexer url");
        let mut assembly = RuntimeAssembly::new([source.clone()]);
        let _opened = assembly.sync(core, BTreeSet::from([keys.public_key()]));
        let handle = assembly.handle.expect("the author need opens one query");
        let row = RowDelta::Added(Row {
            event: list_event(keys, rows),
            sources: BTreeSet::from([source]),
        });
        let _effects = assembly
            .consume_rows(core, handle, std::slice::from_ref(&row))
            .expect("the current row belongs to this assembly");
        core.author_routes(&keys.public_key())
    }

    fn present(state: AuthorRouteState) -> nmp_router::AuthorRoutes {
        match state {
            AuthorRouteState::Present(routes) => routes,
            other => panic!("a signed relay list is positive knowledge, got {other:?}"),
        }
    }

    /// "Sending to a recipient whose relay list names localhost -- that entry
    /// is skipped." Their LAN is not our LAN, and the address is meaningless
    /// to us at best.
    #[test]
    fn a_recipients_localhost_relay_row_is_skipped() {
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        let recipient = Keys::generate();
        let routes = present(learn_routes(
            &mut core,
            &recipient,
            &[
                ("ws://127.0.0.1:7777", None),
                ("wss://public.example", None),
            ],
        ));
        assert_eq!(
            routes.inbound(),
            &BTreeSet::from([RelayUrl::parse("wss://public.example").unwrap()]),
            "only the public relay survives someone else's declaration"
        );
        assert_eq!(
            routes.refused(),
            &BTreeSet::from([RelayUrl::parse("ws://127.0.0.1:7777").unwrap()]),
            "the skipped entry is recorded, not dropped on the floor"
        );
    }

    /// "The signed-in user's own relay list names localhost -- we attempt it."
    /// The bytes arrived from an indexer we do not control; what makes the
    /// list ours is the key that signed it.
    #[test]
    fn our_own_localhost_relay_row_is_heeded_however_it_arrived() {
        let me = Keys::generate();
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        core.handle(EngineMsg::SetActivePubkey(Some(me.public_key())));
        let routes = present(learn_routes(
            &mut core,
            &me,
            &[("ws://127.0.0.1:7777", Some("write"))],
        ));
        assert_eq!(
            routes.outbound(),
            &BTreeSet::from([RelayUrl::parse("ws://127.0.0.1:7777").unwrap()]),
            "our own declaration describes our own network"
        );
        assert!(routes.refused().is_empty());
        assert_eq!(
            core.dial_declarer(&RelayUrl::parse("ws://127.0.0.1:7777").unwrap()),
            nmp_network_policy::Declarer::Ourselves,
            "the grant has to survive to the socket, or we heed a relay we \
             then refuse to dial"
        );
    }

    /// Authorship, not arrival: the SAME event, the same indexer, the same
    /// localhost row. Only whether we hold the signing key differs.
    #[test]
    fn the_identical_list_flips_on_who_signed_it_and_nothing_else() {
        let keys = Keys::generate();
        let rows = &[("ws://127.0.0.1:7777", None)][..];

        let mut signed_out = EngineCore::new(MemoryStore::new(), 8);
        let theirs = present(learn_routes(&mut signed_out, &keys, rows));
        assert!(
            theirs.outbound().is_empty() && !theirs.refused().is_empty(),
            "signed out, nothing is ours: {theirs:?}"
        );

        let mut signed_in = EngineCore::new(MemoryStore::new(), 8);
        signed_in.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let ours = present(learn_routes(&mut signed_in, &keys, rows));
        assert!(
            !ours.outbound().is_empty() && ours.refused().is_empty(),
            "signed in as that key, the same bytes are ours: {ours:?}"
        );
    }

    /// "An author whose entire list was refused is not reported as having no
    /// relays." This is the whole defect: both used to be `Present` with two
    /// empty sets, so nothing downstream could tell a user with LAN relays
    /// from a user with none.
    #[test]
    fn an_entirely_refused_list_is_not_the_same_value_as_an_empty_one() {
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        let all_local = present(learn_routes(
            &mut core,
            &Keys::generate(),
            &[("ws://127.0.0.1:7777", None), ("ws://192.168.1.10", None)],
        ));
        let declared_none = present(learn_routes(
            &mut core,
            &Keys::generate(),
            &[("not a relay url", None)],
        ));

        assert!(all_local.outbound().is_empty() && all_local.inbound().is_empty());
        assert!(declared_none.outbound().is_empty() && declared_none.inbound().is_empty());
        assert_ne!(
            all_local, declared_none,
            "the two must not be the same value; that identity IS the bug"
        );
        assert!(all_local.every_declared_relay_was_refused());
        assert!(!declared_none.every_declared_relay_was_refused());
        assert_eq!(all_local.refused().len(), 2);
    }

    /// "Tor enabled -- another person's `.onion` relay is used." Reachability
    /// is a separate axis from provenance, so declaring it changes what a
    /// STRANGER's list may name.
    #[test]
    fn declared_tor_reachability_admits_a_strangers_onion_relay() {
        let stranger = Keys::generate();
        let onion = "ws://nmprelayxyz.onion";
        let rows = &[(onion, None)][..];

        let mut without_tor = EngineCore::new(MemoryStore::new(), 8);
        let refused = present(learn_routes(&mut without_tor, &stranger, rows));
        assert_eq!(
            refused.refused(),
            &BTreeSet::from([RelayUrl::parse(onion).unwrap()])
        );

        let mut with_tor = EngineCore::new(MemoryStore::new(), 8)
            .with_relay_admission(RelayAdmissionPolicy::new([], OnionReachability::Reachable));
        let used = present(learn_routes(&mut with_tor, &stranger, rows));
        assert_eq!(
            used.inbound(),
            &BTreeSet::from([RelayUrl::parse(onion).unwrap()]),
            "a declared Tor capability makes other people's hidden services usable"
        );
        assert!(used.refused().is_empty());
    }

    /// The local-host allowlist is about local hosts. Declaring Tor must not
    /// re-admit loopback, and listing loopback must not re-admit Tor.
    #[test]
    fn the_two_axes_do_not_grant_each_others_addresses() {
        let stranger = Keys::generate();

        let mut tor_only = EngineCore::new(MemoryStore::new(), 8)
            .with_relay_admission(RelayAdmissionPolicy::new([], OnionReachability::Reachable));
        let routes = present(learn_routes(
            &mut tor_only,
            &stranger,
            &[("ws://127.0.0.1:7777", None)],
        ));
        assert_eq!(routes.refused().len(), 1, "Tor grants loopback nothing");

        let mut local_only =
            EngineCore::new(MemoryStore::new(), 8).with_relay_admission(RelayAdmissionPolicy::new(
                ["nmprelayxyz.onion".to_string()],
                OnionReachability::Unreachable,
            ));
        let routes = present(learn_routes(
            &mut local_only,
            &stranger,
            &[("ws://nmprelayxyz.onion", None)],
        ));
        assert_eq!(
            routes.refused().len(),
            1,
            "a hidden service listed as a local host is still not reachable"
        );
    }

    /// "Own" is per-identity. Holding key B's signer must not widen what key
    /// A's routes contain: the grant belongs to the exact list it came from,
    /// so a write signing as A can never reach somewhere only B declared.
    #[test]
    fn holding_one_keys_signer_never_widens_another_keys_routes() {
        let key_a = Keys::generate();
        let key_b = Keys::generate();
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        core.handle(EngineMsg::SignerAttached(key_b.public_key()));

        let b_routes = present(learn_routes(
            &mut core,
            &key_b,
            &[("ws://127.0.0.1:7777", Some("write"))],
        ));
        assert_eq!(b_routes.outbound().len(), 1, "B's own list is heeded");

        let a_routes = present(learn_routes(
            &mut core,
            &key_a,
            &[("ws://127.0.0.1:7777", Some("write"))],
        ));
        assert!(
            a_routes.outbound().is_empty(),
            "A did not sign for us, so A's identical row stays refused: {a_routes:?}"
        );
        assert_eq!(a_routes.refused().len(), 1);
    }

    /// Publishing as one identity while another is active: the write signs as
    /// A, so A's own list is what may name a local relay for it.
    #[test]
    fn an_explicitly_named_identity_is_own_even_when_a_different_key_is_active() {
        let publishing_as = Keys::generate();
        let active = Keys::generate();
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        core.handle(EngineMsg::SetActivePubkey(Some(active.public_key())));
        core.handle(EngineMsg::SignerAttached(publishing_as.public_key()));

        let routes = present(learn_routes(
            &mut core,
            &publishing_as,
            &[("ws://127.0.0.1:7777", Some("write"))],
        ));
        assert_eq!(
            routes.outbound().len(),
            1,
            "the key we can publish AS owns its own list, active or not"
        );
    }

    /// Detaching the signer takes the identity back out: a key we can no
    /// longer act as stops being us.
    #[test]
    fn a_detached_signer_stops_being_one_of_our_identities() {
        let keys = Keys::generate();
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        core.handle(EngineMsg::SignerAttached(keys.public_key()));
        assert_eq!(
            core.relay_list_declarer(&keys.public_key()),
            Declarer::Ourselves
        );
        core.handle(EngineMsg::AuthCapabilityInvalidated(
            keys.public_key(),
            crate::core::AuthCapability::Signer,
            crate::core::AuthCapabilityInstance(1),
        ));
        assert_eq!(
            core.relay_list_declarer(&keys.public_key()),
            Declarer::SomeoneElse
        );
    }
}

/// Operator-tier provenance (#1251): what THIS app declared, all the way to
/// the socket. These live beside the identity falsifiers because they are the
/// other half of one rule — the app's own declaration and its own user's
/// declaration are the same grant, arrived at differently.
#[cfg(test)]
mod operator_provenance {
    use crate::core::{Declarer, EngineCore, EngineMsg};
    use nmp_grammar::{
        Durability, Identity, SourceAuthority, WriteIntent, WritePayload, WriteRouting,
    };

    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind, RelayUrl};
    use std::collections::BTreeSet;

    fn config_with_app_relay(url: &str) -> EngineCore<MemoryStore> {
        let facts = crate::core::RoutingFactStore::new([RelayUrl::parse(url).unwrap()], []);
        EngineCore::new_with_routing_facts(MemoryStore::new(), facts, 8)
    }

    /// "The app relay list names localhost -- we heed it." Not merely in the
    /// route set, which was already true, but at the dial: the operator
    /// naming their own dev relay is the whole declaration, and needing a
    /// second `allowed_local_relay_hosts` entry to reach it was two owners
    /// disagreeing about one answer.
    #[test]
    fn an_app_relay_on_localhost_is_heeded_by_the_socket_with_no_allowlist() {
        let local = RelayUrl::parse("ws://127.0.0.1:7777").unwrap();
        let core = config_with_app_relay("ws://127.0.0.1:7777");
        assert_eq!(
            core.dial_declarer(&local),
            Declarer::Ourselves,
            "the operator's own lane must reach the socket as ours"
        );
        let unrelated = RelayUrl::parse("ws://192.168.1.10").unwrap();
        assert_eq!(
            core.dial_declarer(&unrelated),
            Declarer::SomeoneElse,
            "the grant is exact; it does not spill onto other local hosts"
        );
    }

    /// An exact route the app named for one write is the app describing its
    /// own network, so the socket heeds it.
    #[test]
    fn an_explicit_write_route_is_heeded_by_the_socket() {
        let local = RelayUrl::parse("ws://127.0.0.1:7788").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        assert_eq!(core.dial_declarer(&local), Declarer::SomeoneElse);
        let keys = Keys::generate();
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: Vec::new(),
                content: "to my own relay".to_string(),
                created_at: None,
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![local.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        assert_eq!(core.dial_declarer(&local), Declarer::Ourselves);
    }

    /// A pinned read source is the app naming the exact relays to ask --
    /// `RelayScope::on`, a NIP-29 host, an operator indexer query.
    #[test]
    fn a_pinned_read_source_is_heeded_by_the_socket() {
        let local = RelayUrl::parse("ws://127.0.0.1:7799").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 8);
        assert_eq!(core.dial_declarer(&local), Declarer::SomeoneElse);
        let demand = nmp_grammar::Demand::new(
            nmp_grammar::Filter {
                kinds: Some(BTreeSet::from([1u16])),
                ..nmp_grammar::Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([local.clone()])),
            nmp_grammar::AccessContext::Public,
        )
        .expect("a pinned single-source demand is observable");
        core.handle(EngineMsg::Subscribe(nmp_grammar::LiveQuery::single(demand)));
        assert_eq!(core.dial_declarer(&local), Declarer::Ourselves);
    }

    /// Signed out with nothing attached, no author's list is ours -- only the
    /// operator tier grants anything.
    #[test]
    fn signed_out_leaves_only_the_operator_tier() {
        let core = config_with_app_relay("ws://127.0.0.1:7777");
        assert_eq!(
            core.relay_list_declarer(&Keys::generate().public_key()),
            Declarer::SomeoneElse
        );
        assert_eq!(
            core.dial_declarer(&RelayUrl::parse("ws://127.0.0.1:7777").unwrap()),
            Declarer::Ourselves,
            "the app's own configured relay is still the app's own"
        );
    }
}
