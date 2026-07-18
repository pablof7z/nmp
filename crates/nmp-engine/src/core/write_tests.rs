//! Ownership-domain tests moved with the implementation they falsify.

use super::*;

#[cfg(test)]
mod receipt_allocator_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<WriteStatus>>>);

    impl ReceiptSink for Sink {
        fn on_status(&self, status: WriteStatus) {
            self.0.lock().unwrap().push(status);
        }
    }

    fn rejected_intent(keys: &Keys, created_at: u64) -> WriteIntent {
        WriteIntent {
            payload: WritePayload::Unsigned(UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(created_at),
                Kind::TextNote,
                vec![],
                "no active account",
            )),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
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
        let sink = Sink::default();
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::UnsignedReplaceableEdit {
                    unsigned: UnsignedEvent::new(
                        keys.public_key(),
                        Timestamp::from(30u64),
                        Kind::ContactList,
                        vec![],
                        "my edit",
                    ),
                    expected_base: Some(base.id),
                },
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            },
            Box::new(sink.clone()),
        ));

        let expected = WriteStatus::ReplaceableConflict {
            expected: Some(base.id),
            actual: Some(concurrent.id),
        };
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(_, status) if *status == expected)));
        assert!(core.pending.is_empty());
        assert!(core.resolver.store().recover_outbox().is_empty());
    }

    #[test]
    fn last_upper_half_id_is_issued_once_then_exhaustion_is_stable_and_typed() {
        const FIRST_UNACCEPTED_ID: u64 = 1u64 << 63;
        let keys = Keys::generate();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.set_next_unaccepted_receipt_for_test(Some(FIRST_UNACCEPTED_ID));

        let last_sink = Sink::default();
        let last = core.handle(EngineMsg::Publish(
            rejected_intent(&keys, 1),
            Box::new(last_sink.clone()),
        ));
        assert!(last.iter().any(|effect| {
            matches!(
                effect,
                Effect::EmitReceipt(ReceiptId(id), WriteStatus::Failed(_))
                    if *id == FIRST_UNACCEPTED_ID
            )
        }));
        assert!(matches!(
            last_sink.0.lock().unwrap().as_slice(),
            [WriteStatus::Failed(_)]
        ));

        for created_at in [2, 3] {
            let exhausted_sink = Sink::default();
            let exhausted = core.handle(EngineMsg::Publish(
                rejected_intent(&keys, created_at),
                Box::new(exhausted_sink.clone()),
            ));
            assert!(matches!(
                exhausted.as_slice(),
                [Effect::PublishFailed(
                    PublishError::ReceiptCorrelationIdExhausted
                )]
            ));
            assert!(exhausted_sink.0.lock().unwrap().is_empty());
            assert!(!exhausted
                .iter()
                .any(|effect| matches!(effect, Effect::EmitReceipt(..))));
        }

        assert_eq!(FIRST_UNACCEPTED_ID - 1, u64::MAX >> 1);
        assert!(core.pending.is_empty());
        assert!(core.resolver.store().recover_outbox().is_empty());
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
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(93u64),
                    Kind::TextNote,
                    vec![],
                    "correlation boundary",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            },
            Box::new(Sink::default()),
        ));
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
}
