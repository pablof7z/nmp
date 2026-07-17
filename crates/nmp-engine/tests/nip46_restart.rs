//! Issue #6 acceptance proof: the durable write obligation outlives the
//! process that accepted it, then a real NIP-46 session reattaches and drives
//! the ordinary promotion/publication path to a relay ACK.

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nmp_engine::core::{RelayAdmissionPolicy, RowDelta};
use nmp_engine::outbox::WriteStatus;
use nmp_engine::runtime::{EngineThread, ReceiptReattachment, RowsReceiver};
use nmp_grammar::{Binding, Durability, Filter, WriteIntent, WritePayload, WriteRouting};
use nmp_resolver::LiveQuery;
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

struct SignResponseBarrier {
    request_observed: mpsc::Sender<()>,
    release_response: mpsc::Receiver<()>,
}

fn spawn_signer_relay(
    mutate_sign_event: bool,
    sign_barrier: Option<SignResponseBarrier>,
) -> (RelayUrl, Keys, Keys) {
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
                            if let Some(barrier) = &sign_barrier {
                                barrier
                                    .request_observed
                                    .send(())
                                    .expect("test receives the blocked signing request");
                                barrier
                                    .release_response
                                    .recv_timeout(Duration::from_secs(5))
                                    .expect("test releases the blocked signing response");
                            }
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

/// #571's checkpoint/restore falsifier needs the mock signer relay to serve
/// TWO independent sequential client connections against the SAME
/// `remote`/`user` identity: an initial pairing session, then a later
/// checkpoint-restored session with no re-pairing. `spawn_signer_relay`
/// above accepts only once; this variant loops `accept()` and reports each
/// connection's observed method sequence so the test can prove the restored
/// connection never re-sends `connect`.
fn spawn_multi_session_signer_relay(
    remote: Keys,
    user: Keys,
) -> (RelayUrl, mpsc::Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = RelayUrl::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
    let (seen_tx, seen_rx) = mpsc::channel();

    thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let mut socket = tungstenite::accept(stream).unwrap();
            let mut subscription_id = None;
            let mut seen_methods = Vec::new();
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
                            remote.secret_key(),
                            &event.pubkey,
                            event.content.as_bytes(),
                        )
                        .unwrap();
                        let request: Value = serde_json::from_str(&plaintext).unwrap();
                        let id = request["id"].as_str().unwrap();
                        let method = request["method"].as_str().unwrap();
                        let params = request["params"].as_array().unwrap();
                        seen_methods.push(method.to_string());
                        let result = match method {
                            "connect" => "ack".to_string(),
                            "get_public_key" => user.public_key().to_hex(),
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
                                UnsignedEvent::new(
                                    user.public_key(),
                                    Timestamp::from(body.created_at),
                                    Kind::from_u16(body.kind),
                                    tags,
                                    body.content,
                                )
                                .sign_with_keys(&user)
                                .unwrap()
                                .as_json()
                            }
                            other => panic!("unexpected NIP-46 method {other}"),
                        };
                        let response = response_event(&remote, event.pubkey, id, result);
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
            let _ = seen_tx.send(seen_methods);
        }
    });
    (relay, seen_rx)
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

fn wait_for_exact_rows(
    rows: &RowsReceiver,
    current: &mut BTreeSet<EventId>,
    expected_present: EventId,
    expected_absent: EventId,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (deltas, _) = rows
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("expected live row state before deadline");
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    current.insert(row.event.id);
                }
                RowDelta::Removed(id) => {
                    current.remove(&id);
                }
                RowDelta::SourcesGrew { .. } => {}
            }
        }
        if current.contains(&expected_present) && !current.contains(&expected_absent) {
            return;
        }
    }
}

#[test]
fn offline_accept_restart_real_bunker_reattach_publish_and_ack() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nip46-restart.redb");
    let (signer_relay, remote, user) = spawn_signer_relay(false, None);
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
            RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
        )
        .expect("test engine thread construction");
        handle.set_active_account(Some(user.public_key()));
        let receipt = handle
            .publish_tracked(WriteIntent {
                payload: WritePayload::Unsigned(unsigned.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            })
            .unwrap();
        assert_eq!(receipt.statuses.recv().unwrap(), WriteStatus::Accepted);
        assert_eq!(
            receipt.statuses.recv().unwrap(),
            WriteStatus::AwaitingCapability {
                pubkey: user.public_key()
            }
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
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )
    .expect("test engine thread construction");
    let statuses = match handle.reattach_receipt(receipt_id) {
        ReceiptReattachment::Attached(statuses) => statuses,
        _ => panic!("durable receipt must reattach after restart"),
    };
    assert_eq!(statuses.recv().unwrap(), WriteStatus::Accepted);
    assert_eq!(
        statuses.recv().unwrap(),
        WriteStatus::AwaitingCapability {
            pubkey: user.public_key()
        }
    );

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
    let (request_observed_tx, request_observed_rx) = mpsc::channel();
    let (release_response_tx, release_response_rx) = mpsc::channel();
    let (signer_relay, remote, user) = spawn_signer_relay(
        true,
        Some(SignResponseBarrier {
            request_observed: request_observed_tx,
            release_response: release_response_rx,
        }),
    );
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
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )
    .expect("test engine thread construction");
    handle.set_active_account(Some(user.public_key()));
    let (query_handle, rows) = handle
        .subscribe(LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([Kind::Metadata.as_u16()])),
            authors: Some(Binding::Literal(BTreeSet::from([user
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        }))
        .expect("test subscription construction");
    let mut current_rows = BTreeSet::new();
    wait_for_exact_rows(&rows, &mut current_rows, predecessor.id, replacement_id);
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
            identity_override: None,
        })
        .unwrap();
    assert_eq!(receipt.statuses.recv().unwrap(), WriteStatus::Accepted);
    request_observed_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the signer response is blocked after the optimistic accept");
    wait_for_exact_rows(&rows, &mut current_rows, replacement_id, predecessor.id);
    release_response_tx
        .send(())
        .expect("release the deliberately mutated signer response");
    assert!(matches!(
        wait_for_status(&receipt.statuses, |status| matches!(status, WriteStatus::Failed(_))),
        WriteStatus::Failed(reason) if reason.contains("mutated")
    ));

    handle.unsubscribe(query_handle);
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

/// #571 acceptance falsifier: pair once, take a checkpoint, accept a
/// durable write while offline, close/reopen the store in a fresh engine,
/// restore the signer from the checkpoint alone (NO re-pairing), reach the
/// identical user pubkey, and resume/sign/publish the parked obligation.
/// Also proves the checkpoint's client secret never lands in the redb dump.
#[test]
fn checkpoint_restore_reattaches_durable_write_without_repairing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nip46-checkpoint-restart.redb");
    let remote = Keys::generate();
    let user = Keys::generate();
    let (signer_relay, seen) = spawn_multi_session_signer_relay(remote.clone(), user.clone());
    let (write_relay, published) = spawn_write_relay();
    let directory =
        || FixtureDirectory::new().with_write(user.public_key().to_hex(), [write_relay.clone()]);
    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_060),
        Kind::TextNote,
        vec![Tag::hashtag("nip46-checkpoint")],
        "accepted before the restored signer existed",
    );
    let frozen_id = EventId::new(
        &unsigned.pubkey,
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );

    // Phase 0: pair once (independent of any engine/store) and take a
    // checkpoint of that already-authorized session.
    let bunker_uri = format!(
        "bunker://{}?relay={}&secret=checkpoint-restart-proof",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(signer_relay.as_str().as_bytes()).collect::<String>()
    );
    let paired = Nip46Signer::connect_bunker(&bunker_uri, Duration::from_secs(5)).unwrap();
    assert_eq!(paired.user_public_key(), user.public_key());
    let checkpoint = paired.checkpoint();
    drop(paired);
    let pairing_methods = seen.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(pairing_methods.contains(&"connect".to_string()));

    // Phase 1: the durable write is accepted while no signer is attached.
    let receipt_id = {
        let (engine, handle) = EngineThread::spawn(
            RedbStore::open(&path).unwrap(),
            directory(),
            10,
            PoolConfig::default(),
            RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
        )
        .expect("test engine thread construction");
        handle.set_active_account(Some(user.public_key()));
        let receipt = handle
            .publish_tracked(WriteIntent {
                payload: WritePayload::Unsigned(unsigned.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            })
            .unwrap();
        assert_eq!(receipt.statuses.recv().unwrap(), WriteStatus::Accepted);
        assert_eq!(
            receipt.statuses.recv().unwrap(),
            WriteStatus::AwaitingCapability {
                pubkey: user.public_key()
            }
        );
        handle.shutdown();
        engine.join();
        receipt.id
    };

    // Secrecy falsifier: the checkpoint's client secret must never appear
    // anywhere in the closed redb store's raw bytes.
    let raw_store_bytes = std::fs::read(&path).unwrap();
    let secret_hex = checkpoint.client_secret_key.to_secret_hex();
    let secret_needle = secret_hex.as_bytes();
    let contains_secret = raw_store_bytes
        .windows(secret_needle.len())
        .any(|window| window == secret_needle);
    assert!(
        !contains_secret,
        "the NIP-46 client secret must never appear in the redb store dump"
    );

    // Phase 2: close/reopen in a fresh engine and reattach the parked
    // receipt -- still no signer attached.
    let (engine, handle) = EngineThread::spawn(
        RedbStore::open(&path).unwrap(),
        directory(),
        10,
        PoolConfig::default(),
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )
    .expect("test engine thread construction");
    let statuses = match handle.reattach_receipt(receipt_id) {
        ReceiptReattachment::Attached(statuses) => statuses,
        _ => panic!("durable receipt must reattach after restart"),
    };
    assert_eq!(statuses.recv().unwrap(), WriteStatus::Accepted);
    assert_eq!(
        statuses.recv().unwrap(),
        WriteStatus::AwaitingCapability {
            pubkey: user.public_key()
        }
    );

    // Phase 3: restore the signer from the checkpoint ALONE -- no bunker
    // URI, no `nostrconnect://` invitation, no re-pairing handshake.
    let restored = Nip46Signer::from_parts(checkpoint, Duration::from_secs(5)).unwrap();
    assert_eq!(restored.user_public_key(), user.public_key());
    handle.add_signer(restored).unwrap();

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
    assert_eq!(event.content, unsigned.content);

    handle.shutdown();
    engine.join();

    let restore_methods = seen.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        !restore_methods.contains(&"connect".to_string()),
        "restore must never re-send the pairing `connect` RPC: {restore_methods:?}"
    );
    assert!(restore_methods.contains(&"get_public_key".to_string()));
    assert!(restore_methods.contains(&"sign_event".to_string()));
}
