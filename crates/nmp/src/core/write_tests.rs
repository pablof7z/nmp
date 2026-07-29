//! Ownership-domain tests moved with the implementation they falsify.

use super::*;

#[cfg(test)]
mod receipt_allocator_tests {
    use super::*;

    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind};

    fn rejected_intent(created_at: u64) -> WriteIntent {
        WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("no active account").into(),
                created_at: Some(Timestamp::from(created_at)),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity_override: None,
            correlation: None,
        }
    }

    #[test]
    fn stale_replaceable_edit_surfaces_a_typed_conflict_before_acceptance() {
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

        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
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
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity_override: None,
            correlation: None,
        }));

        let expected = WriteStatus::ReplaceableConflict {
            expected: Some(base.id),
            actual: Some(concurrent.id),
        };
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(_, status) if *status == expected)));
        assert!(core.pending.is_empty());
        assert!(core
            .resolver
            .store()
            .recover_outbox()
            .expect("recover outbox")
            .is_empty());
    }

    #[test]
    fn last_upper_half_id_is_issued_once_then_exhaustion_is_stable_and_typed() {
        const FIRST_UNACCEPTED_ID: u64 = 1u64 << 63;
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.set_next_unaccepted_receipt_for_test(Some(FIRST_UNACCEPTED_ID));

        let last = core.handle(EngineMsg::Publish(rejected_intent(1)));
        assert!(last.iter().any(|effect| {
            matches!(
                effect,
                Effect::EmitReceipt(ReceiptId(id), WriteStatus::Failed(_))
                    if *id == FIRST_UNACCEPTED_ID
            )
        }));
        for created_at in [2, 3] {
            let exhausted = core.handle(EngineMsg::Publish(rejected_intent(created_at)));
            assert!(matches!(
                exhausted.as_slice(),
                [Effect::PublishFailed(
                    PublishError::ReceiptCorrelationIdExhausted
                )]
            ));
            assert!(!exhausted
                .iter()
                .any(|effect| matches!(effect, Effect::EmitReceipt(..))));
        }

        assert_eq!(FIRST_UNACCEPTED_ID - 1, u64::MAX >> 1);
        assert!(core.pending.is_empty());
        assert!(core
            .resolver
            .store()
            .recover_outbox()
            .expect("recover outbox")
            .is_empty());
    }

    #[test]
    fn last_attempt_correlation_is_issued_once_then_exhaustion_is_stable_and_typed() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
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
            FixtureDirectory::new().with_write(keys.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("correlation boundary").into(),
                created_at: Some(Timestamp::from(93u64)),
            }),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity_override: None,
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
        let intent = core.pending[&receipt].intent_id.unwrap();
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
        let directory = FixtureDirectory::new().with_write(author.to_hex(), [unrelated]);
        let core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
        let route = WriteRouting::Explicit(vec![b.clone(), a.clone()]);

        assert_eq!(
            core.resolve_routes(&route, &author.to_hex()).unwrap(),
            BTreeSet::from([a, b]),
            "an explicit route executes only the caller's set and never unions a directory fact"
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
}
