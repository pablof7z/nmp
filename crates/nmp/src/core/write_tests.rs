//! Ownership-domain tests moved with the implementation they falsify.

use super::*;

#[cfg(test)]
mod receipt_allocator_tests {
    use super::*;

    use nmp_router::FixtureRoutingFacts;
    use nmp_store::{MemoryStore, RedbStore, RefuseReason};
    use nostr::{Keys, Kind};

    fn frozen_note(author: PublicKey) -> SignedEvent {
        let created_at = Timestamp::from(1_700_000_000);
        let kind = Kind::TextNote;
        let tags = nostr::Tags::new();
        let content = "route need".to_string();
        SignedEvent::new(
            EventId::new(&author, &created_at, &kind, &tags, &content),
            author,
            created_at,
            kind,
            tags,
            content,
            nmp_store::sentinel_signature(),
        )
    }

    #[test]
    fn stale_replaceable_edit_is_refused_into_custody_keeping_both_event_ids() {
        use nmp_store::RelayObserved;
        use nostr::EventBuilder;

        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://source.example").unwrap();
        let base = EventBuilder::new(Kind::ContactList, "base")
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        let concurrent = EventBuilder::new(Kind::ContactList, "concurrent")
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = MemoryStore::new();
        store
            .insert(
                base.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(10u64)),
            )
            .unwrap();
        store
            .insert(
                concurrent.clone(),
                RelayObserved::new(relay, Timestamp::from(20u64)),
            )
            .unwrap();

        let mut core = EngineCore::new(store, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::ReplaceableEdit {
                builder: nmp_grammar::EventBuilder {
                    kind: Kind::ContactList,
                    tags: (vec![]).into_iter().collect(),
                    content: ("my edit").into(),
                    created_at: Some(Timestamp::from(30u64)),
                },
                expected_base: Some(base.id),
            },
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));

        // CUSTODY, not a refused call: the store was working and said no,
        // so the write becomes a permanently-failed queue entry the app can
        // read back. Both event ids survive verbatim -- that pair is what
        // lets an app fetch `actual`, reapply the change and resubmit
        // without ever troubling the user.
        let receipt = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::WriteAccepted(id) => Some(*id),
                _ => None,
            })
            .expect("a store-refused write is still taken into custody");
        let expected = WriteFact::Outcome(WriteOutcome::Refused(
            RefuseReason::ReplaceableBaseChanged {
                expected: Some(base.id),
                actual: Some(concurrent.id),
            },
        ));
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::EmitReceipt(id, status) if *id == receipt && *status == expected
            )),
            "the refusal must name BOTH event ids: {effects:?}"
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::PublishFailed(_))),
            "a stale base is never a refused call: {effects:?}"
        );
        assert!(core.pending.is_empty());
        assert!(core
            .resolver
            .store()
            .recover_publish_queue()
            .expect("recover delivery")
            .is_empty());
    }

    #[test]
    fn last_attempt_correlation_is_issued_once_then_exhaustion_is_stable_and_typed() {
        let mut core = EngineCore::new(MemoryStore::new(), 10);
        core.set_next_attempt_correlation_for_test(Some(u64::MAX));

        assert_eq!(
            core.alloc_attempt_correlation(),
            Ok(AttemptCorrelation(u64::MAX))
        );
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted)
        );
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted),
            "exhaustion remains stable: no wrap, reuse, or fabricated id"
        );
    }

    #[test]
    fn attempt_correlation_exhaustion_precedes_lane_and_pending_mutation() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://correlation-exhausted.example").unwrap();
        let directory =
            FixtureRoutingFacts::new().with_outbound_routes(keys.public_key(), [relay.clone()]);
        let mut core =
            EngineCore::new_with_fixture_routing_facts(MemoryStore::new(), directory, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("correlation boundary").into(),
                created_at: Some(Timestamp::from(93u64)),
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
            .expect("accepted unsigned intent requests signing");
        let intent = core.pending[&receipt].intent_id;
        core.set_next_attempt_correlation_for_test(None);

        let effects = core.handle(EngineMsg::SignerCompleted(
            receipt,
            generation,
            Ok(unsigned.sign_with_keys(&keys).unwrap()),
        ));

        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(core.attempt_correlations.is_empty());
        assert!(core.pending[&receipt].pending_relays.is_empty());
        assert!(core.pending[&receipt].attempt_ordinals.is_empty());
        assert!(core
            .resolver
            .store()
            .recover_attempts(intent)
            .unwrap()
            .is_empty());
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted),
            "the failed call must not revive or wrap the namespace"
        );
    }

    /// Verbatim execution: an explicit route resolves to exactly the relays
    /// the caller named. The directory is deliberately populated with a
    /// DIFFERENT write relay for this very author -- if resolution consulted
    /// it at all, that relay would show up here.
    #[test]
    fn an_explicit_route_resolves_verbatim_and_never_consults_the_directory() {
        let author = Keys::generate().public_key();
        let a = RelayUrl::parse("wss://chosen-a.example").unwrap();
        let b = RelayUrl::parse("wss://chosen-b.example").unwrap();
        let unrelated = RelayUrl::parse("wss://unrelated.example").unwrap();
        let directory = FixtureRoutingFacts::new().with_outbound_routes(author, [unrelated]);
        let core = EngineCore::new_with_fixture_routing_facts(MemoryStore::new(), directory, 10);
        let route = WriteRouting::Explicit(vec![b.clone(), a.clone()]);

        let created_at = Timestamp::from(1_700_000_000);
        let kind = nostr::Kind::TextNote;
        let tags = nostr::Tags::new();
        let content = "explicit route".to_string();
        let frozen = SignedEvent::new(
            EventId::new(&author, &created_at, &kind, &tags, &content),
            author,
            created_at,
            kind,
            tags,
            content,
            nmp_store::sentinel_signature(),
        );

        let answer = core.resolve_routes(&route, &frozen);
        assert_eq!(
            answer.relays,
            BTreeSet::from([a, b]),
            "an explicit route executes only the caller's set and never unions a directory fact"
        );
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "verbatim execution reads nothing, so it can never learn anything later: {answer:?}"
        );
    }

    #[test]
    fn a_zero_destination_answer_separates_still_looking_from_finished_looking() {
        // The whole of #1236, as a type rather than a string. `complete` is a
        // statement about KNOWLEDGE EXHAUSTION, never about delivery, and a
        // zero-destination answer is where that earns its keep: an app must
        // show "determining destinations" for one and "nowhere to publish"
        // for the other, and collapsing them makes both unactionable.
        let author = Keys::generate().public_key();
        let event = frozen_note(author);

        // Still looking: nobody has finished the lookup, so nothing has been
        // learned yet and NOTHING may expire this.
        let core = EngineCore::new_with_fixture_routing_facts(
            MemoryStore::new(),
            FixtureRoutingFacts::new(),
            10,
        );
        let unknown = core.resolve_routes(&WriteRouting::Auto, &event);
        assert!(
            unknown.relays.is_empty() && !unknown.complete,
            "an unsettled author must park, not terminate: {unknown:?}"
        );
        assert_eq!(
            unknown.author_route_needs,
            BTreeSet::from([author]),
            "a parked write keeps its contributor declared, because a later positive replacement is the only unpark signal"
        );

        // Finished looking, twice over. A settled-but-empty list and a
        // settled absence are both answers, and an answer that names nobody
        // means there is nowhere to publish.
        for (label, detail_label, facts) in [
            (
                "Present empty",
                "Present outbound",
                FixtureRoutingFacts::new().with_author_routes(author, [], []),
            ),
            (
                "Absent",
                "Absent",
                FixtureRoutingFacts::new().with_author_absent(author),
            ),
        ] {
            let core = EngineCore::new_with_fixture_routing_facts(MemoryStore::new(), facts, 10);
            let answer = core.resolve_routes(&WriteRouting::Auto, &event);

            assert!(
                answer.relays.is_empty() && answer.complete,
                "{label} with no operator route is knowledge EXHAUSTED, so the write has nowhere to go rather than something to wait for: {answer:?}"
            );
            assert!(
                answer.author_route_needs.is_empty(),
                "{label} has nothing left to look up, so it must not hold a provider open on a question already answered: {answer:?}"
            );
            assert!(
                answer
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains(detail_label)),
                "the answer must still distinguish {label} truthfully: {answer:?}"
            );
        }
    }

    #[test]
    fn a_settled_zero_route_author_retires_when_an_operator_route_exists() {
        let author = Keys::generate().public_key();
        let app = RelayUrl::parse("wss://app.example").unwrap();
        let facts = FixtureRoutingFacts::new()
            .with_author_absent(author)
            .with_operator_app([app.clone()]);
        let core = EngineCore::new_with_fixture_routing_facts(MemoryStore::new(), facts, 10);

        let answer = core.resolve_routes(&WriteRouting::Auto, &frozen_note(author));

        assert_eq!(answer.relays, BTreeSet::from([app]));
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "a settled contributor with an actual destination has no remaining provider need: {answer:?}"
        );
    }

    /// The durable value is a STRATEGY. `Auto` journals a bare label with no
    /// relay in it, so replaying it after a crash re-resolves against
    /// whatever the engine knows then rather than against a stale answer;
    /// `Explicit` journals exactly the relays it was given, in order.
    #[test]
    fn routing_snapshots_round_trip_the_strategy_not_a_resolved_relay_set() {
        let a = RelayUrl::parse("wss://chosen-a.example").unwrap();
        let b = RelayUrl::parse("wss://chosen-b.example").unwrap();

        let auto = EngineCore::<MemoryStore>::routing_snapshot(&WriteRouting::Auto);
        assert_eq!(auto, "auto", "Auto stores a label, never a relay set");
        assert!(matches!(
            EngineCore::<MemoryStore>::parse_routing_snapshot(&auto),
            Some(WriteRouting::Auto)
        ));

        let route = WriteRouting::Explicit(vec![b.clone(), a.clone()]);
        let snapshot = EngineCore::<MemoryStore>::routing_snapshot(&route);
        let restored = EngineCore::<MemoryStore>::parse_routing_snapshot(&snapshot)
            .expect("a valid explicit snapshot must remain readable");
        let WriteRouting::Explicit(relays) = restored else {
            panic!("snapshot restored the wrong routing variant")
        };
        assert_eq!(relays, vec![b, a]);
    }

    #[test]
    fn boot_redeclares_recovered_auto_route_needs_to_protocol_assembly() {
        let keys = Keys::generate();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("route-needs.redb");

        {
            let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
            core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
            let accepted = core.handle(EngineMsg::Publish(WriteIntent {
                payload: WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::TextNote,
                    tags: (vec![]).into_iter().collect(),
                    content: ("recover my route need").into(),
                    created_at: Some(Timestamp::from(100u64)),
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
                .expect("accepted auto write requests signing");
            core.handle(EngineMsg::SignerCompleted(
                receipt,
                generation,
                Ok(unsigned.sign_with_keys(&keys).unwrap()),
            ));
            assert_eq!(
                core.author_route_needs(),
                BTreeSet::from([keys.public_key()])
            );
        }

        let mut recovered = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
        let effects = recovered.recover_on_boot();
        let replayed = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::AuthorRouteNeedsChanged(needs) => Some(needs.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            replayed,
            vec![BTreeSet::from([keys.public_key()])],
            "boot must publish the exact stateless route-need set rebuilt from durable intents"
        );
    }
}
