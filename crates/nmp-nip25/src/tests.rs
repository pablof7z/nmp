use std::time::Duration;

use nmp::{
    Binding, Demand, Durability, Engine, EngineConfig, EventId, Filter, Freshness, Kind, LiveQuery,
    RelayUrl, Row, RowDelta, Subscription, Tag, Timestamp, UnsignedEvent, WriteIntent,
    WritePayload, WriteRouting, WriteStatus,
};
use nostr::{Event, EventBuilder, Keys};

use crate::draft::compose_reaction_at;
use crate::{
    reaction_draft, reaction_target, ReactionDraftError, ReactionTarget, ReactionTargetError,
    ReactionValue, ReactionValueError,
};

fn signed_event(keys: &Keys, kind: Kind, created_at: u64, tags: Vec<Tag>) -> Event {
    EventBuilder::new(kind, "target")
        .tags(tags)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("fixture signs")
}

fn rows(event: &UnsignedEvent) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect()
}

fn canonical_target(event: Event, sources: impl IntoIterator<Item = RelayUrl>) -> ReactionTarget {
    ReactionTarget::from_canonical_row(Row {
        event,
        sources: sources.into_iter().collect(),
    })
    .expect("signed fixture qualifies")
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

fn assert_observation_added(observation: &Subscription, event_id: EventId) {
    let frame = observation
        .recv_timeout(Duration::from_secs(2))
        .expect("canonical update after durable acceptance");
    assert!(
        frame
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event_id)),
        "durable acceptance must make the fixture visible through the canonical query"
    );
}

#[test]
fn exact_event_target_tags_include_e_p_k_and_deterministic_hint() {
    let target_keys = Keys::generate();
    let target_event = signed_event(&target_keys, Kind::TextNote, 42, vec![]);
    let target_id = target_event.id;
    let relay_a = RelayUrl::parse("wss://a.example").unwrap();
    let relay_z = RelayUrl::parse("wss://z.example").unwrap();
    let target = canonical_target(target_event, [relay_z, relay_a.clone()]);
    let author = Keys::generate().public_key();

    let draft = compose_reaction_at(
        &target,
        ReactionValue::like(),
        author,
        Timestamp::from(1_700_000_000u64),
    );
    assert_eq!(draft.event().kind, Kind::Reaction);
    assert_eq!(draft.event().content, "+");
    assert_eq!(
        rows(draft.event()),
        vec![
            vec![
                "e".to_string(),
                target_id.to_hex(),
                relay_a.to_string(),
                target_keys.public_key().to_hex(),
            ],
            vec![
                "p".to_string(),
                target_keys.public_key().to_hex(),
                relay_a.to_string(),
            ],
            vec!["k".to_string(), "1".to_string()],
        ]
    );
}

#[test]
fn addressable_target_keeps_mandatory_e_then_a_p_k_with_exact_hints() {
    let target_keys = Keys::generate();
    let target_event = signed_event(
        &target_keys,
        Kind::from(30_023u16),
        42,
        vec![Tag::parse(["d", "article"]).unwrap()],
    );
    let target_id = target_event.id;
    let relay = RelayUrl::parse("wss://articles.example").unwrap();
    let target = canonical_target(target_event, [relay.clone()]);
    let draft = compose_reaction_at(
        &target,
        ReactionValue::emoji("⭐").unwrap(),
        Keys::generate().public_key(),
        Timestamp::from(1_700_000_000u64),
    );
    let coordinate = format!("30023:{}:article", target_keys.public_key().to_hex());
    assert_eq!(
        rows(draft.event()),
        vec![
            vec![
                "e".to_string(),
                target_id.to_hex(),
                relay.to_string(),
                target_keys.public_key().to_hex(),
            ],
            vec!["a".to_string(), coordinate, relay.to_string()],
            vec![
                "p".to_string(),
                target_keys.public_key().to_hex(),
                relay.to_string(),
            ],
            vec!["k".to_string(), "30023".to_string()],
        ]
    );
}

#[test]
fn custom_emoji_is_exactly_one_matching_body_and_tag() {
    let target_keys = Keys::generate();
    let target = canonical_target(signed_event(&target_keys, Kind::TextNote, 42, vec![]), []);
    let draft = compose_reaction_at(
        &target,
        ReactionValue::custom_emoji("soap_box-2", "https://cdn.example/emoji/soapbox.png").unwrap(),
        Keys::generate().public_key(),
        Timestamp::from(1_700_000_000u64),
    );
    assert_eq!(draft.event().content, ":soap_box-2:");
    assert_eq!(
        rows(draft.event()).last().unwrap(),
        &vec![
            "emoji".to_string(),
            "soap_box-2".to_string(),
            "https://cdn.example/emoji/soapbox.png".to_string(),
        ]
    );
    assert_eq!(
        rows(draft.event())
            .iter()
            .filter(|row| row.first().map(String::as_str) == Some("emoji"))
            .count(),
        1
    );
}

#[test]
fn malformed_reaction_values_fail_before_draft_composition() {
    assert_eq!(
        ReactionValue::emoji(""),
        Err(ReactionValueError::EmptyEmoji)
    );
    assert!(matches!(
        ReactionValue::emoji("+"),
        Err(ReactionValueError::StandardValueRequiresTypedVariant { .. })
    ));
    assert!(matches!(
        ReactionValue::emoji(":soapbox:"),
        Err(ReactionValueError::CustomEmojiRequiresMetadata { .. })
    ));
    assert!(matches!(
        ReactionValue::emoji("two words"),
        Err(ReactionValueError::InvalidEmojiToken { .. })
    ));
    assert!(matches!(
        ReactionValue::custom_emoji("bad!", "https://cdn.example/x.png"),
        Err(ReactionValueError::InvalidCustomEmojiShortcode { .. })
    ));
    assert!(matches!(
        ReactionValue::custom_emoji("ok", "file:///tmp/x.png"),
        Err(ReactionValueError::InvalidCustomEmojiUrl { .. })
    ));
}

fn accepted_signed_target(engine: &Engine, target_keys: &Keys) -> Event {
    let event = signed_event(target_keys, Kind::TextNote, 42, vec![]);
    let observation = exact_cache_observation(engine, event.id);
    engine
        .set_active_account(Some(target_keys.public_key()))
        .unwrap();
    let receipt = engine
        .publish_tracked(WriteIntent {
            payload: WritePayload::Signed(event.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        })
        .expect("signed target acceptance");
    assert_eq!(
        receipt
            .statuses
            .recv_timeout(Duration::from_secs(2))
            .expect("accepted status"),
        WriteStatus::Accepted
    );
    assert_observation_added(&observation, event.id);
    event
}

#[test]
fn canonical_lookup_refuses_unknown_and_unverified_pending_rows() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let unknown = EventId::from_slice(&[9; 32]).unwrap();
    assert_eq!(
        reaction_target(&engine, unknown),
        Err(ReactionTargetError::TargetNotFound { event_id: unknown })
    );

    let keys = Keys::generate();
    engine.set_active_account(Some(keys.public_key())).unwrap();
    let mut pending = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(42u64),
        Kind::TextNote,
        vec![],
        "unsigned target".to_string(),
    );
    let pending_id = pending.id();
    let observation = exact_cache_observation(&engine, pending_id);
    let receipt = engine
        .publish_tracked(WriteIntent {
            payload: WritePayload::Unsigned(pending),
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
    assert_observation_added(&observation, pending_id);
    assert_eq!(
        reaction_target(&engine, pending_id),
        Err(ReactionTargetError::TargetNotVerified {
            event_id: pending_id
        })
    );
    engine.shutdown();
}

#[test]
fn account_change_reroots_author_and_signed_out_refuses() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let target_keys = Keys::generate();
    let target_event = accepted_signed_target(&engine, &target_keys);
    let target = reaction_target(&engine, target_event.id).unwrap();

    let later_account = Keys::generate().public_key();
    engine.set_active_account(Some(later_account)).unwrap();
    let draft = reaction_draft(&engine, &target, ReactionValue::dislike()).unwrap();
    assert_eq!(draft.event().pubkey, later_account);
    assert_eq!(draft.event().content, "-");

    engine.set_active_account(None).unwrap();
    assert_eq!(
        reaction_draft(&engine, &target, ReactionValue::like()),
        Err(ReactionDraftError::NoActiveReactionAuthor)
    );
    engine.shutdown();
}
