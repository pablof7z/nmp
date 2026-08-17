use nmp::{
    Engine, EngineConfig, EventBuilder, Identity, NotSentReason, ReceiptId, ReceiptResult,
    RelayUrl, SigningState, WriteFact, WriteIntent, WriteOutcome, WritePayload, WriteRouting,
};
use nostr::{Keys, Kind};

fn parked_write() -> (Keys, WriteIntent) {
    let keys = Keys::generate();
    let intent = WriteIntent {
        payload: WritePayload::Event(EventBuilder::new(Kind::TextNote).content("await one answer")),
        routing: WriteRouting::Explicit(vec![
            RelayUrl::parse("wss://receipt-result.invalid").unwrap()
        ]),
        identity: Identity::Explicit(keys.public_key()),
    };
    (keys, intent)
}

fn cancelled() -> ReceiptResult {
    ReceiptResult {
        outcome: WriteOutcome::NotSent(NotSentReason::Cancelled),
        relays: Default::default(),
    }
}

#[test]
fn receipt_result_returns_one_terminal_answer_without_app_reduction() {
    let engine = Engine::new(EngineConfig::default()).unwrap();
    let (_keys, intent) = parked_write();
    let receipt = engine.publish(intent).unwrap();

    assert!(matches!(
        receipt.statuses.recv().unwrap(),
        WriteFact::Signing(SigningState::AwaitingSigner { .. })
    ));

    engine.cancel(receipt.id).unwrap();
    assert_eq!(receipt.result().unwrap(), cancelled());
    engine.shutdown();
}

#[test]
fn restart_reattachment_returns_the_same_terminal_answer_without_cursor_code() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("receipt-result.redb");
    let receipt_id: ReceiptId;

    {
        let engine = Engine::new(EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .unwrap();
        let (_keys, intent) = parked_write();
        receipt_id = engine.publish(intent).unwrap().id;
        engine.shutdown();
    }

    let engine = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .unwrap();
    engine.cancel(receipt_id).unwrap();
    assert_eq!(engine.receipt_result(receipt_id).unwrap(), cancelled());
    engine.shutdown();
}
