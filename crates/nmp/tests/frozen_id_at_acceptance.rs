//! Acceptance hands back the event id it just froze (#1314).
//!
//! The value was always decided by the acceptance transaction and always
//! dropped on the floor, so the only way an app could learn its own write's
//! identity was the old zero-argument `Engine::publish_queue()` — which
//! materialized EVERY retained receipt. #903 has since deleted that spelling
//! in favor of bounded pages plus direct event-id lookup.
//!
//! What these falsify is not the ergonomics but the VALUE: the id `publish`
//! returns must be the post-restamp one in every case, must be what the queue
//! reports for the same receipt, and must be what the signature eventually
//! lands on. A door that returned a pre-restamp guess for a replaceable edit
//! would be worse than no door.

use std::time::Duration;

use nmp::{Engine, EngineConfig, SigningState, WriteFact};
use nmp_grammar::{Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_store::{RedbStore, RelayObserved};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, RelayUrl, Tags, Timestamp};

/// Far enough ahead that no wall clock in this test can reach it, so the
/// store's monotonic stamp is provably the thing that decided the id.
const WINNER_STAMP: u64 = 4_000_000_000;

fn unreachable_relay() -> RelayUrl {
    RelayUrl::parse("wss://frozen-id-at-acceptance.invalid").unwrap()
}

fn engine_over(path: &std::path::Path, keys: &Keys) -> Engine {
    let engine = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .expect("engine opens over the store");
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), true)
        .expect("register the provider and select its account");
    engine
}

fn note(keys: &Keys, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .sign_with_keys(keys)
        .expect("sign fixture")
}

fn durable(event: Event) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Signed(event),
        routing: WriteRouting::Explicit(vec![unreachable_relay()]),
        identity: Identity::Active,
        correlation: None,
    }
}

/// The plain case, at the door an app actually reaches: what `publish`
/// answers is what the queue answers for the same receipt.
#[test]
fn acceptance_answers_the_same_event_id_the_queue_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("plain.redb");
    let keys = Keys::generate();
    let engine = engine_over(&path, &keys);

    let event = note(&keys, "the write an app just made");
    let receipt = engine.publish(durable(event.clone())).expect("accepted");

    assert_eq!(
        receipt.event_id, event.id,
        "a Signed payload's identity is already in its bytes"
    );
    let entry = engine
        .publish_queue(None, u8::MAX)
        .expect("the queue reads back")
        .into_iter()
        .find(|entry| entry.receipt_id == receipt.id)
        .expect("the accepted write is retained");
    assert_eq!(
        entry.event_id, receipt.event_id,
        "acceptance and the queue must not be two authorities on one id"
    );
    engine.shutdown();
}

/// The case a pre-restamp answer would get wrong, and the reason this cannot
/// be a doc caveat: a replaceable edit that leaves `created_at` unsaid has its
/// stamp moved forward INSIDE the acceptance transaction, against the row that
/// transaction is CAS-ing, which re-derives the id. `publish` must report the
/// value that survived that, not the one the reducer froze on the way in.
#[test]
fn a_restamped_replaceable_edit_reports_its_post_restamp_id() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("restamped.redb");
    let keys = Keys::generate();

    // A local winner already sitting at this author's replaceable coordinate,
    // stamped beyond any clock this test can observe.
    let winner = EventBuilder::new(Kind::ContactList, "winner")
        .custom_created_at(Timestamp::from(WINNER_STAMP))
        .sign_with_keys(&keys)
        .expect("sign the winner");
    {
        let mut store = RedbStore::open(&path).expect("open store");
        store
            .insert(
                winner.clone(),
                RelayObserved::new(unreachable_relay(), Timestamp::from(WINNER_STAMP)),
            )
            .expect("seed the winner");
    }

    let engine = engine_over(&path, &keys);
    let content = "edited without stating a created_at";
    let receipt = engine
        .publish(WriteIntent {
            payload: WritePayload::ReplaceableEdit {
                builder: nmp_grammar::EventBuilder::new(Kind::ContactList).content(content),
                expected_base: Some(winner.id),
            },
            routing: WriteRouting::Explicit(vec![unreachable_relay()]),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("the edit wins its compare-and-swap");

    let restamped = EventId::new(
        &keys.public_key(),
        &Timestamp::from(WINNER_STAMP + 1),
        &Kind::ContactList,
        &Tags::new(),
        content,
    );
    assert_eq!(
        receipt.event_id, restamped,
        "acceptance must report the id the store's monotonic stamp produced"
    );
    let at_wall_clock = EventId::new(
        &keys.public_key(),
        &Timestamp::now(),
        &Kind::ContactList,
        &Tags::new(),
        content,
    );
    assert_ne!(
        receipt.event_id, at_wall_clock,
        "the fixture must actually exercise a restamp, or it proves nothing"
    );
    assert_eq!(
        engine
            .publish_queue(None, u8::MAX)
            .expect("the queue reads back")
            .into_iter()
            .find(|entry| entry.receipt_id == receipt.id)
            .expect("the edit is retained")
            .event_id,
        receipt.event_id,
        "one write, one identity, whichever door reports it"
    );
    engine.shutdown();
}

/// A NIP-01 id never depends on `sig`, so the id acceptance froze is the id
/// the signature lands on. That is what makes it answerable at acceptance
/// rather than at signing time.
#[test]
fn the_frozen_id_is_the_id_the_signature_lands_on() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("signed.redb");
    let keys = Keys::generate();
    let engine = engine_over(&path, &keys);

    let receipt = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(
                nmp_grammar::EventBuilder::new(Kind::TextNote).content("NMP signs this one"),
            ),
            routing: WriteRouting::Explicit(vec![unreachable_relay()]),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("accepted");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let signed = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the registered signer never answered"
        );
        match receipt.statuses.recv_timeout(Duration::from_millis(250)) {
            Ok(WriteFact::Signing(SigningState::Signed { event_id })) => break event_id,
            Ok(_) => continue,
            Err(_) => continue,
        }
    };
    assert_eq!(
        signed, receipt.event_id,
        "signing may not move an id acceptance already answered with"
    );
    engine.shutdown();
}

/// #591's correlation token makes a repeated publish resolve to the OBLIGATION
/// it already accepted, discarding the re-composed draft entirely. The
/// acceptance answer must follow the obligation, so the id reported is the
/// retained one and never the discarded draft's.
#[test]
fn a_correlation_replay_answers_with_the_retained_obligation_id() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("correlation.redb");
    let keys = Keys::generate();
    let engine = engine_over(&path, &keys);

    let first = note(&keys, "the write that was accepted");
    let second = note(&keys, "a differently composed retry of the same intent");
    assert_ne!(first.id, second.id, "the fixture needs two distinct bodies");

    let token = nmp_grammar::CorrelationToken::try_from("frozen-id-at-acceptance")
        .expect("a non-empty bounded token");
    let accepted = engine
        .publish(WriteIntent {
            correlation: Some(token.clone()),
            ..durable(first.clone())
        })
        .expect("accepted");
    let replayed = engine
        .publish(WriteIntent {
            correlation: Some(token),
            ..durable(second)
        })
        .expect("the token resolves to the existing obligation");

    assert_eq!(replayed.id, accepted.id, "one token, one receipt");
    assert_eq!(
        replayed.event_id, first.id,
        "the replay must report the obligation's identity, not the draft it threw away"
    );
    engine.shutdown();
}

/// The cost shape #1314 is about, stated in the only terms the code makes
/// checkable: the other route to this fact answers with the whole retained
/// receipt set, and nothing bounds that set (#46). Acceptance answers with one
/// value it already held, so what it costs does not move when the store grows.
#[test]
fn the_answer_does_not_grow_with_the_retained_receipt_set() {
    const RETAINED: usize = 64;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("retained.redb");
    let keys = Keys::generate();
    let engine = engine_over(&path, &keys);

    for i in 0..RETAINED {
        let event = note(&keys, &format!("retained {i}"));
        let receipt = engine.publish(durable(event.clone())).expect("accepted");
        assert_eq!(
            receipt.event_id, event.id,
            "every acceptance answers about ITS write, however many came before"
        );
    }

    let latest = note(&keys, "the write being asked about");
    let receipt = engine.publish(durable(latest.clone())).expect("accepted");
    assert_eq!(
        receipt.event_id, latest.id,
        "the answer is the same whether the store holds one receipt or many"
    );
    assert_eq!(
        engine
            .publish_queue(None, u8::MAX)
            .expect("the queue reads back")
            .len(),
        RETAINED + 1,
        "the only other route to this one fact materializes every retained \
         receipt, which is what makes it the wrong door for a write path"
    );
    engine.shutdown();
}

/// The reproducible cost curve behind #1314, run on demand:
///
/// ```text
/// cargo test --release -p nmp --test frozen_id_at_acceptance \
///     measure_frozen_id_read_cost -- --ignored --nocapture
/// ```
///
/// It publishes `n` durable writes owed to a relay nothing connects to, and
/// reads each one's frozen id back through #903's exact event-id lookup.
/// `#[ignore]`d because it builds a real on-disk store with thousands of intents and reports
/// wall-clock numbers, neither of which belongs in the ordinary suite.
#[test]
#[ignore = "manual cost qualification"]
fn measure_frozen_id_read_cost() {
    for count in [250usize, 500, 1_000, 2_000, 4_000] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("measure.redb");
        let keys = Keys::generate();
        let engine = engine_over(&path, &keys);

        let mut accepting = Duration::ZERO;
        let mut scanning = Duration::ZERO;
        for i in 0..count {
            let intent = durable(note(&keys, &format!("measured {i}")));
            let started = std::time::Instant::now();
            let receipt = engine.publish(intent).expect("accepted");
            accepting += started.elapsed();

            let started = std::time::Instant::now();
            let scanned = engine
                .publish_queue_for_event(receipt.event_id, None, u8::MAX)
                .expect("the exact active-obligation lookup reads back")
                .into_iter()
                .find(|entry| entry.receipt_id == receipt.id)
                .map(|entry| entry.event_id);
            scanning += started.elapsed();
            assert_eq!(scanned, Some(receipt.event_id));
        }
        println!(
            "n={count}  accepting={accepting:?} ({:?}/write)  \
             scanning={scanning:?} ({:?}/lookup)",
            accepting / count as u32,
            scanning / count as u32
        );
        engine.shutdown();
    }
}
