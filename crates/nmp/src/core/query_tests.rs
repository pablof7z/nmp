//! Ownership-domain tests moved with the implementation they falsify.

use super::*;

#[cfg(test)]
mod affected_handle_invalidation_tests {
    use nmp_grammar::{Binding, Filter, IndexedTagName};
    use nmp_store::MemoryStore;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    use super::*;

    const HANDLE_COUNT: usize = 64;
    const ROWS_PER_HANDLE: usize = 4;

    fn room_event(keys: &Keys, room: usize, ordinal: usize, created_at: u64) -> SignedEvent {
        EventBuilder::new(Kind::from(9u16), format!("room-{room}-event-{ordinal}"))
            .tag(Tag::parse(["h".to_owned(), format!("room-{room}")]).unwrap())
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn room_query_for_kind(room: usize, kind: u16, limit: usize) -> LiveQuery {
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([kind])),
            tags: BTreeMap::from([(
                IndexedTagName::new('h').unwrap(),
                Binding::Literal(BTreeSet::from([format!("room-{room}")])),
            )]),
            limit: Some(limit),
            ..Filter::default()
        })
    }

    fn room_query(room: usize) -> LiveQuery {
        room_query_for_kind(room, 9, 200)
    }

    fn unlimited_room_query(room: usize) -> LiveQuery {
        LiveQuery::from_filter(Filter {
            tags: BTreeMap::from([(
                IndexedTagName::new('h').unwrap(),
                Binding::Literal(BTreeSet::from([format!("room-{room}")])),
            )]),
            ..Filter::default()
        })
    }

    fn exact_signed_intent(event: SignedEvent, relay: &RelayUrl) -> WriteIntent {
        WriteIntent {
            payload: WritePayload::Signed(event),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        }
    }

    fn subscribed_handle(effects: &[Effect]) -> ObservationId {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, _, _) => Some(*id),
                _ => None,
            })
            .expect("subscribe emits the initial row/evidence snapshot")
    }

    fn row_batches(effects: &[Effect]) -> Vec<&[RowDelta]> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::EmitRows(_, deltas, _) if !deltas.is_empty() => Some(deltas.as_slice()),
                _ => None,
            })
            .collect()
    }

    fn assert_remembered_rows_match_oracle(core: &EngineCore<MemoryStore>, id: ObservationId) {
        let branch = core.observations[&id].branches[0];
        let oracle = core.rows_for(branch).unwrap();
        let oracle: BTreeMap<_, _> = oracle
            .into_iter()
            .map(|(event_id, row)| {
                (
                    event_id,
                    RememberedRow {
                        created_at: row.event.created_at.as_secs(),
                        sources: row.sources,
                    },
                )
            })
            .collect();
        assert_eq!(core.observations[&id].last_rows, oracle);
    }

    #[test]
    fn local_signed_acceptance_updates_unlimited_handle_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-delta.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);

        let initial = room_event(&keys, 7, 0, 10);
        core.resolver
            .store_mut()
            .insert(
                initial,
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        let subscribe = core.handle(EngineMsg::Subscribe(unlimited_room_query(7)));
        let handle = subscribed_handle(&subscribe);
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);

        let arriving = room_event(&keys, 7, 1, 12);
        let effects = core.on_publish(exact_signed_intent(arriving.clone(), &relay));

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(effects.iter().any(|effect| match effect {
            Effect::EmitRows(id, deltas, _) if *id == handle => {
                matches!(deltas.as_slice(), [RowDelta::Added(row)]
                    if row.event.id == arriving.id)
            }
            _ => false,
        }));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn demand_changing_local_acceptance_keeps_the_full_refresh_oracle() {
        let author = Keys::generate();
        let followed = Keys::generate();
        let relay = RelayUrl::parse("wss://local-demand-change.example").unwrap();
        let followed_post = nmp_resolver::testkit::kind1(&followed, "already cached", 10);
        let mut store = MemoryStore::new();
        store
            .insert(
                followed_post.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 20);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));

        let follows_query = LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
                inner: nmp_grammar::Demand::from_filter(Filter {
                    kinds: Some(BTreeSet::from([3u16])),
                    authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                    ..Filter::default()
                }),
                project: nmp_grammar::Selector::Tag("p".to_owned()),
            }))),
            ..Filter::default()
        });
        let subscribe = core.handle(EngineMsg::Subscribe(follows_query));
        let handle = subscribed_handle(&subscribe);
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);

        let contact_list = nmp_resolver::testkit::kind3(&author, &[followed.public_key()], 20);
        let effects = core.on_publish(exact_signed_intent(contact_list, &relay));

        assert_eq!(core.router_compiles.get(), 1);
        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(effects.iter().any(|effect| match effect {
            Effect::EmitRows(id, deltas, _) if *id == handle => deltas
                .iter()
                .any(|delta| matches!(delta, RowDelta::Added(row)
                    if row.event.id == followed_post.id)),
            _ => false,
        }));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn local_compensation_removes_pending_row_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-compensation.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        core.active_pubkey = Some(keys.public_key());
        let subscribe = core.handle(EngineMsg::Subscribe(unlimited_room_query(9)));
        let handle = subscribed_handle(&subscribe);

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let accepted = core.on_publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::from(9u16),
                tags: vec![Tag::parse(["h".to_owned(), "room-9".to_owned()]).unwrap()],
                content: "pending local row".into(),
                created_at: Some(Timestamp::from(21u64)),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![relay]),
            identity: Identity::Active,
            correlation: None,
        });
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("local acceptance emits its receipt");
        let pending_id = row_batches(&accepted)[0]
            .iter()
            .find_map(|delta| match delta {
                RowDelta::Added(row) => Some(row.event.id),
                _ => None,
            })
            .expect("pending row was projected");
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let cancelled = core.cancel_write(receipt).1;

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(cancelled.iter().any(|effect| match effect {
            Effect::EmitRows(id, deltas, _) if *id == handle => {
                matches!(deltas.as_slice(), [RowDelta::Removed(event_id)]
                    if *event_id == pending_id)
            }
            _ => false,
        }));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn local_top_n_compensation_uses_one_bounded_backfill_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-top-n.example").unwrap();
        let oldest = room_event(&keys, 10, 0, 10);
        let retained = room_event(&keys, 10, 1, 20);
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                [oldest.clone(), retained]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(21u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 21);
        core.active_pubkey = Some(keys.public_key());
        let subscribe = core.handle(EngineMsg::Subscribe(room_query_for_kind(10, 9, 2)));
        let handle = subscribed_handle(&subscribe);

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let accepted = core.on_publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::from(9u16),
                tags: (vec![Tag::parse(["h".to_owned(), "room-10".to_owned()]).unwrap()])
                    .into_iter()
                    .collect(),
                content: ("newest pending").into(),
                created_at: Some(Timestamp::from(30u64)),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
            correlation: None,
        });
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("local acceptance emits its receipt");
        let accepted_batches = row_batches(&accepted);
        let pending_id = accepted_batches[0]
            .iter()
            .find_map(|delta| match delta {
                RowDelta::Added(row) => Some(row.event.id),
                _ => None,
            })
            .expect("new pending row is visible");
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(accepted_batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == oldest.id)));
        assert_remembered_rows_match_oracle(&core, handle);

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let cancelled = core.cancel_write(receipt).1;

        assert_eq!(core.projection_store_queries.get(), 1);
        assert_eq!(core.router_compiles.get(), 0);
        let batches = row_batches(&cancelled);
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == pending_id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == oldest.id)));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn local_replaceable_compensation_restores_predecessor_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-replaceable.example").unwrap();
        let predecessor = EventBuilder::new(Kind::ContactList, "old")
            .tag(Tag::public_key(Keys::generate().public_key()))
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = MemoryStore::new();
        store
            .insert(
                predecessor.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 20);
        core.active_pubkey = Some(keys.public_key());
        let subscribe = core.handle(EngineMsg::Subscribe(LiveQuery::from_filter(
            Filter::default(),
        )));
        let handle = subscribed_handle(&subscribe);

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let accepted = core.on_publish(WriteIntent {
            payload: WritePayload::ReplaceableEdit {
                builder: nmp_grammar::EventBuilder {
                    kind: Kind::ContactList,
                    tags: (vec![Tag::public_key(Keys::generate().public_key())])
                        .into_iter()
                        .collect(),
                    content: ("new").into(),
                    created_at: Some(Timestamp::from(20u64)),
                },
                expected_base: Some(predecessor.id),
            },
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![relay]),
            identity: Identity::Active,
            correlation: None,
        });
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("replaceable acceptance emits its receipt");
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        let accepted_batches = row_batches(&accepted);
        assert_eq!(accepted_batches.len(), 1);
        let pending_id = accepted_batches[0]
            .iter()
            .find_map(|delta| match delta {
                RowDelta::Added(row) => Some(row.event.id),
                _ => None,
            })
            .expect("new pending winner was added");
        assert!(accepted_batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == predecessor.id)));
        assert_remembered_rows_match_oracle(&core, handle);

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let cancelled = core.cancel_write(receipt).1;

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        let cancelled_batches = row_batches(&cancelled);
        assert_eq!(cancelled_batches.len(), 1);
        assert!(cancelled_batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == predecessor.id)));
        assert!(cancelled_batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == pending_id)));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn local_kind5_compensation_reveals_target_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-kind5.example").unwrap();
        let target = room_event(&keys, 13, 0, 10);
        let mut store = MemoryStore::new();
        store
            .insert(
                target.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 20);
        core.active_pubkey = Some(keys.public_key());
        let subscribe = core.handle(EngineMsg::Subscribe(unlimited_room_query(13)));
        let handle = subscribed_handle(&subscribe);

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let accepted = core.on_publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::EventDeletion,
                tags: (vec![Tag::event(target.id)]).into_iter().collect(),
                content: ("").into(),
                created_at: Some(Timestamp::from(20u64)),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(vec![relay]),
            identity: Identity::Active,
            correlation: None,
        });
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("kind5 acceptance emits its receipt");
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(matches!(
            row_batches(&accepted).as_slice(),
            [batch]
                if matches!(*batch, [RowDelta::Removed(id)] if *id == target.id)
        ));
        assert_remembered_rows_match_oracle(&core, handle);

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let cancelled = core.cancel_write(receipt).1;

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(matches!(
            row_batches(&cancelled).as_slice(),
            [batch]
                if matches!(*batch, [RowDelta::Added(row)] if row.event.id == target.id)
        ));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn nip40_expiry_removes_unlimited_row_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-expiry.example").unwrap();
        let expiring = EventBuilder::new(Kind::from(9u16), "expires")
            .tag(Tag::parse(["h".to_owned(), "room-11".to_owned()]).unwrap())
            .tag(Tag::expiration(Timestamp::from(100u64)))
            .custom_created_at(Timestamp::from(50u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = MemoryStore::new();
        store
            .insert(
                expiring.clone(),
                RelayObserved::new(relay, Timestamp::from(51u64)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, 51);
        let subscribe = core.handle(EngineMsg::Subscribe(unlimited_room_query(11)));
        let handle = subscribed_handle(&subscribe);
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);

        let effects = core.handle(EngineMsg::Tick(Timestamp::from(100u64)));

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(effects.iter().any(|effect| match effect {
            Effect::EmitRows(id, deltas, _) if *id == handle => {
                matches!(deltas.as_slice(), [RowDelta::Removed(event_id)]
                    if *event_id == expiring.id)
            }
            _ => false,
        }));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    fn apply_local_differential_accept(
        core: &mut EngineCore<MemoryStore>,
        event: SignedEvent,
        accepted_at: u64,
        direct: bool,
    ) -> (IntentId, SignedEvent) {
        let accepted = core
            .resolver
            .accept_local(nmp_resolver::testkit::accept_write_of(event, accepted_at))
            .unwrap();
        let (intent_id, pending) = match &accepted.outcome {
            AcceptOutcome::Inserted { intent_id, row, .. }
            | AcceptOutcome::Superseded { intent_id, row, .. }
            | AcceptOutcome::Kind5Processed { intent_id, row, .. } => {
                (*intent_id, row.event.clone())
            }
            other => panic!("differential mutation must commit a pending row, got {other:?}"),
        };
        let mut effects = Vec::new();
        if direct {
            core.apply_committed_mutation(accepted.committed, &mut effects);
        } else {
            core.recompile(&mut effects);
            core.refresh_all_observations(&mut effects);
        }
        (intent_id, pending)
    }

    fn apply_local_differential_compensation(
        core: &mut EngineCore<MemoryStore>,
        intent_id: IntentId,
        pending: SignedEvent,
        direct: bool,
    ) {
        let outcome = core
            .resolver
            .store_mut()
            .compensate_write(intent_id)
            .unwrap();
        let committed = core
            .resolver
            .react_to_compensation(pending, &outcome)
            .unwrap();
        let mut effects = Vec::new();
        if direct {
            core.apply_committed_mutation(committed, &mut effects);
        } else {
            core.recompile(&mut effects);
            core.refresh_all_observations(&mut effects);
        }
    }

    fn apply_local_differential_expiry(
        core: &mut EngineCore<MemoryStore>,
        now: Timestamp,
        direct: bool,
    ) {
        let expired = core.resolver.store_mut().expire_due(now).unwrap();
        let removed = expired.into_iter().map(|row| row.event).collect();
        let committed = core.resolver.retract(removed).unwrap();
        let mut effects = Vec::new();
        if direct {
            core.apply_committed_mutation(committed, &mut effects);
        } else {
            core.recompile(&mut effects);
            core.refresh_all_observations(&mut effects);
        }
    }

    #[test]
    fn mixed_local_accept_compensate_and_expiry_match_forced_full_refresh() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-differential.example").unwrap();
        let predecessor = EventBuilder::new(Kind::ContactList, "old")
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        let target = room_event(&keys, 31, 0, 11);
        let expiring = EventBuilder::new(Kind::TextNote, "expires")
            .tag(Tag::expiration(Timestamp::from(100u64)))
            .custom_created_at(Timestamp::from(12u64))
            .sign_with_keys(&keys)
            .unwrap();
        let seed = [predecessor.clone(), target.clone(), expiring.clone()];

        let make_core = || {
            let mut store = MemoryStore::new();
            store
                .insert_batch(
                    seed.iter()
                        .cloned()
                        .map(|event| {
                            (
                                event,
                                RelayObserved::new(relay.clone(), Timestamp::from(13u64)),
                            )
                        })
                        .collect(),
                )
                .unwrap();
            let mut core = EngineCore::new(store, 13);
            let subscribed = core.handle(EngineMsg::Subscribe(LiveQuery::from_filter(
                Filter::default(),
            )));
            let handle = subscribed_handle(&subscribed);
            (core, handle)
        };
        let (mut direct, direct_handle) = make_core();
        let (mut oracle, oracle_handle) = make_core();

        let assert_same = |direct: &EngineCore<MemoryStore>, oracle: &EngineCore<MemoryStore>| {
            assert_remembered_rows_match_oracle(direct, direct_handle);
            assert_remembered_rows_match_oracle(oracle, oracle_handle);
            assert_eq!(
                direct.observations[&direct_handle].last_rows,
                oracle.observations[&oracle_handle].last_rows
            );
        };
        assert_same(&direct, &oracle);

        let winner = EventBuilder::new(Kind::ContactList, "new")
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let (direct_replaceable_id, direct_replaceable) =
            apply_local_differential_accept(&mut direct, winner.clone(), 21, true);
        let (oracle_replaceable_id, oracle_replaceable) =
            apply_local_differential_accept(&mut oracle, winner, 21, false);
        assert_same(&direct, &oracle);

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target.id))
            .custom_created_at(Timestamp::from(30u64))
            .sign_with_keys(&keys)
            .unwrap();
        let (direct_deletion_id, direct_deletion) =
            apply_local_differential_accept(&mut direct, deletion.clone(), 31, true);
        let (oracle_deletion_id, oracle_deletion) =
            apply_local_differential_accept(&mut oracle, deletion, 31, false);
        assert_same(&direct, &oracle);

        let ordinary = room_event(&keys, 31, 1, 40);
        apply_local_differential_accept(&mut direct, ordinary.clone(), 41, true);
        apply_local_differential_accept(&mut oracle, ordinary, 41, false);
        assert_same(&direct, &oracle);

        apply_local_differential_compensation(
            &mut direct,
            direct_deletion_id,
            direct_deletion,
            true,
        );
        apply_local_differential_compensation(
            &mut oracle,
            oracle_deletion_id,
            oracle_deletion,
            false,
        );
        assert_same(&direct, &oracle);

        apply_local_differential_compensation(
            &mut direct,
            direct_replaceable_id,
            direct_replaceable,
            true,
        );
        apply_local_differential_compensation(
            &mut oracle,
            oracle_replaceable_id,
            oracle_replaceable,
            false,
        );
        assert_same(&direct, &oracle);

        apply_local_differential_expiry(&mut direct, Timestamp::from(100u64), true);
        apply_local_differential_expiry(&mut oracle, Timestamp::from(100u64), false);
        assert_same(&direct, &oracle);
        assert!(!direct.observations[&direct_handle]
            .last_rows
            .contains_key(&expiring.id));
    }

    #[test]
    fn ordinary_room_batch_queries_only_the_matching_handle_and_skips_router_compile() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://affected-room.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);

        let mut seed = Vec::new();
        for room in 0..HANDLE_COUNT {
            for ordinal in 0..ROWS_PER_HANDLE {
                let event = room_event(
                    &keys,
                    room,
                    ordinal,
                    (room * ROWS_PER_HANDLE + ordinal + 1) as u64,
                );
                seed.push((
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(1u64)),
                ));
            }
        }
        core.resolver.store_mut().insert_batch(seed).unwrap();

        for room in 0..HANDLE_COUNT {
            core.handle(EngineMsg::Subscribe(room_query(room)));
        }

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);

        let arriving = room_event(&keys, 17, 99, 50_000);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(50_001u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        let batches = row_batches(&effects);
        assert_eq!(batches.len(), 1);
        assert!(matches!(
            batches[0],
            [RowDelta::Added(row)] if row.event.id == arriving.id
        ));

        // A byte-for-byte duplicate observation is a true no-op: no handle
        // query and no router compile merely to rediscover that fact.
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let mut duplicate_effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(50_001u64)),
            )],
            &mut duplicate_effects,
        );
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(duplicate_effects
            .iter()
            .all(|effect| !matches!(effect, Effect::EmitRows(..))));

        // The same id from a genuinely new relay changes only provenance.
        // The committed provenance fact is already exact: emit SourcesGrew
        // without re-querying prior room history, unrelated handles, or the
        // router.
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let second_relay = RelayUrl::parse("wss://second-room-source.example").unwrap();
        let mut provenance_effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(second_relay.clone(), Timestamp::from(50_002u64)),
            )],
            &mut provenance_effects,
        );
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        let batches = row_batches(&provenance_effects);
        assert_eq!(batches.len(), 1);
        assert!(matches!(
            batches[0],
            [RowDelta::SourcesGrew { id, sources }]
                if *id == arriving.id
                    && *sources == BTreeSet::from([relay.clone(), second_relay.clone()])
        ));
    }

    #[test]
    fn top_n_insert_queries_only_its_handle_and_emits_eviction_delta() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://top-n-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let oldest = room_event(&keys, 7, 0, 10);
        let retained = room_event(&keys, 7, 1, 20);
        let unrelated = room_event(&keys, 8, 0, 10);
        core.resolver
            .store_mut()
            .insert_batch(
                [oldest.clone(), retained, unrelated]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(30u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();

        core.handle(EngineMsg::Subscribe(room_query_for_kind(7, 9, 2)));
        core.handle(EngineMsg::Subscribe(room_query_for_kind(8, 9, 2)));
        core.projection_store_queries.set(0);

        let newest = room_event(&keys, 7, 2, 40);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                newest.clone(),
                RelayObserved::new(relay, Timestamp::from(41u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        let batches = row_batches(&effects);
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == newest.id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == oldest.id)));
    }

    #[test]
    fn top_n_visible_removal_uses_one_bounded_backfill_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://top-n-backfill.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let oldest = room_event(&keys, 21, 0, 10);
        let middle = room_event(&keys, 21, 1, 20);
        let newest = room_event(&keys, 21, 2, 30);
        core.resolver
            .store_mut()
            .insert_batch(
                [oldest.clone(), middle, newest.clone()]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(31u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();

        core.handle(EngineMsg::Subscribe(room_query_for_kind(21, 9, 2)));
        core.projection_store_queries.set(0);

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(newest.id))
            .custom_created_at(Timestamp::from(40u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(deletion, RelayObserved::new(relay, Timestamp::from(41u64)))],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 1);
        let batches = row_batches(&effects);
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == oldest.id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == newest.id)));
    }

    #[test]
    fn top_n_equal_timestamp_id_tie_is_applied_without_store_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://top-n-tie.example").unwrap();
        let tied = |content: &str| {
            EventBuilder::new(Kind::from(9u16), content)
                .tag(Tag::parse(["h".to_owned(), "room-22".to_owned()]).unwrap())
                .custom_created_at(Timestamp::from(50u64))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let mut pair = [tied("a"), tied("b")];
        pair.sort_by_key(|event| event.id);
        let arriving = pair[0].clone();
        let seeded = pair[1].clone();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        core.resolver
            .store_mut()
            .insert(
                seeded.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(51u64)),
            )
            .unwrap();

        core.handle(EngineMsg::Subscribe(room_query_for_kind(22, 9, 1)));
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(relay, Timestamp::from(52u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        let batches = row_batches(&effects);
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == arriving.id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == seeded.id)));
    }

    #[test]
    fn same_batch_insert_and_delete_is_a_zero_query_zero_delta_net_noop() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://same-batch-delete.example").unwrap();
        let target = room_event(&keys, 23, 0, 10);
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target.id))
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        core.handle(EngineMsg::Subscribe(room_query(23)));
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![
                (
                    target,
                    RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
                ),
                (deletion, RelayObserved::new(relay, Timestamp::from(21u64))),
            ],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::EmitRows(..))));
    }

    #[test]
    fn same_batch_multi_relay_insert_emits_complete_initial_sources_without_read() {
        let keys = Keys::generate();
        let first = RelayUrl::parse("wss://batch-source-a.example").unwrap();
        let second = RelayUrl::parse("wss://batch-source-b.example").unwrap();
        let event = room_event(&keys, 24, 0, 10);
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        core.handle(EngineMsg::Subscribe(room_query(24)));
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![
                (
                    event.clone(),
                    RelayObserved::new(first.clone(), Timestamp::from(11u64)),
                ),
                (
                    event.clone(),
                    RelayObserved::new(second.clone(), Timestamp::from(12u64)),
                ),
            ],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(matches!(
            row_batches(&effects).as_slice(),
            [batch] if matches!(
                *batch,
                [RowDelta::Added(row)]
                    if row.event.id == event.id
                        && row.sources == BTreeSet::from([first, second])
            )
        ));
    }

    #[test]
    fn replaceable_supersession_invalidates_old_and_new_matches_only() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://replaceable-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let replaceable = |room: usize, created_at: u64| {
            EventBuilder::new(Kind::from(10_000u16), format!("winner-{room}"))
                .tag(Tag::parse(["h".to_owned(), format!("room-{room}")]).unwrap())
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let old = replaceable(3, 10);
        core.resolver
            .store_mut()
            .insert_batch(vec![(
                old.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )])
            .unwrap();

        for room in [3, 4, 5] {
            core.handle(EngineMsg::Subscribe(room_query_for_kind(room, 10_000, 10)));
        }
        core.projection_store_queries.set(0);

        let new = replaceable(4, 20);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                new.clone(),
                RelayObserved::new(relay, Timestamp::from(21u64)),
            )],
            &mut effects,
        );

        // Both windows were known incomplete (one row under limit 10), so
        // neither removal nor insertion can expose an unknown backfill.
        assert_eq!(core.projection_store_queries.get(), 0);
        let batches = row_batches(&effects);
        assert_eq!(batches.len(), 2);
        assert!(batches
            .iter()
            .any(|batch| matches!(*batch, [RowDelta::Removed(id)] if *id == old.id)));
        assert!(batches
            .iter()
            .any(|batch| matches!(*batch, [RowDelta::Added(row)] if row.event.id == new.id)));
    }

    #[test]
    fn kind_five_removed_row_invalidates_matching_handle_without_shape_luck() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://deletion-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let target = room_event(&keys, 12, 0, 10);
        core.resolver
            .store_mut()
            .insert_batch(vec![(
                target.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )])
            .unwrap();

        core.handle(EngineMsg::Subscribe(room_query(12)));
        core.handle(EngineMsg::Subscribe(room_query(13)));
        core.projection_store_queries.set(0);

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target.id))
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(deletion, RelayObserved::new(relay, Timestamp::from(21u64)))],
            &mut effects,
        );

        // The prior window held one row under limit 200, proving no hidden
        // backfill candidate existed; the committed removal is exact.
        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(matches!(
            row_batches(&effects).as_slice(),
            [batch] if matches!(*batch, [RowDelta::Removed(id)] if *id == target.id)
        ));
    }

    #[test]
    fn strict_pinned_projection_keeps_provenance_filtering_on_the_refresh_oracle() {
        let keys = Keys::generate();
        let pinned = RelayUrl::parse("wss://strict-pinned.example").unwrap();
        let other = RelayUrl::parse("wss://strict-other.example").unwrap();
        let mut demand = room_query(25).branches()[0].clone();
        demand.source = SourceAuthority::Pinned(BTreeSet::from([pinned.clone()]));
        demand.cache = CacheMode::Strict;

        let mut core = EngineCore::new(MemoryStore::new(), 20);
        core.handle(EngineMsg::Subscribe(LiveQuery::single(demand)));

        let event = room_event(&keys, 25, 0, 10);
        core.projection_store_queries.set(0);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                event.clone(),
                RelayObserved::new(other.clone(), Timestamp::from(11u64)),
            )],
            &mut effects,
        );
        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(row_batches(&effects).is_empty());

        core.projection_store_queries.set(0);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                event.clone(),
                RelayObserved::new(pinned.clone(), Timestamp::from(12u64)),
            )],
            &mut effects,
        );
        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(matches!(
            row_batches(&effects).as_slice(),
            [batch] if matches!(
                *batch,
                [RowDelta::Added(row)]
                    if row.event.id == event.id
                        && row.sources == BTreeSet::from([other, pinned])
            )
        ));
    }

    #[test]
    fn one_resolved_root_with_a_derived_subtree_uses_the_refresh_oracle() {
        let me = Keys::generate();
        let followed = Keys::generate();
        let relay = RelayUrl::parse("wss://derived-fallback.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let contact_list = EventBuilder::new(Kind::ContactList, "")
            .tag(Tag::public_key(followed.public_key()))
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&me)
            .unwrap();
        core.resolver
            .store_mut()
            .insert(
                contact_list,
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        core.handle(EngineMsg::SetActivePubkey(Some(me.public_key())));

        let query = LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([9u16])),
            authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
                inner: nmp_grammar::Demand::from_filter(Filter {
                    kinds: Some(BTreeSet::from([3u16])),
                    authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                    ..Filter::default()
                }),
                project: nmp_grammar::Selector::Tag("p".to_owned()),
            }))),
            ..Filter::default()
        });
        core.handle(EngineMsg::Subscribe(query));

        let post = EventBuilder::new(Kind::from(9u16), "followed post")
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&followed)
            .unwrap();
        core.projection_store_queries.set(0);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                post.clone(),
                RelayObserved::new(relay, Timestamp::from(21u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(matches!(
            row_batches(&effects).as_slice(),
            [batch] if matches!(*batch, [RowDelta::Added(row)] if row.event.id == post.id)
        ));
    }

    #[test]
    fn incomplete_projection_forces_one_recovery_read_before_direct_deltas_resume() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-recovery.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let subscribed = core.handle(EngineMsg::Subscribe(unlimited_room_query(28)));
        let handle = subscribed_handle(&subscribed);
        core.observations
            .get_mut(&handle)
            .unwrap()
            .projection_complete = false;

        let first = room_event(&keys, 28, 0, 10);
        core.projection_store_queries.set(0);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                first.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )],
            &mut effects,
        );
        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(core.observations[&handle].projection_complete);

        let second = room_event(&keys, 28, 1, 20);
        core.projection_store_queries.set(0);
        effects.clear();
        core.ingest_relay_events(
            vec![(
                second.clone(),
                RelayObserved::new(relay, Timestamp::from(21u64)),
            )],
            &mut effects,
        );
        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(matches!(
            row_batches(&effects).as_slice(),
            [batch] if matches!(*batch, [RowDelta::Added(row)] if row.event.id == second.id)
        ));
    }

    #[test]
    fn fixed_seed_mixed_batches_match_a_forced_full_refresh_after_every_commit() {
        let keys = Keys::generate();
        let first = RelayUrl::parse("wss://differential-a.example").unwrap();
        let second = RelayUrl::parse("wss://differential-b.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let subscribe = core.handle(EngineMsg::Subscribe(unlimited_room_query(26)));
        let handle = subscribed_handle(&subscribe);
        let mut app_rows = BTreeMap::<EventId, Row>::new();
        let mut candidates = Vec::<SignedEvent>::new();
        let mut seed = 0x4d59_5df4_d0f3_3173u64;

        for step in 0..256u64 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let created_at = 1_000 + step / 3;
            let observed_at = Timestamp::from(50_000 + step);
            let batch = match seed % 5 {
                0 => {
                    let event = room_event(&keys, 26, step as usize, created_at);
                    candidates.push(event.clone());
                    vec![(event, RelayObserved::new(first.clone(), observed_at))]
                }
                1 if !candidates.is_empty() => {
                    let event = candidates[(seed as usize) % candidates.len()].clone();
                    vec![(event, RelayObserved::new(second.clone(), observed_at))]
                }
                2 if !candidates.is_empty() => {
                    let target = &candidates[(seed as usize) % candidates.len()];
                    let deletion = EventBuilder::new(Kind::EventDeletion, "")
                        .tag(Tag::event(target.id))
                        .custom_created_at(Timestamp::from(100_000 + step))
                        .sign_with_keys(&keys)
                        .unwrap();
                    vec![(deletion, RelayObserved::new(first.clone(), observed_at))]
                }
                3 => {
                    let event =
                        EventBuilder::new(Kind::from(10_000u16), format!("revision-{step}"))
                            .tag(Tag::parse(["h".to_owned(), "room-26".to_owned()]).unwrap())
                            .custom_created_at(Timestamp::from(200_000 + step))
                            .sign_with_keys(&keys)
                            .unwrap();
                    candidates.push(event.clone());
                    vec![(event, RelayObserved::new(first.clone(), observed_at))]
                }
                _ => {
                    let event = room_event(&keys, 27, step as usize, created_at);
                    vec![(event, RelayObserved::new(first.clone(), observed_at))]
                }
            };

            core.projection_store_queries.set(0);
            let mut effects = Vec::new();
            core.ingest_relay_events(batch, &mut effects);
            assert_eq!(
                core.projection_store_queries.get(),
                0,
                "unlimited ordinary handle re-read history at step {step}"
            );

            for delta in row_batches(&effects).into_iter().flatten() {
                match delta {
                    RowDelta::Added(row) => {
                        app_rows.insert(row.event.id, row.clone());
                    }
                    RowDelta::SourcesGrew { id, sources } => {
                        app_rows
                            .get_mut(id)
                            .expect("source growth follows add")
                            .sources = sources.clone();
                    }
                    RowDelta::Removed(id) => {
                        app_rows.remove(id);
                    }
                }
            }

            assert_remembered_rows_match_oracle(&core, handle);
            let remembered = &core.observations[&handle].last_rows;
            assert_eq!(app_rows.len(), remembered.len());
            for (event_id, row) in &app_rows {
                assert_eq!(row.sources, remembered[event_id].sources);
            }
        }
    }

    #[test]
    fn resolver_internal_handle_is_filtered_before_any_projection_read() {
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let (internal, _delta) = core
            .resolver
            .subscribe(room_query(1).branches()[0].clone())
            .unwrap();
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.refresh_observations_of_branches([internal.id()], &mut effects);

        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(effects.is_empty());
    }

    #[test]
    fn projected_private_relay_evidence_is_gated_without_counter_inflation() {
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let private = RelayUrl::parse("ws://127.0.0.1:7777").unwrap();
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                ids: Some(BTreeSet::from(["11".repeat(32)])),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::from([RoutingEvidence {
                relay: private,
                origin: nmp_grammar::RoutingEvidenceKind::Hint,
            }]),
        };
        let demand = BTreeSet::from([atom]);

        let admitted = core.admit_projected_routing_evidence(&demand);
        assert!(admitted.iter().next().unwrap().routing_evidence.is_empty());
        assert_eq!(core.discovered_private_relays_rejected, 1);
        core.admit_projected_routing_evidence(&demand);
        assert_eq!(
            core.discovered_private_relays_rejected, 1,
            "an unchanged recompile must not recount one rejected fact"
        );
    }

    #[test]
    fn operator_allowlist_admits_projected_local_evidence() {
        let mut core = EngineCore::new(MemoryStore::new(), 20).with_relay_admission(
            RelayAdmissionPolicy::new(["127.0.0.1".to_string()], OnionReachability::Unreachable),
        );
        let atom = ContextualAtom {
            filter: ConcreteFilter::default(),
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::from([RoutingEvidence {
                relay: RelayUrl::parse("ws://127.0.0.1:7777").unwrap(),
                origin: nmp_grammar::RoutingEvidenceKind::SourceProvenance,
            }]),
        };

        let admitted = core.admit_projected_routing_evidence(&BTreeSet::from([atom]));

        assert_eq!(admitted.iter().next().unwrap().routing_evidence.len(), 1);
        assert_eq!(core.discovered_private_relays_rejected, 0);
    }
}

#[cfg(test)]
mod coverage_evidence_refresh_tests {
    use std::borrow::Cow;

    use nmp_grammar::Filter;
    use nmp_store::MemoryStore;
    use nostr::{Kind, SubscriptionId};

    use super::*;

    fn pinned_query(relay: &RelayUrl) -> LiveQuery {
        LiveQuery::single(
            nmp_grammar::Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
                    ..Filter::default()
                },
                SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
                AccessContext::Public,
            )
            .unwrap(),
        )
    }

    fn connected_core(
        relay: &RelayUrl,
    ) -> (
        EngineCore<MemoryStore>,
        TransportRelayHandle,
        RelaySessionKey,
    ) {
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let handle = TransportRelayHandle {
            slot: 7,
            generation: 1,
        };
        let session = RelaySessionKey::public(relay.clone());
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        core.handle(EngineMsg::RelayInformationResolved(relay.clone(), None));
        core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
        (core, handle, session)
    }

    fn wire_id(effects: &[Effect]) -> String {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Wire(delta) => delta.ops.iter().find_map(|(_, ops)| {
                    ops.iter().find_map(|op| match op {
                        WireOp::Req(id, _) => Some(id.1.to_string()),
                        WireOp::Close(_) => None,
                    })
                }),
                _ => None,
            })
            .expect("subscription opens a wire request")
    }

    fn eose(
        core: &mut EngineCore<MemoryStore>,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
        wire_id: String,
    ) -> Vec<Effect> {
        core.handle(EngineMsg::Tick(Timestamp::from(101u64)));
        core.handle(EngineMsg::RelayFrame(
            handle,
            session,
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                SubscriptionId::new(wire_id),
            ))),
        ))
    }

    #[test]
    fn eose_refreshes_live_evidence_without_event_index_query() {
        let relay = RelayUrl::parse("wss://evidence-only-live.example").unwrap();
        let (mut core, transport, session) = connected_core(&relay);
        let opened = core.handle(EngineMsg::Subscribe(pinned_query(&relay)));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, _, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let wire = wire_id(&opened);
        core.projection_store_queries.set(0);

        let effects = eose(&mut core, transport, session, wire);

        assert_eq!(core.projection_store_queries.get(), 0);
        let (_, deltas, evidence) = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(handle, deltas, evidence) if *handle == id => {
                    Some((*handle, deltas, evidence))
                }
                _ => None,
            })
            .expect("coverage advance emits live evidence");
        assert!(deltas.is_empty());
        assert_eq!(
            evidence[0].sources[0].reconciled_through,
            Some(Timestamp::from(101u64))
        );
    }

    #[test]
    fn eose_does_not_requery_a_complete_history_projection() {
        let relay = RelayUrl::parse("wss://evidence-only-history.example").unwrap();
        let (mut core, transport, session) = connected_core(&relay);
        let opened = core.handle(EngineMsg::SubscribeHistory(HistoryQuery::new(
            pinned_query(&relay),
            3,
            6,
        )));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let wire = wire_id(&opened);
        let remembered = core.histories[&id].last_rows.clone();
        core.history_store_queries.set(0);

        eose(&mut core, transport, session, wire);

        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.histories[&id].last_rows, remembered);
    }

    #[test]
    fn evidence_only_refresh_falls_back_for_incomplete_projections() {
        let relay = RelayUrl::parse("wss://evidence-recovery.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let live = core.handle(EngineMsg::Subscribe(pinned_query(&relay)));
        let live_id = live
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, _, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let history = core.handle(EngineMsg::SubscribeHistory(HistoryQuery::new(
            pinned_query(&relay),
            3,
            6,
        )));
        let history_id = history
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.observations
            .get_mut(&live_id)
            .unwrap()
            .projection_complete = false;
        core.histories
            .get_mut(&history_id)
            .unwrap()
            .projection_complete = false;
        core.projection_store_queries.set(0);
        core.history_store_queries.set(0);

        let mut effects = Vec::new();
        core.refresh_all_observation_evidence(&mut effects);
        core.refresh_all_history_evidence(&mut effects);

        assert_eq!(core.projection_store_queries.get(), 1);
        assert_eq!(core.history_store_queries.get(), 1);
        assert!(core.observations[&live_id].projection_complete);
        assert!(core.histories[&history_id].projection_complete);
    }
}
