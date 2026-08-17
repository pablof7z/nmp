//! #903 capstone: queue inspection is bounded at its public door, while an
//! event id returned by the ordinary query/write path reaches the exact live
//! obligation without scanning unrelated retained receipts.

use nmp::{CancelWriteOutcome, Engine, EngineConfig, ReceiptId};
use nmp_grammar::{EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};
use nostr::{Keys, Kind, Timestamp};

fn parked_note(author: nostr::PublicKey, created_at: u64, content: String) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Event(
            EventBuilder::new(Kind::TextNote)
                .content(content)
                .created_at(Timestamp::from(created_at)),
        ),
        routing: WriteRouting::Auto,
        identity: Identity::Explicit(author),
    }
}

#[test]
fn queue_pages_are_disjoint_and_no_call_can_return_more_than_u8_allows() {
    let engine = Engine::new(EngineConfig::default()).expect("engine builds");
    let author = Keys::generate().public_key();

    for index in 0..300u64 {
        engine
            .publish(parked_note(
                author,
                1_000 + index,
                format!("parked {index}"),
            ))
            .expect("explicit absent signer parks after acceptance");
    }

    let first = engine
        .publish_queue(None, u8::MAX)
        .expect("first bounded page reads");
    assert_eq!(first.len(), usize::from(u8::MAX));
    let cursor = first.last().expect("first page is nonempty").receipt_id;
    let second = engine
        .publish_queue(Some(cursor), u8::MAX)
        .expect("second bounded page reads");
    assert_eq!(second.len(), 45);
    assert!(
        first.last().unwrap().receipt_id < second.first().unwrap().receipt_id,
        "the exclusive receipt-id cursor makes pages stable and disjoint"
    );
    assert!(
        engine.publish_queue(None, 0).unwrap().is_empty(),
        "zero capacity materializes nothing"
    );

    engine.shutdown();
}

#[test]
fn an_event_id_reaches_every_matching_active_receipt_before_and_after_restart() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let path = fixture.path().join("event-obligation-lookup.redb");
    let author = Keys::generate().public_key();

    let (event_id, expected_ids) = {
        let engine = Engine::new(EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        })
        .expect("persistent engine builds");
        let intent = || parked_note(author, 1_000, "same frozen event".to_string());
        let first = engine.publish(intent()).expect("first write is accepted");
        let second = engine.publish(intent()).expect("second write is accepted");
        assert_eq!(
            first.event_id, second.event_id,
            "the event bytes are identical"
        );

        let exact = engine
            .publish_queue_for_event(first.event_id, None, u8::MAX)
            .expect("exact active-obligation lookup reads");
        assert_eq!(
            exact
                .iter()
                .map(|entry| entry.receipt_id)
                .collect::<Vec<_>>(),
            vec![first.id, second.id],
            "the exact door chooses neither receipt and includes no unrelated write"
        );
        let event_id = first.event_id;
        let ids = vec![first.id, second.id];
        engine.shutdown();
        (event_id, ids)
    };

    let reopened = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .expect("the real Redb store reopens");
    let recovered = reopened
        .publish_queue_for_event(event_id, None, u8::MAX)
        .expect("the exact index is rebuilt from durable intents");
    assert_eq!(
        recovered
            .iter()
            .map(|entry| entry.receipt_id)
            .collect::<Vec<ReceiptId>>(),
        expected_ids
    );

    assert_eq!(
        reopened
            .cancel(expected_ids[0])
            .expect("the unsigned obligation cancels durably"),
        CancelWriteOutcome::Cancelled
    );
    let still_active = reopened
        .publish_queue_for_event(event_id, None, u8::MAX)
        .expect("exact lookup remains readable after cancellation");
    assert_eq!(
        still_active
            .iter()
            .map(|entry| entry.receipt_id)
            .collect::<Vec<_>>(),
        vec![expected_ids[1]],
        "exact event lookup reports obligations, not terminal history"
    );
    let retained = reopened
        .publish_queue(None, u8::MAX)
        .expect("the general retained queue remains readable");
    let cancelled = retained
        .iter()
        .find(|entry| entry.receipt_id == expected_ids[0] && entry.is_terminal())
        .expect("cancellation ends the obligation but retains its evidence until removal");
    assert_ne!(
        cancelled.accepted_at,
        Timestamp::from(0u64),
        "terminal inspection projects the receipt's durable acceptance time"
    );

    reopened.shutdown();
}
