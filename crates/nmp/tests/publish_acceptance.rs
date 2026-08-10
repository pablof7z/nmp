use nmp::{Engine, EngineConfig, EngineError};
use nmp_grammar::{Identity, WriteIntent, WritePayload, WriteRouting};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

/// An event that is already expired is not a failed delivery. NMP never
/// accepts custody for it, so the public call refuses immediately and no
/// receipt or body survives in the app's publish queue.
#[test]
fn already_expired_publish_is_refused_before_receipt_custody() {
    let engine = Engine::new(EngineConfig::default()).expect("engine must build");
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Metadata, "expired")
        .tag(Tag::expiration(Timestamp::from(1u64)))
        .sign_with_keys(&keys)
        .expect("test fixture must sign cleanly");

    let refused = engine.publish(WriteIntent {
        payload: WritePayload::Signed(event),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    });
    match refused {
        Err(error) => assert_eq!(
            error,
            EngineError::PublishRefused {
                reason: "the event was already expired at acceptance".to_string(),
            }
        ),
        Ok(_) => panic!("an already-expired event must be refused"),
    }
    assert!(
        engine
            .publish_queue()
            .expect("publish queue inspection must succeed")
            .is_empty(),
        "pre-custody expiry must retain neither a body nor a receipt"
    );

    engine.shutdown();
}
