use std::time::Duration;

use nmp::{
    Binding, Demand, Durability, Engine, EngineConfig, EventId, Filter, Freshness, Kind, LiveQuery,
    RowDelta, Subscription, Timestamp, WriteIntent, WritePayload, WriteRouting, WriteStatus,
};
use nostr::{EventBuilder, Keys};

use super::*;

fn expect_error<T>(result: Result<T, FfiReactionError>) -> FfiReactionError {
    match result {
        Ok(_) => panic!("operation unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn exact_cache_observation(engine: &Engine, event_id: EventId) -> Subscription {
    let mut demand = Demand::from_filter(Filter {
        ids: Some(Binding::Literal([event_id.to_hex()].into_iter().collect())),
        ..Filter::default()
    });
    demand.freshness = Freshness::CacheOnly;
    let observation = engine.observe(LiveQuery(demand), None).unwrap();
    assert!(
        observation
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .deltas
            .is_empty(),
        "fixture event must not exist before acceptance"
    );
    observation
}

fn accepted_target(engine: &Engine, keys: &Keys) -> nostr::Event {
    let event = EventBuilder::new(Kind::TextNote, "target")
        .custom_created_at(Timestamp::from(42u64))
        .sign_with_keys(keys)
        .unwrap();
    let observation = exact_cache_observation(engine, event.id);
    engine.set_active_account(Some(keys.public_key())).unwrap();
    let receipt = engine
        .publish_tracked(WriteIntent {
            payload: WritePayload::Signed(event.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        })
        .unwrap();
    assert_eq!(
        receipt
            .statuses
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        WriteStatus::Accepted
    );
    let frame = observation
        .recv_timeout(Duration::from_secs(2))
        .expect("canonical update after durable acceptance");
    assert!(
        frame
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event.id)),
        "durable acceptance must make the fixture visible through the canonical query"
    );
    event
}

fn tag_rows(event: &nmp::UnsignedEvent) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect()
}

#[test]
fn ffi_composes_exact_opaque_schema_from_canonical_id() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let target_keys = Keys::generate();
    let target_event = accepted_target(&engine, &target_keys);
    let author = Keys::generate().public_key();
    engine.set_active_account(Some(author)).unwrap();

    let target = reaction_target(&engine, target_event.id.to_hex()).unwrap();
    let draft = reaction_draft(&engine, target, FfiReactionValue::Like).unwrap();
    let event = draft.event_for_test().unwrap();

    assert_eq!(event.kind, Kind::Reaction);
    assert_eq!(event.pubkey, author);
    assert_eq!(event.content, "+");
    assert_eq!(
        tag_rows(&event),
        vec![
            vec![
                "e".to_string(),
                target_event.id.to_hex(),
                String::new(),
                target_keys.public_key().to_hex(),
            ],
            vec!["p".to_string(), target_keys.public_key().to_hex()],
            vec!["k".to_string(), "1".to_string()],
        ]
    );
    engine.shutdown();
}

#[test]
fn ffi_refuses_malformed_unknown_signed_out_and_invalid_value_inputs() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    assert!(matches!(
        reaction_target(&engine, "not-an-event-id".to_string()),
        Err(FfiReactionError::InvalidEventId { .. })
    ));
    let unknown = nmp::EventId::from_slice(&[7; 32]).unwrap();
    assert_eq!(
        expect_error(reaction_target(&engine, unknown.to_hex())),
        FfiReactionError::TargetNotFound {
            event_id: unknown.to_hex()
        }
    );

    let keys = Keys::generate();
    let target_event = accepted_target(&engine, &keys);
    let target = reaction_target(&engine, target_event.id.to_hex()).unwrap();
    engine.set_active_account(None).unwrap();
    assert_eq!(
        expect_error(reaction_draft(
            &engine,
            Arc::clone(&target),
            FfiReactionValue::Like
        )),
        FfiReactionError::NoActiveReactionAuthor
    );
    engine.set_active_account(Some(keys.public_key())).unwrap();
    assert_eq!(
        expect_error(reaction_draft(
            &engine,
            Arc::clone(&target),
            FfiReactionValue::Emoji {
                value: ":missing:".to_string()
            }
        )),
        FfiReactionError::CustomEmojiRequiresMetadata {
            got: ":missing:".to_string()
        }
    );
    engine.shutdown();
    assert_eq!(
        expect_error(reaction_target(&engine, target_event.id.to_hex())),
        FfiReactionError::EngineClosed
    );
}

#[test]
fn every_internal_failure_maps_to_one_reachable_native_variant() {
    let id = nmp::EventId::from_slice(&[3; 32]).unwrap();
    assert_eq!(
        target_error_to_ffi(nmp_nip25::ReactionTargetError::CanonicalLookupUnavailable {
            reason: "closed initial frame".to_string()
        }),
        FfiReactionError::CanonicalLookupUnavailable {
            reason: "closed initial frame".to_string()
        }
    );
    assert_eq!(
        target_error_to_ffi(nmp_nip25::ReactionTargetError::TargetNotVerified { event_id: id }),
        FfiReactionError::TargetNotVerified {
            event_id: id.to_hex()
        }
    );

    assert_eq!(
        value_from_ffi(FfiReactionValue::Emoji {
            value: String::new()
        }),
        Err(FfiReactionError::EmptyEmoji)
    );
    assert_eq!(
        value_from_ffi(FfiReactionValue::Emoji {
            value: "+".to_string()
        }),
        Err(FfiReactionError::StandardValueRequiresTypedVariant {
            got: "+".to_string()
        })
    );
    assert_eq!(
        value_from_ffi(FfiReactionValue::Emoji {
            value: "two words".to_string()
        }),
        Err(FfiReactionError::InvalidEmojiToken {
            got: "two words".to_string()
        })
    );
    assert_eq!(
        value_from_ffi(FfiReactionValue::CustomEmoji {
            shortcode: "bad!".to_string(),
            image_url: "https://cdn.example/x.png".to_string()
        }),
        Err(FfiReactionError::InvalidCustomEmojiShortcode {
            got: "bad!".to_string()
        })
    );
    assert_eq!(
        value_from_ffi(FfiReactionValue::CustomEmoji {
            shortcode: "ok".to_string(),
            image_url: "file:///tmp/x.png".to_string()
        }),
        Err(FfiReactionError::InvalidCustomEmojiUrl {
            got: "file:///tmp/x.png".to_string()
        })
    );
}
