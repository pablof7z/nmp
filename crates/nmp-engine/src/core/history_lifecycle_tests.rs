//! Ownership-domain tests moved with the implementation they falsify.

use super::*;

#[cfg(test)]
mod history_mutation_tests {
    use nmp_grammar::{Binding, Derived, Filter, IdentityField, IndexedTagName, Selector};
    use nmp_store::RedbStore;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    use super::*;

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
            LiveQuery::single(nmp_grammar::Demand::public(Filter {
                kinds: Some(kinds),
                tags: BTreeMap::from([(
                    IndexedTagName::new('h').unwrap(),
                    Binding::Literal(BTreeSet::from([format!("room-{room}")])),
                )]),
                ..Filter::default()
            })),
            3,
            6,
        )
    }

    fn open_six(
        events: &[SignedEvent],
        kinds: BTreeSet<u16>,
        relay: &RelayUrl,
    ) -> (EngineCore, HistorySessionId) {
        let mut store = RedbStore::temporary().expect("temporary Redb store");
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
        let mut core = EngineCore::new(store, 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(history_query(47, kinds)));
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
        assert_eq!(core.history.projection(id).len(), 6);
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        (core, id)
    }

    fn ordered_ids(core: &EngineCore, id: HistorySessionId) -> Vec<EventId> {
        core.history.projection(id).ids()
    }

    /// The window's incremental projection against a full recomputation from
    /// the canonical store, compared as the ordered row list rather than as
    /// the `last_rows` map the oracle happens to return: the incremental path
    /// maintains membership and canonical position in two collections, and
    /// only comparing the fused list can see them disagree.
    fn assert_matches_oracle(core: &EngineCore, id: HistorySessionId, at: &str) {
        let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
        let mut expected: Vec<_> = oracle.into_values().collect();
        expected.sort_by_key(|row| (Reverse(row.created_at().as_secs()), row.id()));
        assert_eq!(
            core.history.projection(id).rows,
            expected,
            "incremental history diverged from the full oracle {at}"
        );
    }

    fn ingest(
        core: &mut EngineCore,
        event: SignedEvent,
        relay: RelayUrl,
        observed_at: u64,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                event,
                RelayObserved::new(relay, Timestamp::from(observed_at)),
            )],
            &mut effects,
        );
        effects
    }

    fn assert_one_atomic_batch(effects: &[Effect], history_id: HistorySessionId) -> HistoryBatch {
        let batches: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::EmitHistory(candidate, batch) if *candidate == history_id => {
                    Some(batch.clone())
                }
                _ => None,
            })
            .collect();
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
        let (mut core, id) = open_six(&base, BTreeSet::from([9]), &relay);
        let inserted = room_event(&keys, 47, 99, 1_000);
        let effects = ingest(&mut core, inserted.clone(), relay.clone(), 2_000);
        let batch = assert_one_atomic_batch(&effects, id);
        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.history_rows_examined.get(), 0);
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == inserted.id)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(_))));
        assert_eq!(core.history.projection(id).len(), 6);

        // Middle provenance growth is exact from the committed fact.
        let middle = ordered_ids(&core, id)[2];
        let middle_event = core
            .store
            .query(&nostr::Filter::new().id(middle))
            .unwrap()
            .pop()
            .unwrap()
            .event;
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let effects = ingest(&mut core, middle_event, second.clone(), 2_001);
        let batch = assert_one_atomic_batch(&effects, id);
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
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let effects = ingest(&mut core, deletion, relay.clone(), 3_001);
        let batch = assert_one_atomic_batch(&effects, id);
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
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let effects = ingest(&mut core, deletion, relay.clone(), 3_101);
        assert_one_atomic_batch(&effects, id);
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
        let mut store = RedbStore::temporary().expect("temporary Redb store");
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
            .branches()[0]
            .selection
            .clone();
        let query = HistoryQuery::new(
            LiveQuery::single(nmp_grammar::Demand {
                selection,
                source: SourceAuthority::Pinned(BTreeSet::from([wanted])),
                access: AccessContext::Public,
                cache: CacheMode::Strict,
                freshness: Freshness::Live,
            }),
            2,
            4,
        );
        let mut core = EngineCore::new(store, 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(query));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            core.history
                .projection(id)
                .rows
                .iter()
                .map(|row| row.created_at().as_secs())
                .collect::<Vec<_>>(),
            vec![400, 300]
        );

        core.handle(EngineMsg::RequestRows(id, 4));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(
            core.history
                .projection(id)
                .rows
                .iter()
                .map(|row| row.created_at().as_secs())
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
        let mut store = RedbStore::temporary().expect("temporary Redb store");
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
            .branches()[0]
            .selection
            .clone();
        let strict_query = HistoryQuery::new(
            LiveQuery::single(nmp_grammar::Demand {
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
        let mut core = EngineCore::new(store, 20);
        let strict_id = core
            .handle(EngineMsg::SubscribeHistory(strict_query))
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let agnostic_id = core
            .handle(EngineMsg::SubscribeHistory(agnostic_query))
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

        let new = room_event(&keys, 47, 99, 500);
        let effects = ingest(&mut core, new.clone(), other.clone(), 2_000);
        assert!(!effects.iter().any(
            |effect| matches!(effect, Effect::EmitHistory(candidate, _) if *candidate == strict_id)
        ));
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
        let projection = core.history.projection(strict_id);
        let strict_new = projection.row(&new.id).expect("the new row is projected");
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
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let effects = ingest(&mut core, deletion, wanted, 3_001);
        assert_eq!(core.history_store_queries.get(), 2);
        assert_eq!(core.history_rows_examined.get(), 2);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::EmitHistory(candidate, _) if *candidate == strict_id))
                .count(),
            1
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::EmitHistory(candidate, _) if *candidate == agnostic_id))
                .count(),
            1
        );

        for history_id in [strict_id, agnostic_id] {
            assert_matches_oracle(&core, history_id, "after the strict/agnostic deletion");
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
        let (mut core, id) = open_six(&base, BTreeSet::from([9, 10_000]), &relay);
        assert!(core.history.projection(id).holds(&replaceable.id));
        let replacement = EventBuilder::new(Kind::from(10_000u16), "new")
            .tag(room_tag(47))
            .custom_created_at(Timestamp::from(1_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        let effects = ingest(&mut core, replacement.clone(), relay.clone(), 2_000);
        let batch = assert_one_atomic_batch(&effects, id);
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
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == replacement.id)));

        let expiring = EventBuilder::new(Kind::from(9u16), "expires")
            .tag(room_tag(47))
            .tag(Tag::expiration(Timestamp::from(5_000u64)))
            .custom_created_at(Timestamp::from(900u64))
            .sign_with_keys(&keys)
            .unwrap();
        ingest(&mut core, expiring.clone(), relay, 2_001);
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let effects = core.tick(Timestamp::from(5_000u64));
        let batch = assert_one_atomic_batch(&effects, id);
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
        let mut store = RedbStore::temporary().expect("temporary Redb store");
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
        let mut core = EngineCore::new(store, 20);
        core.white_box("active_pubkey", |s| {
            s.active_pubkey = Some(keys.public_key())
        });
        let opened = core.handle(EngineMsg::SubscribeHistory(history_query(
            47,
            BTreeSet::from([9, 10_000]),
        )));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(ordered_ids(&core, id), vec![x.id, y.id, z.id]);

        let accepted = core.white_box("on_publish", |s| {
            s.on_publish(WriteIntent {
                payload: WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::from(10_000u16),
                    tags: (vec![room_tag(47)]).into_iter().collect(),
                    content: ("pending replacement").into(),
                    created_at: Some(Timestamp::from(1_000u64)),
                }),
                routing: WriteRouting::Explicit(vec![relay]),
                identity: Identity::Active,
            })
        });
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::WriteAccepted(id, _) => Some(*id),
                _ => None,
            })
            .expect("replaceable local acceptance emits a receipt");
        let pending = *ordered_ids(&core, id).first().unwrap();
        assert_eq!(ordered_ids(&core, id)[1..], [x.id, y.id]);

        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let effects = core.cancel_write(receipt).1;

        let batch = assert_one_atomic_batch(&effects, id);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1),
            "one old-boundary reconciliation finds Z despite predecessor restoring count"
        );
        assert_eq!(ordered_ids(&core, id), vec![x.id, y.id, z.id]);
        assert!(!core.history.projection(id).holds(&predecessor.id));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == pending)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == z.id)));
        assert!(!batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == predecessor.id)));
    }

    #[test]
    fn fixed_seed_mixed_remove_insert_batches_match_full_history_oracle() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-differential.example").unwrap();
        let base: Vec<_> = (0..30)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let (mut core, id) = open_six(&base, BTreeSet::from([9]), &relay);
        let mut seed = 0x6a09_e667_f3bc_c909u64;

        for step in 0..64usize {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let visible = ordered_ids(&core, id);
            let removed_id = visible[(seed as usize) % visible.len()];
            let removed = core
                .store
                .query(&nostr::Filter::new().id(removed_id))
                .unwrap()
                .pop()
                .unwrap()
                .event;
            core.white_box("store.remove", |s| {
                s.store
                    .remove(removed_id, nmp_store::RetractReason::Rejected)
                    .unwrap()
            });

            seed = seed.rotate_left(17) ^ 0xa5a5_5a5a_0123_4567;
            let created_at = 50 + (seed % 1_500);
            let inserted = room_event(&keys, 47, 10_000 + step, created_at);
            core.white_box("store.insert", |s| {
                s.store
                    .insert(
                        inserted.clone(),
                        RelayObserved::new(relay.clone(), Timestamp::from(2_000 + step as u64)),
                    )
                    .unwrap()
            });
            let changes = CommittedRowChanges {
                inserted: vec![nmp_resolver::CommittedCurrentRow {
                    event: inserted,
                    observed_relays: BTreeSet::from([relay.clone()]),
                    locally_accepted: false,
                    signature_state: nmp_store::SigState::Signed,
                }],
                removed: vec![removed],
                provenance_grew: Vec::new(),
                updated: Vec::new(),
            };

            core.history_store_queries.set(0);
            core.history_rows_examined.set(0);
            let mut effects = Vec::new();
            assert!(
                core.white_box("try_apply_committed_history_row_changes", |s| s
                    .try_apply_committed_history_row_changes(id, &changes, &mut effects))
            );
            assert!(core.history_store_queries.get() <= 1);
            assert!(core.history_rows_examined.get() <= 1);

            assert_matches_oracle(&core, id, &format!("at mixed batch {step}"));
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
        let mut store = RedbStore::temporary().expect("temporary Redb store");
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
                inner: nmp_grammar::Demand::author_outboxes(nmp_grammar::Filter {
                    kinds: Some(BTreeSet::from([30_003u16])),
                    authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                    ..nmp_grammar::Filter::default()
                })
                .expect("the selection binds `authors`"),
                project: Selector::AddressCoord,
            }))),
            ..nmp_grammar::Filter::default()
        };
        let query = HistoryQuery::new(
            LiveQuery::single(
                nmp_grammar::Demand::author_outboxes(selection)
                    .expect("the selection binds `authors`"),
            ),
            3,
            6,
        );
        let mut core = EngineCore::new(store, 20);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let opened = core.handle(EngineMsg::SubscribeHistory(query));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(id, 6));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(core.history.projection(id).len(), 6);
        let primary = *core
            .history
            .projection(id)
            .handle_ids
            .first()
            .expect("an opened window holds at least one resolver handle");
        assert_eq!(core.resolver.root_atoms(primary).len(), 8);
        assert!(core.resolver.subtree_atoms(primary).len() > 8);

        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let replacement = addressable("g7", 1_000, "replacement");
        let effects = ingest(&mut core, replacement.clone(), relay, 2_000);

        let batch = assert_one_atomic_batch(&effects, id);
        assert_eq!(core.history_store_queries.get(), 1);
        assert!(core.history_rows_examined.get() <= 1);
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == replacement.id)));
        assert_matches_oracle(&core, id, "after the multi-root replacement");
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
        let (mut core, id) = open_six(&base, BTreeSet::from([9]), &relay);
        let late = (0..1_000usize)
            .map(|ordinal| room_event(&keys, 47, 20_000 + ordinal, 100))
            .find(|event| event.id < old_boundary.id)
            .expect("deterministically find an id that sorts before the old tie boundary");

        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let effects = ingest(&mut core, late.clone(), relay, 2_000);

        let batch = assert_one_atomic_batch(&effects, id);
        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.history_rows_examined.get(), 0);
        let projection = core.history.projection(id);
        assert!(projection.holds(&late.id));
        assert!(!projection.holds(&old_boundary.id));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == late.id)));
        assert!(batch.deltas.iter().any(
            |delta| matches!(delta, RowDelta::Removed(event_id) if *event_id == old_boundary.id)
        ));
        assert_matches_oracle(&core, id, "after the late tie-second insert");
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
        let mut core = EngineCore::new(store, 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(history_query(
            47,
            BTreeSet::from([9]),
        )));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(id, 6));
        core.handle(EngineMsg::CommitHistoryLoad(id));

        let inserted = room_event(&keys, 47, 99, 1_000);
        ingest(&mut core, inserted, relay.clone(), 2_000);
        let removed = ordered_ids(&core, id)[2];
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(removed))
            .custom_created_at(Timestamp::from(3_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let effects = ingest(&mut core, deletion, relay, 3_001);

        assert_one_atomic_batch(&effects, id);
        assert_eq!(core.history_store_queries.get(), 1);
        assert_eq!(core.history_rows_examined.get(), 1);
        assert_matches_oracle(&core, id, "after the Redb insert and retraction");
    }

    #[test]
    fn staged_load_rollback_and_cancel_restore_exact_session_ownership() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-rollback.example").unwrap();
        let events: Vec<_> = (0..9)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let mut store = RedbStore::temporary().expect("temporary Redb store");
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
        let mut core = EngineCore::new(store, 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(history_query(
            47,
            BTreeSet::from([9]),
        )));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let ordinary = core.handle(EngineMsg::Subscribe(
            history_query(47, BTreeSet::from([9])).live_query().clone(),
        ));
        let ordinary_id = ordinary
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(handle, _, _) => Some(*handle),
                _ => None,
            })
            .unwrap();
        let second_open = core.handle(EngineMsg::SubscribeHistory(history_query(
            47,
            BTreeSet::from([9]),
        )));
        let second_id = second_open
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(candidate, _) if *candidate != id => Some(*candidate),
                _ => None,
            })
            .unwrap();

        let prior = core.history.projection(id);
        let ordinary_prior_rows = core.observations[&ordinary_id].last_rows.clone();
        let ordinary_prior_evidence = core.observations[&ordinary_id].last_evidence.clone();
        let second_prior = core.history.projection(second_id);

        // A staged advance mutates only this session's retained projection
        // and emits no delivery fact until commit; every other projection
        // is untouched.
        let staged = core.handle(EngineMsg::RequestRows(id, 6));
        assert!(staged.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        assert!(core.history.projection(id).advance_staged);
        assert_eq!(core.history.projection(id).len(), 6);
        assert!(!staged
            .iter()
            .any(|effect| matches!(effect, Effect::EmitHistory(..) | Effect::EmitRows(..))));
        assert_eq!(
            core.observations[&ordinary_id].last_rows,
            ordinary_prior_rows
        );
        assert_eq!(
            core.observations[&ordinary_id].last_evidence,
            ordinary_prior_evidence
        );
        assert_eq!(
            core.history.projection(second_id),
            second_prior,
            "a staged advance on one window must leave every other window byte-identical"
        );

        let rolled_back = core.handle(EngineMsg::RollbackHistoryLoad(id));
        assert_eq!(
            core.history.projection(id),
            prior,
            "rollback must restore the window byte-identical"
        );
        assert_eq!(prior.target_rows, 3, "the window was at its opening target");
        assert!(!rolled_back
            .iter()
            .any(|effect| matches!(effect, Effect::EmitHistory(..) | Effect::EmitRows(..))));

        // The identical declarative request retries cleanly after rollback.
        let retried = core.handle(EngineMsg::RequestRows(id, 6));
        assert!(retried.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        let committed = core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(core.history.projection(id).len(), 6);
        let delivered: Vec<_> = retried
            .iter()
            .chain(committed.iter())
            .filter_map(|effect| match effect {
                Effect::EmitHistory(candidate, batch) if *candidate == id => Some(batch.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].load, WindowLoad::Requesting);
        assert_eq!(delivered[1].load, WindowLoad::Returned { added: 3 });
        assert_eq!(
            delivered[1].evidence[0]
                .shortfall
                .iter()
                .filter(|fact| matches!(fact, ShortfallFact::NoPlannedSource { .. }))
                .count(),
            3,
            "initial, exact tie-second, and older handles all contribute evidence"
        );
        let owned_handles = core.history.projection(id).handle_ids;
        core.handle(EngineMsg::UnsubscribeHistory(id));
        assert!(core.history.is_retired(id));
        for handle in owned_handles {
            assert!(core.resolver.root_atoms(handle).is_empty());
        }

        let reopened = core.handle(EngineMsg::SubscribeHistory(history_query(
            47,
            BTreeSet::from([9]),
        )));
        let active_id = reopened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(active_id, 6));
        let active = core.history.projection(active_id);
        assert!(active.advance_staged);
        core.handle(EngineMsg::UnsubscribeHistory(active_id));
        assert!(core.history.is_retired(active_id));
        for handle in active.handle_ids {
            assert!(core.resolver.root_atoms(handle).is_empty());
        }
    }
}
