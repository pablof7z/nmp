use super::*;
use nmp_engine::core::{AuthorRouteReplacement, ReceiptId, Row};
use nmp_engine::publish_queue::{RelayState, RelayWaiting, WriteFact};
use nmp_grammar::{
    AccessContext, Binding, Filter, Identity, RelaySessionKey, WriteIntent, WritePayload,
    WriteRouting,
};
use nmp_store::RedbStore;
use nmp_transport::{HandoffResult, RelayFrame, RelayHandle};
use nostr::{EventBuilder, EventId, Keys, Kind, RelayMessage, Tag, Timestamp};

fn author() -> PublicKey {
    Keys::generate().public_key()
}

fn relay(port: u16) -> RelayUrl {
    RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("valid test relay")
}

/// The workspace's own provider, installed exactly the way an application
/// installs one. Nothing in this crate names it outside these tests.
fn outbox_slot(sources: impl IntoIterator<Item = RelayUrl>) -> RouteProviderSlot {
    RouteProviderSlot::new(Box::new(nmp_outbox::Nip65Outbox::new(sources)))
}

fn publish_auto(
    core: &mut EngineCore,
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
///
/// The park must also still NAME who it is waiting on after a reattach:
/// the reason is replayed off the same reducer memory a live resolution
/// writes, so an app that restarts and reattaches learns the same thing
/// it would have learned by holding the stream open.
fn assert_parked_on_unknown_route(core: &mut EngineCore, receipt: ReceiptId, awaited: &PublicKey) {
    let replay = core.reattach_receipt(receipt);
    assert!(replay.is_attached(), "the durable receipt must reattach");
    assert!(
        replay.facts.iter().any(|fact| matches!(
            fact,
            WriteFact::Destinations {
                relays,
                complete: false,
                awaiting_author_routes,
            } if relays.is_empty()
                && awaiting_author_routes == &BTreeSet::from([*awaited])
        )),
        "a reattached park must remain visible AND say who it waits on: {:?}",
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
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    let mut slot = outbox_slot([relay(19_870), relay(19_871)]);
    let needs = BTreeSet::from([author()]);

    let app_observation = core
        .handle(EngineMsg::Subscribe(LiveQuery::from_filter(
            Filter::default(),
        )))
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, ..) => Some(*id),
            _ => None,
        })
        .expect("an ordinary app subscription opens");

    let opened = provider_reroot(&mut core, &mut slot, needs.clone());
    assert!(
        opened
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(..))),
        "ordinary subscribe must expose the internal query handle"
    );
    assert!(slot.bound.is_some(), "the internal query stays owned");
    assert_ne!(
        slot.bound,
        Some(app_observation),
        "the loop's ownership gate must never hand an app subscription's \
         delivery to the provider"
    );

    assert!(
        provider_reroot(&mut core, &mut slot, needs).is_empty(),
        "an unchanged need set must not reopen the query"
    );

    let _closed = provider_reroot(&mut core, &mut slot, BTreeSet::new());
    assert!(
        slot.bound.is_none(),
        "empty needs must release the internal query"
    );
}

#[test]
fn zero_sources_leave_needs_unknown_without_opening_a_query() {
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    let mut slot = outbox_slot([]);

    assert!(
        provider_reroot(&mut core, &mut slot, BTreeSet::from([author()])).is_empty(),
        "without operator-selected sources there is no exact query to ask"
    );
    assert!(slot.bound.is_none());
}

#[test]
fn someone_elses_local_relay_list_row_becomes_a_route_candidate() {
    let author = Keys::generate();
    let local = relay(19_872);
    let source = RelayUrl::parse("wss://indexer.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    let query = LiveQuery::from_filter(Filter {
        authors: Some(Binding::Literal(BTreeSet::from([author
            .public_key()
            .to_hex()]))),
        ..Filter::default()
    });
    core.handle(EngineMsg::Subscribe(query));

    let event = EventBuilder::new(Kind::RelayList, "")
        .tag(
            Tag::parse(["r".to_string(), local.to_string()])
                .expect("valid unmarked local relay tag"),
        )
        .custom_created_at(Timestamp::from(1u64))
        .sign_with_keys(&author)
        .expect("relay-list fixture signs");
    let row = RowDelta::Added(Row::from_relay_event(
        event,
        BTreeSet::from([source.clone()]),
    ));
    let mut slot = outbox_slot([source]);
    provider_reroot(&mut core, &mut slot, BTreeSet::from([author.public_key()]));
    let updates = slot.provider.observe_rows(&[row]);
    let mut effects = apply_author_routes(&mut core, updates);
    effects.extend(core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(2u64))));

    assert!(
        effects.iter().any(|effect| {
            let Effect::Wire(delta) = effect else {
                return false;
            };
            delta.ops.iter().any(|(session, ops)| {
                session.relay == local
                    && ops
                        .iter()
                        .any(|op| matches!(op, nmp_router::WireOp::Req(..)))
            })
        }),
        "someone else's valid local route must reach the ordinary router plan: {effects:?}"
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
                        complete: false,
                        awaiting_author_routes,
                    }
                ) if *id == receipt
                    && relays.is_empty()
                    && awaiting_author_routes == &BTreeSet::from([author.public_key()])
            )),
            "a first-install Auto write must park instead of dying, and the park must say \
             whose relay list it is waiting for: {effects:?}"
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
    assert_parked_on_unknown_route(&mut core, recovered_receipt, &author.public_key());

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
                    complete: false,
                    awaiting_author_routes,
                }
            ) if *id == fresh_receipt
                && relays.is_empty()
                && awaiting_author_routes == &BTreeSet::from([author.public_key()])
        )),
        "the fresh write must enter the same visible park, naming the same author: \
         {fresh_effects:?}"
    );
    assert_parked_on_unknown_route(&mut core, fresh_receipt, &author.public_key());

    let relay_list = EventBuilder::new(Kind::RelayList, "")
        .tag(
            Tag::parse(["r".to_string(), outbox.to_string(), "write".to_string()])
                .expect("valid write-marked relay tag"),
        )
        .custom_created_at(Timestamp::from(3u64))
        .sign_with_keys(&author)
        .expect("truthful kind:10002 fixture signs");
    let row = RowDelta::Added(Row::from_relay_event(
        relay_list,
        BTreeSet::from([source.clone()]),
    ));
    let mut slot = outbox_slot([source]);
    let opened = provider_reroot(&mut core, &mut slot, BTreeSet::from([author.public_key()]));
    let handle = slot
        .bound
        .expect("the exact author-route need opens one provider query");
    assert!(
        opened
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(id, ..) if *id == handle)),
        "provider acquisition must run through the ordinary query owner"
    );
    let updates = slot.provider.observe_rows(std::slice::from_ref(&row));
    let route_effects = apply_author_routes(&mut core, updates);
    let routed_receipts = route_effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitReceipt(
                id,
                WriteFact::Destinations {
                    relays,
                    complete: true,
                    awaiting_author_routes,
                },
            ) if relays == &BTreeSet::from([outbox.clone()])
                && awaiting_author_routes.is_empty() =>
            {
                Some(*id)
            }
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
                WriteFact::Relay { relay, state: RelayState::Waiting(RelayWaiting::NotConnected), .. } if relay == &outbox
            )),
            "receipt {receipt:?} must own exactly the newly learned lane: {:?}",
            replay.facts
        );
    }

    let transport = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let session = RelaySessionKey::new(outbox.clone(), AccessContext::Nip42(author.public_key()));
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
            delivery
                .extend(core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written)));
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
    for (receipt, event_id) in [
        (recovered_receipt, recovered_event),
        (fresh_receipt, fresh_event),
    ] {
        let replay = core.reattach_receipt(receipt);
        assert!(
            replay.facts.contains(&WriteFact::Relay {
                event_id,
                relay: outbox.clone(),
                state: RelayState::Published
            }),
            "receipt {receipt:?} must retain delivery to the one learned lane: {:?}",
            replay.facts
        );
    }
}

/// A provider that answers entirely from a fixed table, opening no query at
/// all. The seam has to admit this shape or "supply your own algorithm"
/// means "supply your own way of asking relays" — the acceptance case for a
/// third-party algorithm that already knows the answer.
struct FixedTable {
    routes: BTreeMap<PublicKey, RelayUrl>,
}

impl AuthorRouteProvider for FixedTable {
    fn reroot(
        &mut self,
        needs: BTreeSet<PublicKey>,
    ) -> (ProviderReroot, Vec<nmp_engine::core::AuthorRouteUpdate>) {
        let updates = needs
            .into_iter()
            .filter_map(|author| {
                let relay = self.routes.get(&author)?;
                Some(nmp_engine::core::AuthorRouteUpdate {
                    author,
                    replacement: AuthorRouteReplacement::Present(nmp_router::AuthorRoutes::new(
                        [relay.clone()],
                        [],
                    )),
                })
            })
            .collect();
        (ProviderReroot::Closed, updates)
    }

    fn observe_rows(&mut self, _rows: &[RowDelta]) -> Vec<nmp_engine::core::AuthorRouteUpdate> {
        unreachable!("a provider that opens no query is never delivered rows")
    }

    fn observe_evidence(
        &mut self,
        _evidence: &[ObservationEvidence],
    ) -> Vec<nmp_engine::core::AuthorRouteUpdate> {
        unreachable!("a provider that opens no query is never delivered evidence")
    }
}

#[test]
fn a_provider_that_asks_no_question_still_routes_an_auto_write() {
    let author = Keys::generate();
    let table_relay = relay(19_873);
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    let (receipt, _event, _effects) = publish_auto(&mut core, &author, 1, "table-routed");

    let mut slot = RouteProviderSlot::new(Box::new(FixedTable {
        routes: BTreeMap::from([(author.public_key(), table_relay.clone())]),
    }));
    let effects = provider_reroot(&mut core, &mut slot, BTreeSet::from([author.public_key()]));

    assert!(
        slot.bound.is_none(),
        "an immediate answer opens no observation"
    );
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                id,
                WriteFact::Destinations {
                    relays,
                    complete: true,
                    awaiting_author_routes,
                }
            ) if *id == receipt
                && relays == &BTreeSet::from([table_relay.clone()])
                && awaiting_author_routes.is_empty()
        )),
        "the fixed table must resolve the parked write in the same turn: {effects:?}"
    );
}
