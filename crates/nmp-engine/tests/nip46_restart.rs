//! Issue #6 acceptance proof: the durable write obligation outlives the
//! process that accepted it, then a real NIP-46 session reattaches and drives
//! the ordinary promotion/publication path to a relay ACK.

use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nmp_engine::core::RelayAdmissionPolicy;
use nmp_engine::outbox::WriteStatus;
use nmp_engine::runtime::{EngineThread, ReceiptReattachment};
use nmp_grammar::{Durability, WriteIntent, WritePayload, WriteRouting};
use nmp_router::FixtureDirectory;
use nmp_signer::Nip46Signer;
use nmp_store::{EventStore, RedbStore, RelayObserved, SigState};
use nmp_transport::PoolConfig;
use nostr::nips::nip44;
use nostr::{
    Event, EventBuilder, EventId, JsonUtil, Keys, Kind, PublicKey, RelayUrl, Tag, Timestamp,
    UnsignedEvent,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tungstenite::Message;

#[derive(Deserialize)]
struct SignBody {
    kind: u16,
    created_at: u64,
    tags: Vec<Vec<String>>,
    content: String,
}

fn response_event(remote: &Keys, client: PublicKey, id: &str, result: String) -> Event {
    let plaintext = json!({ "id": id, "result": result, "error": null }).to_string();
    let ciphertext = nip44::encrypt(
        remote.secret_key(),
        &client,
        plaintext,
        nip44::Version::default(),
    )
    .unwrap();
    EventBuilder::new(Kind::NostrConnect, ciphertext)
        .tag(Tag::public_key(client))
        .sign_with_keys(remote)
        .unwrap()
}

fn spawn_signer_relay(mutate_sign_event: bool) -> (RelayUrl, Keys, Keys) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = RelayUrl::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
    let remote = Keys::generate();
    let user = Keys::generate();
    let remote_thread = remote.clone();
    let user_thread = user.clone();

    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        let mut subscription_id = None;
        while let Ok(Message::Text(text)) = socket.read() {
            let frame: Value = serde_json::from_str(text.as_ref()).unwrap();
            let parts = frame.as_array().unwrap();
            match parts.first().and_then(Value::as_str) {
                Some("REQ") => {
                    subscription_id = parts.get(1).and_then(Value::as_str).map(str::to_string);
                }
                Some("EVENT") => {
                    let event = Event::from_json(parts[1].to_string()).unwrap();
                    let plaintext = nip44::decrypt(
                        remote_thread.secret_key(),
                        &event.pubkey,
                        event.content.as_bytes(),
                    )
                    .unwrap();
                    let request: Value = serde_json::from_str(&plaintext).unwrap();
                    let id = request["id"].as_str().unwrap();
                    let method = request["method"].as_str().unwrap();
                    let params = request["params"].as_array().unwrap();
                    let result = match method {
                        "connect" => "ack".to_string(),
                        "get_public_key" => user_thread.public_key().to_hex(),
                        "switch_relays" => "null".to_string(),
                        "sign_event" => {
                            let body: SignBody =
                                serde_json::from_str(params[0].as_str().unwrap()).unwrap();
                            let tags = body
                                .tags
                                .iter()
                                .map(Tag::parse)
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap();
                            let content = if mutate_sign_event {
                                "mutated by remote signer".to_string()
                            } else {
                                body.content
                            };
                            UnsignedEvent::new(
                                user_thread.public_key(),
                                Timestamp::from(body.created_at),
                                Kind::from_u16(body.kind),
                                tags,
                                content,
                            )
                            .sign_with_keys(&user_thread)
                            .unwrap()
                            .as_json()
                        }
                        other => panic!("unexpected NIP-46 method {other}"),
                    };
                    let response = response_event(&remote_thread, event.pubkey, id, result);
                    let frame = json!([
                        "EVENT",
                        subscription_id.as_deref().expect("REQ before EVENT"),
                        response
                    ])
                    .to_string();
                    socket.send(Message::Text(frame.into())).unwrap();
                }
                _ => {}
            }
        }
    });
    (relay, remote, user)
}

fn spawn_write_relay() -> (RelayUrl, mpsc::Receiver<Event>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = RelayUrl::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
    let (event_tx, event_rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        while let Ok(Message::Text(text)) = socket.read() {
            let frame: Value = serde_json::from_str(text.as_ref()).unwrap();
            let parts = frame.as_array().unwrap();
            if parts.first().and_then(Value::as_str) != Some("EVENT") {
                continue;
            }
            let event = Event::from_json(parts[1].to_string()).unwrap();
            event.verify().unwrap();
            let event_id = event.id;
            event_tx.send(event).unwrap();
            socket
                .send(Message::Text(
                    json!(["OK", event_id.to_hex(), true, ""])
                        .to_string()
                        .into(),
                ))
                .unwrap();
            break;
        }
    });
    (relay, event_rx)
}

fn wait_for_status(
    statuses: &mpsc::Receiver<WriteStatus>,
    predicate: impl Fn(&WriteStatus) -> bool,
) -> WriteStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = statuses
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("expected receipt status before deadline");
        if predicate(&status) {
            return status;
        }
    }
}

#[test]
fn offline_accept_restart_real_bunker_reattach_publish_and_ack() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nip46-restart.redb");
    let (signer_relay, remote, user) = spawn_signer_relay(false);
    let (write_relay, published) = spawn_write_relay();
    let directory =
        || FixtureDirectory::new().with_write(user.public_key().to_hex(), [write_relay.clone()]);
    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_046),
        Kind::TextNote,
        vec![Tag::hashtag("nip46")],
        "accepted before the signer existed",
    );
    let frozen_id = EventId::new(
        &unsigned.pubkey,
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );

    let receipt_id = {
        let (engine, handle) = EngineThread::spawn(
            RedbStore::open(&path).unwrap(),
            directory(),
            10,
            PoolConfig::default(),
            RelayAdmissionPolicy::default(),
        );
        handle.set_active_account(Some(user.public_key()));
        let receipt = handle
            .publish_tracked(WriteIntent {
                payload: WritePayload::Unsigned(unsigned.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            })
            .unwrap();
        assert_eq!(receipt.statuses.recv().unwrap(), WriteStatus::Accepted);
        assert_eq!(
            receipt.statuses.recv().unwrap(),
            WriteStatus::AwaitingCapability
        );
        handle.shutdown();
        engine.join();
        receipt.id
    };

    let store = RedbStore::open(&path).unwrap();
    let rows = store.query(&nostr::Filter::new().id(frozen_id)).unwrap();
    assert_eq!(rows.len(), 1, "the accepted pending row survives restart");
    assert_eq!(
        rows[0].provenance.local.as_ref().unwrap().sig_state,
        SigState::Pending
    );
    drop(store);

    let (engine, handle) = EngineThread::spawn(
        RedbStore::open(&path).unwrap(),
        directory(),
        10,
        PoolConfig::default(),
        RelayAdmissionPolicy::default(),
    );
    let statuses = match handle.reattach_receipt(receipt_id) {
        ReceiptReattachment::Attached(statuses) => statuses,
        _ => panic!("durable receipt must reattach after restart"),
    };
    assert_eq!(statuses.recv().unwrap(), WriteStatus::Accepted);
    assert_eq!(statuses.recv().unwrap(), WriteStatus::AwaitingCapability);

    let bunker_uri = format!(
        "bunker://{}?relay={}&secret=restart-proof",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(signer_relay.as_str().as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&bunker_uri, Duration::from_secs(5)).unwrap();
    assert_eq!(signer.user_public_key(), user.public_key());
    handle.add_signer(signer).unwrap();

    assert!(matches!(
        wait_for_status(&statuses, |status| matches!(status, WriteStatus::Signed(_))),
        WriteStatus::Signed(id) if id == frozen_id
    ));
    assert!(matches!(
        wait_for_status(&statuses, |status| matches!(status, WriteStatus::Acked(_))),
        WriteStatus::Acked(relay) if relay == write_relay
    ));
    let event = published.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(event.id, frozen_id);
    assert_eq!(event.pubkey, unsigned.pubkey);
    assert_eq!(event.created_at, unsigned.created_at);
    assert_eq!(event.kind, unsigned.kind);
    assert_eq!(event.tags, unsigned.tags);
    assert_eq!(event.content, unsigned.content);

    handle.shutdown();
    engine.join();
}

#[test]
fn mutated_real_bunker_response_retracts_pending_and_restores_replaceable_predecessor() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nip46-terminal-compensation.redb");
    let (signer_relay, remote, user) = spawn_signer_relay(true);
    let source_relay = RelayUrl::parse("wss://source.example").unwrap();
    let predecessor = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_040),
        Kind::Metadata,
        Vec::new(),
        "previous canonical profile",
    )
    .sign_with_keys(&user)
    .unwrap();
    let replacement = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_041),
        Kind::Metadata,
        Vec::new(),
        "pending replacement",
    );
    let replacement_id = EventId::new(
        &replacement.pubkey,
        &replacement.created_at,
        &replacement.kind,
        &replacement.tags,
        &replacement.content,
    );

    let mut store = RedbStore::open(&path).unwrap();
    store
        .insert(
            predecessor.clone(),
            RelayObserved::new(source_relay, Timestamp::from(1_700_000_042)),
        )
        .unwrap();

    let (engine, handle) = EngineThread::spawn(
        store,
        FixtureDirectory::new(),
        10,
        PoolConfig::default(),
        RelayAdmissionPolicy::default(),
    );
    handle.set_active_account(Some(user.public_key()));
    let bunker_uri = format!(
        "bunker://{}?relay={}&secret=terminal-proof",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(signer_relay.as_str().as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&bunker_uri, Duration::from_secs(5)).unwrap();
    handle.add_signer(signer).unwrap();

    let receipt = handle
        .publish_tracked(WriteIntent {
            payload: WritePayload::Unsigned(replacement),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        })
        .unwrap();
    assert_eq!(receipt.statuses.recv().unwrap(), WriteStatus::Accepted);
    assert!(matches!(
        wait_for_status(&receipt.statuses, |status| matches!(status, WriteStatus::Failed(_))),
        WriteStatus::Failed(reason) if reason.contains("mutated")
    ));

    handle.shutdown();
    engine.join();

    let store = RedbStore::open(&path).unwrap();
    assert!(
        store
            .query(&nostr::Filter::new().id(replacement_id))
            .unwrap()
            .is_empty(),
        "the invalid pending replacement is retracted through compensation"
    );
    let restored = store
        .query(&nostr::Filter::new().id(predecessor.id))
        .unwrap();
    assert_eq!(restored.len(), 1, "the displaced predecessor is restored");
    assert_eq!(restored[0].event, predecessor);
}
