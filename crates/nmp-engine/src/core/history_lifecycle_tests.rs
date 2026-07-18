//! Ownership-domain tests moved with the implementation they falsify.

use super::*;

#[cfg(test)]
mod history_mutation_tests {
    use std::sync::{Arc, Mutex};

    use nmp_grammar::{Derived, IdentityField, IndexedTagName, Selector};
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    use super::*;

    #[derive(Clone, Default)]
    struct CapturingHistorySink(Arc<Mutex<Vec<HistoryBatch>>>);

    impl HistorySink for CapturingHistorySink {
        fn on_history(&self, batch: HistoryBatch) {
            self.0.lock().unwrap().push(batch);
        }
    }

    #[derive(Clone, Default)]
    struct CapturingRowSink(Arc<Mutex<Vec<Vec<RowDelta>>>>);

    impl RowSink for CapturingRowSink {
        fn on_rows(&self, rows: Vec<RowDelta>) {
            self.0.lock().unwrap().push(rows);
        }
    }

    #[derive(Clone, Default)]
    struct CapturingReceiptSink(Arc<Mutex<Vec<WriteStatus>>>);

    impl ReceiptSink for CapturingReceiptSink {
        fn on_status(&self, status: WriteStatus) {
            self.0.lock().unwrap().push(status);
        }
    }

    fn room_tag(room: usize) -> Tag {
        Tag::parse(["h".to_owned(), format!("room-{room}")]).unwrap()
    }

    fn room_event(keys: &Keys, room: usize, ordinal: usize, created_at: u64) -> SignedEvent {
        EventBuilder::new(Kind::from(9u16), format!("room-{room}-{ordinal}"))
            .tag(room_tag(room))
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn history_query(room: usize, kinds: BTreeSet<u16>) -> HistoryQuery {
        HistoryQuery::new(
            LiveQuery::from_filter(Filter {
                kinds: Some(kinds),
                tags: BTreeMap::from([(
                    IndexedTagName::new('h').unwrap(),
                    Binding::Literal(BTreeSet::from([format!("room-{room}")])),
                )]),
                ..Filter::default()
            }),
            3,
            6,
        )
    }

    fn open_six(
        events: &[SignedEvent],
        kinds: BTreeSet<u16>,
        relay: &RelayUrl,
    ) -> (
        EngineCore<MemoryStore>,
        HistorySessionId,
        CapturingHistorySink,
    ) {
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                events
                    .iter()
                    .cloned()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, kinds),
            Box::new(sink.clone()),
        ));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let loaded = core.handle(EngineMsg::RequestRows(id, 6));
        assert!(loaded.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(core.histories[&id].last_rows.len(), 6);
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        (core, id, sink)
    }

    fn ordered_ids<S: EventStore>(core: &EngineCore<S>, id: HistorySessionId) -> Vec<EventId> {
        core.histories[&id]
            .order
            .iter()
            .map(|(_, event_id)| *event_id)
            .collect()
    }

    fn ingest<S: EventStore>(
        core: &mut EngineCore<S>,
        event: SignedEvent,
        relay: RelayUrl,
        observed_at: u64,
    ) {
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                event,
                RelayObserved::new(relay, Timestamp::from(observed_at)),
            )],
            &mut effects,
        );
    }

    fn assert_one_atomic_batch(sink: &CapturingHistorySink) -> HistoryBatch {
        let batches = sink.0.lock().unwrap();
        assert_eq!(
            batches.len(),
            1,
            "one store commit must emit one history batch"
        );
        batches[0].clone()
    }

    #[test]
    fn bounded_history_mutations_touch_only_delta_and_exact_lower_segment() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-mutation.example").unwrap();
        let second = RelayUrl::parse("wss://history-second.example").unwrap();
        let base: Vec<_> = (0..12)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();

        // First boundary insertion is old-window + inserted -> top-N: no
        // store read, and Added+Removed travel in one atomic batch.
        let (mut core, id, sink) = open_six(&base, BTreeSet::from([9]), &relay);
        let inserted = room_event(&keys, 47, 99, 1_000);
        ingest(&mut core, inserted.clone(), relay.clone(), 2_000);
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.history_rows_examined.get(), 0);
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == inserted.id)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(_))));
        assert_eq!(core.histories[&id].last_rows.len(), 6);

        // Middle provenance growth is exact from the committed fact.
        let middle = ordered_ids(&core, id)[2];
        let middle_event = core
            .resolver
            .store()
            .query(&nostr::Filter::new().id(middle))
            .unwrap()
            .pop()
            .unwrap()
            .event;
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, middle_event, second.clone(), 2_001);
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (0, 0)
        );
        assert!(matches!(
            batch.deltas.as_slice(),
            [RowDelta::SourcesGrew { id: changed, sources }]
                if *changed == middle && sources.contains(&relay) && sources.contains(&second)
        ));

        // Middle deletion performs one exclusive cursor read for exactly one
        // replacement row; it never replays all six retained rows.
        let target = ordered_ids(&core, id)[2];
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target))
            .custom_created_at(Timestamp::from(3_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, deletion, relay.clone(), 3_001);
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1)
        );
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == target)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(_))));

        // The lower boundary uses the same one-row segment, proving cursor
        // work does not depend on retained-window size.
        let target = *ordered_ids(&core, id).last().unwrap();
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target))
            .custom_created_at(Timestamp::from(3_100u64))
            .sign_with_keys(&keys)
            .unwrap();
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, deletion, relay.clone(), 3_101);
        assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1)
        );
    }

    #[test]
    fn strict_history_counts_only_pinned_provenance_before_applying_page_bounds() {
        let keys = Keys::generate();
        let wanted = RelayUrl::parse("wss://history-strict.example").unwrap();
        let other = RelayUrl::parse("wss://history-other.example").unwrap();
        let mut store = MemoryStore::new();
        for (created_at, relay, ordinal) in [
            (600, other.clone(), 0),
            (500, other.clone(), 1),
            (400, wanted.clone(), 2),
            (300, wanted.clone(), 3),
            (200, wanted.clone(), 4),
            (100, wanted.clone(), 5),
        ] {
            store
                .insert(
                    room_event(&keys, 47, ordinal, created_at),
                    RelayObserved::new(relay, Timestamp::from(1_000u64)),
                )
                .unwrap();
        }
        let selection = history_query(47, BTreeSet::from([9]))
            .live_query()
            .0
            .selection
            .clone();
        let query = HistoryQuery::new(
            LiveQuery(nmp_grammar::Demand {
                selection,
                source: SourceAuthority::Pinned(BTreeSet::from([wanted])),
                access: AccessContext::Public,
                cache: CacheMode::Strict,
                freshness: Freshness::Live,
            }),
            2,
            4,
        );
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(query, Box::new(sink.clone())));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            ordered_ids(&core, id)
                .iter()
                .map(|event_id| {
                    core.histories[&id].last_rows[event_id]
                        .event
                        .created_at
                        .as_secs()
                })
                .collect::<Vec<_>>(),
            vec![400, 300]
        );

        core.handle(EngineMsg::RequestRows(id, 4));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(
            ordered_ids(&core, id)
                .iter()
                .map(|event_id| {
                    core.histories[&id].last_rows[event_id]
                        .event
                        .created_at
                        .as_secs()
                })
                .collect::<Vec<_>>(),
            vec![400, 300, 200, 100]
        );
    }

    #[test]
    fn strict_and_agnostic_live_mutations_stay_distinct_and_match_their_oracles() {
        let keys = Keys::generate();
        let wanted = RelayUrl::parse("wss://history-live-wanted.example").unwrap();
        let other = RelayUrl::parse("wss://history-live-other.example").unwrap();
        let other_newest = room_event(&keys, 47, 0, 400);
        let wanted_a = room_event(&keys, 47, 1, 300);
        let wanted_b = room_event(&keys, 47, 2, 200);
        let wanted_c = room_event(&keys, 47, 3, 100);
        let mut store = MemoryStore::new();
        for (event, source) in [
            (other_newest.clone(), other.clone()),
            (wanted_a.clone(), wanted.clone()),
            (wanted_b.clone(), wanted.clone()),
            (wanted_c.clone(), wanted.clone()),
        ] {
            store
                .insert(event, RelayObserved::new(source, Timestamp::from(1_000u64)))
                .unwrap();
        }
        let selection = history_query(47, BTreeSet::from([9]))
            .live_query()
            .0
            .selection
            .clone();
        let strict_query = HistoryQuery::new(
            LiveQuery(nmp_grammar::Demand {
                selection,
                source: SourceAuthority::Pinned(BTreeSet::from([wanted.clone()])),
                access: AccessContext::Public,
                cache: CacheMode::Strict,
                freshness: Freshness::Live,
            }),
            3,
            3,
        );
        let agnostic_query = HistoryQuery::new(
            history_query(47, BTreeSet::from([9])).live_query().clone(),
            3,
            3,
        );
        let strict_sink = CapturingHistorySink::default();
        let agnostic_sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let strict_id = core
            .handle(EngineMsg::SubscribeHistory(
                strict_query,
                Box::new(strict_sink.clone()),
            ))
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let agnostic_id = core
            .handle(EngineMsg::SubscribeHistory(
                agnostic_query,
                Box::new(agnostic_sink.clone()),
            ))
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            ordered_ids(&core, strict_id),
            vec![wanted_a.id, wanted_b.id, wanted_c.id]
        );
        assert_eq!(
            ordered_ids(&core, agnostic_id),
            vec![other_newest.id, wanted_a.id, wanted_b.id]
        );
        strict_sink.0.lock().unwrap().clear();
        agnostic_sink.0.lock().unwrap().clear();

        let new = room_event(&keys, 47, 99, 500);
        ingest(&mut core, new.clone(), other.clone(), 2_000);
        assert!(strict_sink.0.lock().unwrap().is_empty());
        assert_eq!(ordered_ids(&core, strict_id)[0], wanted_a.id);
        assert_eq!(ordered_ids(&core, agnostic_id)[0], new.id);

        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        core.history_affected_row_queries.set(0);
        ingest(&mut core, new.clone(), wanted.clone(), 2_001);
        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.history_rows_examined.get(), 0);
        assert_eq!(core.history_affected_row_queries.get(), 1);
        assert_eq!(ordered_ids(&core, strict_id)[0], new.id);
        let strict_new = &core.histories[&strict_id].last_rows[&new.id];
        assert_eq!(
            strict_new.sources,
            BTreeSet::from([other.clone(), wanted.clone()]),
            "a newly Strict-eligible row carries its complete canonical provenance"
        );

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(new.id))
            .custom_created_at(Timestamp::from(3_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        strict_sink.0.lock().unwrap().clear();
        agnostic_sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, deletion, wanted, 3_001);
        assert_eq!(core.history_store_queries.get(), 2);
        assert_eq!(core.history_rows_examined.get(), 2);
        assert_eq!(strict_sink.0.lock().unwrap().len(), 1);
        assert_eq!(agnostic_sink.0.lock().unwrap().len(), 1);

        for history_id in [strict_id, agnostic_id] {
            let (oracle, _) = core.history_rows_and_evidence_for(history_id).unwrap();
            assert_eq!(core.histories[&history_id].last_rows, oracle);
        }
    }

    #[test]
    fn replacement_and_expiry_rebalance_without_full_history_replay() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-replace-expire.example").unwrap();
        let mut base: Vec<_> = (0..11)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let replaceable = EventBuilder::new(Kind::from(10_000u16), "old")
            .tag(room_tag(47))
            .custom_created_at(Timestamp::from(108u64))
            .sign_with_keys(&keys)
            .unwrap();
        base.push(replaceable.clone());
        let (mut core, id, sink) = open_six(&base, BTreeSet::from([9, 10_000]), &relay);
        assert!(core.histories[&id].last_rows.contains_key(&replaceable.id));
        let replacement = EventBuilder::new(Kind::from(10_000u16), "new")
            .tag(room_tag(47))
            .custom_created_at(Timestamp::from(1_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        ingest(&mut core, replacement.clone(), relay.clone(), 2_000);
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1)
        );
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == replaceable.id)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == replacement.id)));

        let expiring = EventBuilder::new(Kind::from(9u16), "expires")
            .tag(room_tag(47))
            .tag(Tag::expiration(Timestamp::from(5_000u64)))
            .custom_created_at(Timestamp::from(900u64))
            .sign_with_keys(&keys)
            .unwrap();
        sink.0.lock().unwrap().clear();
        ingest(&mut core, expiring.clone(), relay, 2_001);
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        core.tick(Timestamp::from(5_000u64));
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1)
        );
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == expiring.id)));
    }

    #[test]
    fn replaceable_compensation_cannot_let_restored_older_row_mask_hidden_tail() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-compensation.example").unwrap();
        let x = room_event(&keys, 47, 1, 900);
        let y = room_event(&keys, 47, 2, 800);
        let z = room_event(&keys, 47, 3, 700);
        let predecessor = EventBuilder::new(Kind::from(10_000u16), "prior")
            .tag(room_tag(47))
            .custom_created_at(Timestamp::from(100u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                [x.clone(), y.clone(), z.clone(), predecessor.clone()]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        core.active_pubkey = Some(keys.public_key());
        let opened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9, 10_000])),
            Box::new(sink.clone()),
        ));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(ordered_ids(&core, id), vec![x.id, y.id, z.id]);
        sink.0.lock().unwrap().clear();

        let accepted = core.on_publish(
            WriteIntent {
                payload: WritePayload::UnsignedReplaceableEdit {
                    unsigned: UnsignedEvent::new(
                        keys.public_key(),
                        Timestamp::from(1_000u64),
                        Kind::from(10_000u16),
                        vec![room_tag(47)],
                        "pending replacement",
                    ),
                    expected_base: Some(predecessor.id),
                },
                durability: Durability::Durable,
                routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(relay)),
                identity_override: None,
                correlation: None,
            },
            Box::new(CapturingReceiptSink::default()),
        );
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("replaceable local acceptance emits a receipt");
        let pending = *ordered_ids(&core, id).first().unwrap();
        assert_eq!(ordered_ids(&core, id)[1..], [x.id, y.id]);

        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let _ = core.cancel_write(receipt);

        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1),
            "one old-boundary reconciliation finds Z despite predecessor restoring count"
        );
        assert_eq!(ordered_ids(&core, id), vec![x.id, y.id, z.id]);
        assert!(!core.histories[&id].last_rows.contains_key(&predecessor.id));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == pending)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == z.id)));
        assert!(!batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == predecessor.id)));
    }

    #[test]
    fn fixed_seed_mixed_remove_insert_batches_match_full_history_oracle() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-differential.example").unwrap();
        let base: Vec<_> = (0..30)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let (mut core, id, sink) = open_six(&base, BTreeSet::from([9]), &relay);
        let mut seed = 0x6a09_e667_f3bc_c909u64;

        for step in 0..64usize {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let visible = ordered_ids(&core, id);
            let removed_id = visible[(seed as usize) % visible.len()];
            let removed = core
                .resolver
                .store()
                .query(&nostr::Filter::new().id(removed_id))
                .unwrap()
                .pop()
                .unwrap()
                .event;
            core.resolver
                .store_mut()
                .remove(removed_id, nmp_store::RetractReason::Rejected)
                .unwrap();

            seed = seed.rotate_left(17) ^ 0xa5a5_5a5a_0123_4567;
            let created_at = 50 + (seed % 1_500);
            let inserted = room_event(&keys, 47, 10_000 + step, created_at);
            core.resolver
                .store_mut()
                .insert(
                    inserted.clone(),
                    RelayObserved::new(relay.clone(), Timestamp::from(2_000 + step as u64)),
                )
                .unwrap();
            let changes = CommittedRowChanges {
                inserted: vec![nmp_resolver::CommittedCurrentRow {
                    event: inserted,
                    observed_relays: BTreeSet::from([relay.clone()]),
                }],
                removed: vec![removed],
                provenance_grew: Vec::new(),
            };

            sink.0.lock().unwrap().clear();
            core.history_store_queries.set(0);
            core.history_rows_examined.set(0);
            let mut effects = Vec::new();
            assert!(core.try_apply_committed_history_row_changes(id, &changes, &mut effects));
            assert!(core.history_store_queries.get() <= 1);
            assert!(core.history_rows_examined.get() <= 1);
            assert!(sink.0.lock().unwrap().len() <= 1);

            let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
            assert_eq!(
                core.histories[&id].last_rows, oracle,
                "incremental history diverged from full oracle at mixed batch {step}"
            );
        }
    }

    #[test]
    fn derived_multi_root_advanced_history_mutates_with_one_bounded_reconciliation() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-multi-root.example").unwrap();
        let addressable = |d: &str, created_at: u64, content: &str| {
            EventBuilder::new(Kind::from(30_003u16), content)
                .tag(Tag::identifier(d))
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let base: Vec<_> = (0..8)
            .map(|index| addressable(&format!("g{index}"), 100 + index, "base"))
            .collect();
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                base.iter()
                    .cloned()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let selection = nmp_grammar::Filter {
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: nmp_grammar::Demand::from_filter(nmp_grammar::Filter {
                    kinds: Some(BTreeSet::from([30_003u16])),
                    authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                    ..nmp_grammar::Filter::default()
                }),
                project: Selector::AddressCoord,
            }))),
            ..nmp_grammar::Filter::default()
        };
        let query = HistoryQuery::new(LiveQuery::from_filter(selection), 3, 6);
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let opened = core.handle(EngineMsg::SubscribeHistory(query, Box::new(sink.clone())));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(id, 6));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(core.histories[&id].last_rows.len(), 6);
        let primary = *core.histories[&id].handle_ids.first().unwrap();
        assert_eq!(core.resolver.root_atoms(primary).len(), 8);
        assert!(core.resolver.subtree_atoms(primary).len() > 8);

        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let replacement = addressable("g7", 1_000, "replacement");
        ingest(&mut core, replacement.clone(), relay, 2_000);

        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(core.history_store_queries.get(), 1);
        assert!(core.history_rows_examined.get() <= 1);
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == replacement.id)));
        let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
        assert_eq!(core.histories[&id].last_rows, oracle);
    }

    #[test]
    fn late_same_second_boundary_insert_after_advance_is_exact_and_read_free() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-late-tie.example").unwrap();
        let base: Vec<_> = [600u64, 500, 400, 300, 200, 100]
            .into_iter()
            .enumerate()
            .map(|(index, created_at)| room_event(&keys, 47, index, created_at))
            .collect();
        let old_boundary = base.last().unwrap().clone();
        let (mut core, id, sink) = open_six(&base, BTreeSet::from([9]), &relay);
        let late = (0..1_000usize)
            .map(|ordinal| room_event(&keys, 47, 20_000 + ordinal, 100))
            .find(|event| event.id < old_boundary.id)
            .expect("deterministically find an id that sorts before the old tie boundary");

        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, late.clone(), relay, 2_000);

        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.history_rows_examined.get(), 0);
        assert!(core.histories[&id].last_rows.contains_key(&late.id));
        assert!(!core.histories[&id].last_rows.contains_key(&old_boundary.id));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == late.id)));
        assert!(batch.deltas.iter().any(
            |delta| matches!(delta, RowDelta::Removed(event_id) if *event_id == old_boundary.id)
        ));
        let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
        assert_eq!(core.histories[&id].last_rows, oracle);
    }

    #[test]
    fn redb_advanced_history_matches_oracle_after_insert_and_retraction() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-redb.example").unwrap();
        let base: Vec<_> = (0..12)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let mut store = nmp_store::RedbStore::open(dir.path().join("history.redb")).unwrap();
        store
            .insert_batch(
                base.iter()
                    .cloned()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9])),
            Box::new(sink.clone()),
        ));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(id, 6));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        sink.0.lock().unwrap().clear();

        let inserted = room_event(&keys, 47, 99, 1_000);
        ingest(&mut core, inserted, relay.clone(), 2_000);
        sink.0.lock().unwrap().clear();
        let removed = ordered_ids(&core, id)[2];
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(removed))
            .custom_created_at(Timestamp::from(3_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, deletion, relay, 3_001);

        assert_one_atomic_batch(&sink);
        assert_eq!(core.history_store_queries.get(), 1);
        assert_eq!(core.history_rows_examined.get(), 1);
        let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
        assert_eq!(core.histories[&id].last_rows, oracle);
    }

    #[test]
    fn staged_load_rollback_and_cancel_restore_exact_session_ownership() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-rollback.example").unwrap();
        let events: Vec<_> = (0..9)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                events
                    .iter()
                    .cloned()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9])),
            Box::new(sink.clone()),
        ));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let row_sink = CapturingRowSink::default();
        let ordinary = core.handle(EngineMsg::Subscribe(
            history_query(47, BTreeSet::from([9])).live_query().clone(),
            Box::new(row_sink.clone()),
        ));
        let ordinary_id = ordinary
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(handle, _, _) => Some(*handle),
                _ => None,
            })
            .unwrap();
        let second_sink = CapturingHistorySink::default();
        let second_open = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9])),
            Box::new(second_sink.clone()),
        ));
        let second_id = second_open
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(candidate, _) if *candidate != id => Some(*candidate),
                _ => None,
            })
            .unwrap();
        sink.0.lock().unwrap().clear();
        row_sink.0.lock().unwrap().clear();
        second_sink.0.lock().unwrap().clear();

        let prior_rows = core.histories[&id].last_rows.clone();
        let prior_order = core.histories[&id].order.clone();
        let prior_evidence = core.histories[&id].last_evidence.clone();
        let prior_handles = core.histories[&id].handle_ids.clone();
        let ordinary_prior_rows = core.handles[&ordinary_id].last_rows.clone();
        let ordinary_prior_evidence = core.handles[&ordinary_id].last_evidence.clone();
        let second_prior_rows = core.histories[&second_id].last_rows.clone();
        let second_prior_evidence = core.histories[&second_id].last_evidence.clone();
        let second_prior_handles = core.histories[&second_id].handle_ids.clone();

        // A staged advance mutates only this session's retained projection
        // and is observable on NO sink until commit; every other projection
        // is untouched.
        let staged = core.handle(EngineMsg::RequestRows(id, 6));
        assert!(staged.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        assert!(core.histories[&id].pending_load.is_some());
        assert_eq!(core.histories[&id].last_rows.len(), 6);
        assert!(
            sink.0.lock().unwrap().is_empty(),
            "staged rows are not observable"
        );
        assert!(row_sink.0.lock().unwrap().is_empty());
        assert!(second_sink.0.lock().unwrap().is_empty());
        assert_eq!(core.handles[&ordinary_id].last_rows, ordinary_prior_rows);
        assert_eq!(
            core.handles[&ordinary_id].last_evidence,
            ordinary_prior_evidence
        );
        assert_eq!(core.histories[&second_id].last_rows, second_prior_rows);
        assert_eq!(
            core.histories[&second_id].last_evidence,
            second_prior_evidence
        );
        assert_eq!(core.histories[&second_id].handle_ids, second_prior_handles);

        core.handle(EngineMsg::RollbackHistoryLoad(id));
        let state = &core.histories[&id];
        assert_eq!(state.last_rows, prior_rows);
        assert_eq!(state.order, prior_order);
        assert_eq!(state.last_evidence, prior_evidence);
        assert_eq!(state.target_rows, 3);
        assert_eq!(state.handle_ids, prior_handles);
        assert!(state.pending_load.is_none());
        assert!(row_sink.0.lock().unwrap().is_empty());
        assert!(second_sink.0.lock().unwrap().is_empty());

        // The identical declarative request retries cleanly after rollback.
        let retried = core.handle(EngineMsg::RequestRows(id, 6));
        assert!(retried.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(core.histories[&id].last_rows.len(), 6);
        let delivered = sink.0.lock().unwrap();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].load, WindowLoad::Requesting);
        assert_eq!(delivered[1].load, WindowLoad::Returned { added: 3 });
        assert_eq!(
            delivered[1]
                .evidence
                .shortfall
                .iter()
                .filter(|fact| matches!(fact, ShortfallFact::NoPlannedSource { .. }))
                .count(),
            3,
            "initial, exact tie-second, and older handles all contribute evidence"
        );
        drop(delivered);

        let owned_handles = core.histories[&id].handle_ids.clone();
        core.handle(EngineMsg::UnsubscribeHistory(id));
        assert!(!core.histories.contains_key(&id));
        assert!(core.history_by_handle.values().all(|owner| *owner != id));
        for handle in owned_handles {
            assert!(core.resolver.root_atoms(handle).is_empty());
        }

        let active_sink = CapturingHistorySink::default();
        let reopened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9])),
            Box::new(active_sink),
        ));
        let active_id = reopened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(active_id, 6));
        let active_handles = core.histories[&active_id].handle_ids.clone();
        assert!(core.histories[&active_id].pending_load.is_some());
        core.handle(EngineMsg::UnsubscribeHistory(active_id));
        assert!(!core.histories.contains_key(&active_id));
        assert!(core
            .history_by_handle
            .values()
            .all(|owner| *owner != active_id));
        for handle in active_handles {
            assert!(core.resolver.root_atoms(handle).is_empty());
        }
    }
}
