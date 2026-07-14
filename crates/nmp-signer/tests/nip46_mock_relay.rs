use std::net::TcpListener;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use nmp_signer::{
    CryptoCapability, Nip46Cancellation, Nip46ClientMetadata, Nip46ConnectionEvent,
    Nip46Invitation, Nip46Signer, SignerError, SignerOp, SigningCapability,
};
use nostr::nips::nip44;
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};
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

fn response_event(
    signer: &Keys,
    client: PublicKey,
    id: &str,
    result: Option<String>,
    error: Option<&str>,
) -> Event {
    let plaintext = json!({ "id": id, "result": result, "error": error }).to_string();
    let ciphertext = nip44::encrypt(
        signer.secret_key(),
        &client,
        plaintext,
        nip44::Version::default(),
    )
    .unwrap();
    EventBuilder::new(Kind::NostrConnect, ciphertext)
        .tag(Tag::public_key(client))
        .sign_with_keys(signer)
        .unwrap()
}

fn event_frame(subscription_id: &str, event: Event) -> String {
    json!(["EVENT", subscription_id, event]).to_string()
}

fn spawn_mock_remote_signer(
    mutate_sign_event: bool,
) -> (String, Keys, Keys, mpsc::Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let relay_url = format!("ws://{address}");
    let remote_signer = Keys::generate();
    let user = Keys::generate();
    let remote_for_thread = remote_signer.clone();
    let user_for_thread = user.clone();
    let (seen_tx, seen_rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        socket
            .send(Message::Text(
                json!(["AUTH", "mock-auth-challenge"]).to_string().into(),
            ))
            .unwrap();

        let mut subscription_id = None;
        let mut seen_methods = Vec::new();
        let mut saw_auth = false;
        while let Ok(message) = socket.read() {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(text.as_ref()) else {
                continue;
            };
            let Some(parts) = frame.as_array() else {
                continue;
            };
            match parts.first().and_then(Value::as_str) {
                Some("AUTH") => {
                    let event = Event::from_json(parts[1].to_string()).unwrap();
                    assert_eq!(event.kind, Kind::Authentication);
                    assert_eq!(event.tags.challenge(), Some("mock-auth-challenge"));
                    event.verify().unwrap();
                    saw_auth = true;
                }
                Some("REQ") => {
                    subscription_id = parts.get(1).and_then(Value::as_str).map(str::to_string);
                }
                Some("EVENT") => {
                    let event = Event::from_json(parts[1].to_string()).unwrap();
                    assert_eq!(event.kind, Kind::NostrConnect);
                    assert!(event
                        .tags
                        .public_keys()
                        .any(|pk| *pk == remote_for_thread.public_key()));
                    let plaintext = nip44::decrypt(
                        remote_for_thread.secret_key(),
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
                        "connect" => {
                            assert_eq!(params[0], remote_for_thread.public_key().to_hex());
                            Some("ack".to_string())
                        }
                        "get_public_key" => Some(user_for_thread.public_key().to_hex()),
                        "switch_relays" => Some("null".to_string()),
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
                                "mutated by signer".to_string()
                            } else {
                                body.content
                            };
                            let unsigned = UnsignedEvent::new(
                                user_for_thread.public_key(),
                                Timestamp::from(body.created_at),
                                Kind::from_u16(body.kind),
                                tags,
                                content,
                            );
                            let auth = response_event(
                                &remote_for_thread,
                                event.pubkey,
                                id,
                                Some("auth_url".to_string()),
                                Some("https://signer.example/approve"),
                            );
                            socket
                                .send(Message::Text(
                                    event_frame(subscription_id.as_deref().unwrap(), auth).into(),
                                ))
                                .unwrap();
                            Some(unsigned.sign_with_keys(&user_for_thread).unwrap().as_json())
                        }
                        "nip44_encrypt" => Some(
                            nip44::encrypt(
                                user_for_thread.secret_key(),
                                &PublicKey::from_hex(params[0].as_str().unwrap()).unwrap(),
                                params[1].as_str().unwrap(),
                                nip44::Version::default(),
                            )
                            .unwrap(),
                        ),
                        "nip44_decrypt" => Some(
                            nip44::decrypt(
                                user_for_thread.secret_key(),
                                &PublicKey::from_hex(params[0].as_str().unwrap()).unwrap(),
                                params[1].as_str().unwrap().as_bytes(),
                            )
                            .unwrap(),
                        ),
                        other => panic!("unexpected method {other}"),
                    };
                    let response =
                        response_event(&remote_for_thread, event.pubkey, id, result, None);
                    socket
                        .send(Message::Text(
                            event_frame(subscription_id.as_deref().unwrap(), response).into(),
                        ))
                        .unwrap();
                    if method == "nip44_decrypt" {
                        assert!(saw_auth);
                        let _ = seen_tx.send(seen_methods);
                        break;
                    }
                }
                _ => {}
            }
        }
    });

    (relay_url, remote_signer, user, seen_rx)
}

fn spawn_unresponsive_remote_signer() -> (String, Keys, Keys, mpsc::Receiver<()>, mpsc::Receiver<()>)
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay_url = format!("ws://{}", listener.local_addr().unwrap());
    let remote = Keys::generate();
    let user = Keys::generate();
    let remote_thread = remote.clone();
    let user_thread = user.clone();
    let (closed_tx, closed_rx) = mpsc::channel();
    let (sign_seen_tx, sign_seen_rx) = mpsc::channel();

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
                    let result = match method {
                        "connect" => Some("ack".to_string()),
                        "get_public_key" => Some(user_thread.public_key().to_hex()),
                        "switch_relays" => None,
                        "sign_event" => {
                            let _ = sign_seen_tx.send(());
                            None
                        }
                        other => panic!("unexpected method {other}"),
                    };
                    if let Some(result) = result {
                        let response =
                            response_event(&remote_thread, event.pubkey, id, Some(result), None);
                        socket
                            .send(Message::Text(
                                event_frame(subscription_id.as_deref().unwrap(), response).into(),
                            ))
                            .unwrap();
                    }
                }
                _ => {}
            }
        }
        let _ = closed_tx.send(());
    });

    (relay_url, remote, user, closed_rx, sign_seen_rx)
}

#[test]
fn real_bunker_flow_auth_sign_and_crypto_round_trip() {
    let (relay, remote, user, seen) = spawn_mock_remote_signer(false);
    let uri = format!(
        "bunker://{}?relay={}&secret=one-use-secret",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&uri, Duration::from_secs(5)).unwrap();
    assert_eq!(signer.remote_signer_public_key(), remote.public_key());
    assert_eq!(signer.user_public_key(), user.public_key());
    assert_ne!(signer.remote_signer_public_key(), signer.user_public_key());

    let events = signer.subscribe_connection_events();
    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_000),
        Kind::TextNote,
        vec![Tag::hashtag("nip46")],
        "signed remotely",
    );
    let signed = signer
        .sign(unsigned.clone())
        .wait(Duration::from_secs(5))
        .unwrap();
    signed.verify().unwrap();
    assert_eq!(signed.pubkey, user.public_key());
    assert_eq!(signed.content, unsigned.content);
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(2)).unwrap(),
        Nip46ConnectionEvent::AuthorizationRequired(url)
            if url == "https://signer.example/approve"
    ));

    let peer = Keys::generate();
    let ciphertext = signer
        .nip44_encrypt(peer.public_key(), "secret payload")
        .wait(Duration::from_secs(5))
        .unwrap();
    let plaintext = signer
        .nip44_decrypt(peer.public_key(), &ciphertext)
        .wait(Duration::from_secs(5))
        .unwrap();
    assert_eq!(plaintext, "secret payload");

    let methods = seen.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(methods.starts_with(&[
        "connect".to_string(),
        "get_public_key".to_string(),
        "switch_relays".to_string(),
    ]));
    assert!(methods.ends_with(&[
        "sign_event".to_string(),
        "nip44_encrypt".to_string(),
        "nip44_decrypt".to_string(),
    ]));
}

#[test]
fn valid_but_mutated_signer_event_is_terminal_invalid_response() {
    let (relay, remote, user, _seen) = spawn_mock_remote_signer(true);
    let uri = format!(
        "bunker://{}?relay={}&secret=one-use-secret",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&uri, Duration::from_secs(5)).unwrap();
    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_001),
        Kind::TextNote,
        vec![],
        "the frozen body",
    );
    assert!(matches!(
        signer.sign(unsigned).wait(Duration::from_secs(5)),
        Err(SignerError::InvalidResponse(reason)) if reason.contains("mutated")
    ));
}

#[test]
fn client_invitation_ignores_forged_secret_then_accepts_valid_signer() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = format!("ws://{}", listener.local_addr().unwrap());
    let invitation = Nip46Invitation::new(
        vec![nostr::RelayUrl::parse(&relay).unwrap()],
        None,
        Nip46ClientMetadata::default(),
    )
    .unwrap();
    let uri = url::Url::parse(&invitation.uri()).unwrap();
    let client = PublicKey::from_hex(uri.host_str().unwrap()).unwrap();
    let secret = uri
        .query_pairs()
        .find(|(key, _)| key == "secret")
        .map(|(_, value)| value.into_owned())
        .unwrap();
    let attacker = Keys::generate();
    let remote = Keys::generate();
    let user = Keys::generate();
    let expected_remote = remote.public_key();
    let expected_user = user.public_key();

    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        let mut subscription_id = None;
        while let Ok(message) = socket.read() {
            let Message::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_ref()).unwrap();
            let parts = frame.as_array().unwrap();
            match parts.first().and_then(Value::as_str) {
                Some("REQ") => {
                    subscription_id = parts.get(1).and_then(Value::as_str).map(str::to_string);
                    let forged = response_event(
                        &attacker,
                        client,
                        "connect-forged",
                        Some("wrong-secret".to_string()),
                        None,
                    );
                    socket
                        .send(Message::Text(
                            event_frame(subscription_id.as_deref().unwrap(), forged).into(),
                        ))
                        .unwrap();
                    let valid = response_event(
                        &remote,
                        client,
                        "connect-valid",
                        Some(secret.clone()),
                        None,
                    );
                    socket
                        .send(Message::Text(
                            event_frame(subscription_id.as_deref().unwrap(), valid).into(),
                        ))
                        .unwrap();
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
                    let result = match method {
                        "get_public_key" => user.public_key().to_hex(),
                        "switch_relays" => "null".to_string(),
                        other => panic!("unexpected method {other}"),
                    };
                    let response = response_event(&remote, event.pubkey, id, Some(result), None);
                    socket
                        .send(Message::Text(
                            event_frame(subscription_id.as_deref().unwrap(), response).into(),
                        ))
                        .unwrap();
                    if method == "switch_relays" {
                        break;
                    }
                }
                _ => {}
            }
        }
    });

    let signer = invitation.connect(Duration::from_secs(5)).unwrap();
    assert_eq!(signer.remote_signer_public_key(), expected_remote);
    assert_eq!(signer.user_public_key(), expected_user);
}

#[test]
fn unavailable_signer_operation_is_retryable() {
    let (tx, rx) = crossbeam_channel::unbounded::<Result<String, nmp_signer::SignerError>>();
    drop(tx);
    assert_eq!(
        SignerOp::pending(rx).wait(Duration::from_millis(10)),
        Err(nmp_signer::SignerError::Disconnected)
    );
}

#[test]
fn engine_associated_connection_and_signing_peak_is_six_executor_tasks() {
    let (relay, remote, user, _closed, sign_seen) = spawn_unresponsive_remote_signer();
    let uri = format!(
        "bunker://{}?relay={}&secret=executor-peak",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let executor = nmp_executor::Executor::new(nmp_executor::DEFAULT_MAX_TASKS).unwrap();
    let cancellation = Nip46Cancellation::default();
    let shutdown_cancellation = cancellation.clone();
    let connect_cancellation = cancellation.clone();
    let connect_executor = executor.clone();
    let (signer_tx, signer_rx) = mpsc::channel();
    let (release_connection_tx, release_connection_rx) = mpsc::channel();

    executor
        .spawn_with_cancel(
            "NIP-46 connection",
            move || {
                shutdown_cancellation.cancel();
                let _ = release_connection_tx.send(());
            },
            move || {
                let signer = Nip46Signer::connect_bunker_observed_with_executor_and_cancellation(
                    &uri,
                    None,
                    Nip46ClientMetadata::default(),
                    Duration::from_secs(5),
                    Arc::new(|_| {}),
                    &connect_cancellation,
                    connect_executor,
                )
                .unwrap();
                signer_tx.send(signer).unwrap();
                let _ = release_connection_rx.recv();
            },
        )
        .unwrap();
    let signer = signer_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_003),
        Kind::TextNote,
        Vec::new(),
        "held signing peak",
    );
    let pending = match signer.sign(unsigned) {
        SignerOp::Pending(pending) => pending,
        SignerOp::Ready(_) => panic!("remote signing must be pending"),
    };
    sign_seen
        .recv_timeout(Duration::from_secs(2))
        .expect("mock signer must receive the held sign request");

    let (pending_rx, cancel) = pending.into_parts();
    let cancel = Arc::new(Mutex::new(cancel));
    let shutdown_cancel = Arc::clone(&cancel);
    executor
        .spawn_with_cancel(
            "engine signer waiter",
            move || {
                if let Some(cancel) = shutdown_cancel.lock().unwrap().take() {
                    cancel();
                }
            },
            move || {
                let _ = pending_rx.recv();
            },
        )
        .unwrap();

    assert_eq!(
        executor.census().admitted,
        6,
        "connection, session, event-forward, switch-relays, mapper, and engine waiter"
    );
    executor.shutdown();
    assert_eq!(executor.census().admitted, 0);
    assert_eq!(executor.census().running, 0);
    drop(signer);
}

#[test]
fn ignored_switch_relays_cannot_keep_the_session_alive_after_signer_drop() {
    let (relay, remote, _user, closed, _sign_seen) = spawn_unresponsive_remote_signer();
    let uri = format!(
        "bunker://{}?relay={}&secret=ignored-switch",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&uri, Duration::from_secs(5)).unwrap();

    drop(signer);

    closed
        .recv_timeout(Duration::from_secs(2))
        .expect("dropping the signer closes the session even when switch_relays never answers");
}

#[test]
fn abandoned_remote_operations_release_every_bounded_pending_slot() {
    let (relay, remote, user, _closed, _sign_seen) = spawn_unresponsive_remote_signer();
    let uri = format!(
        "bunker://{}?relay={}&secret=abandoned-ops",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&uri, Duration::from_secs(5)).unwrap();
    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_002),
        Kind::TextNote,
        Vec::new(),
        "never answered",
    );

    for _ in 0..64 {
        drop(signer.sign(unsigned.clone()));
    }
    thread::sleep(Duration::from_millis(100));

    assert_eq!(
        signer.sign(unsigned).wait(Duration::from_millis(50)),
        Err(SignerError::Timeout),
        "the next request is admitted; it is not rejected by leaked pending slots",
    );
}
