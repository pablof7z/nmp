use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nmp_nip46::{
    Nip46ClientMetadata, Nip46ConnectionEvent, Nip46Invitation, Nip46Origin, Nip46Signer,
};
use nmp_signer::{
    CryptoCapability, SignerError, SignerOp, SignerPublicKey, SignerSignedEvent,
    SignerSignedEventParts, SignerUnsignedEvent, SigningCapability,
};
use nostr::nips::nip44;
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};
use serde::Deserialize;
use serde_json::{json, Value};
use tungstenite::Message;

/// The timeout every test here hands to a NIP-46 entry point. The mock relay
/// answers in microseconds, so this is a liveness backstop for a session that
/// has genuinely hung -- never a performance assertion. Every NIP-46 session
/// in the process shares ONE runtime worker thread, so a whole test binary run
/// in parallel can starve any single flow for a long time; the budget is sized
/// for that, not for the happy path.
const REMOTE_OPERATION: Duration = Duration::from_secs(30);

/// The budget for *observing* that a flow happened -- a recorder having seen
/// the expected calls, a channel having delivered its event.
///
/// #1036: this used to be 2s while the operations it summarised each got 5s,
/// so the deadline for noticing a flow was tighter than the flow itself. It is
/// now never tighter than [`REMOTE_OPERATION`], and every wait on it reports
/// what it *did* see on expiry -- an observation that times out must say what
/// the session actually did, not just that a channel went quiet.
const OBSERVATION: Duration = Duration::from_secs(60);

/// Wait until the mock signer has recorded `expected` method calls.
///
/// The condition is the recorded sequence, not the elapsed time: callers mean
/// "after the N calls have been recorded", so that is what is waited on. On
/// expiry the panic names the methods that *were* seen, which is the whole
/// difference between a diagnosable failure and `unwrap()` on a
/// `RecvTimeoutError`.
fn recorded_methods(seen: &mpsc::Receiver<String>, expected: usize) -> Vec<String> {
    let deadline = Instant::now() + OBSERVATION;
    let mut methods: Vec<String> = Vec::with_capacity(expected);
    while methods.len() < expected {
        match seen.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(method) => methods.push(method),
            Err(error) => panic!(
                "expected {expected} methods, saw {} [{}] ({error})",
                methods.len(),
                methods.join(", "),
            ),
        }
    }
    methods
}

/// Wait for one recorded session's method list (the multi-session relay
/// reports per CONNECTION, since "how many methods" is not fixed there -- the
/// best-effort `switch_relays` may or may not land before teardown).
fn recorded_session(seen: &mpsc::Receiver<Vec<String>>, session: &str) -> Vec<String> {
    seen.recv_timeout(OBSERVATION).unwrap_or_else(|error| {
        panic!("the mock relay never recorded the {session} session's methods ({error})")
    })
}

/// Wait for the `AuthorizationRequired` notification specifically, rather than
/// asserting it is the *first* event to arrive: a session may legitimately
/// interleave availability or relay-authentication notices ahead of it, and
/// which ones land first is a scheduling detail, not a NIP-46 claim.
fn authorization_required_url(events: &mpsc::Receiver<Nip46ConnectionEvent>) -> String {
    let deadline = Instant::now() + OBSERVATION;
    let mut others = Vec::new();
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Nip46ConnectionEvent::AuthorizationRequired(url)) => return url,
            Ok(other) => others.push(other),
            Err(error) => panic!(
                "the session never reported AuthorizationRequired ({error}); \
                 it reported {others:?}"
            ),
        }
    }
}

#[derive(Deserialize)]
struct SignBody {
    kind: u16,
    created_at: u64,
    tags: Vec<Vec<String>>,
    content: String,
}

fn signer_unsigned(unsigned: &UnsignedEvent) -> SignerUnsignedEvent {
    SignerUnsignedEvent::new(
        SignerPublicKey::new(unsigned.pubkey.to_bytes()),
        unsigned.created_at.as_secs(),
        unsigned.kind.as_u16(),
        unsigned
            .tags
            .clone()
            .to_vec()
            .into_iter()
            .map(Tag::to_vec)
            .collect(),
        unsigned.content.clone(),
    )
}

fn nostr_signed(signed: SignerSignedEvent) -> Event {
    let SignerSignedEventParts {
        id,
        public_key,
        created_at,
        kind,
        tags,
        content,
        signature,
    } = signed.into_parts();
    Event::new(
        nostr::EventId::from_slice(&id).unwrap(),
        PublicKey::from_slice(public_key.as_bytes()).unwrap(),
        Timestamp::from(created_at),
        Kind::from(kind),
        tags.into_iter()
            .map(Tag::parse)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        content,
        nostr::secp256k1::schnorr::Signature::from_slice(&signature).unwrap(),
    )
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

/// A bunker-style mock that reports each method name as it observes it.
///
/// #1036: it used to accumulate the names privately and publish the whole list
/// once, on `nip44_decrypt`. That made "how far did the flow actually get"
/// unobservable -- a waiter could only ever see all of it or none of it -- and
/// it truncated the record at `nip44_decrypt`, so the best-effort
/// `switch_relays` was silently lost whenever it landed after the last awaited
/// RPC. Streaming each name lets a waiter block on the condition it means and
/// name what it saw when the condition is not met.
fn spawn_mock_remote_signer(
    mutate_sign_event: bool,
) -> (String, Keys, Keys, mpsc::Receiver<String>) {
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
                    let _ = seen_tx.send(method.to_string());

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
                    }
                    // #1036: no `break` here. The best-effort `switch_relays`
                    // is never awaited by the client, so it can legitimately
                    // arrive after the last awaited RPC -- stopping on
                    // `nip44_decrypt` would drop it from the record and make
                    // "was it fired at all?" unanswerable. The thread ends
                    // when the client disconnects, which also drops the
                    // recorder and turns any still-blocked waiter into a
                    // reported failure instead of a hang.
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

/// A bunker-style mock that accepts and fully serves an arbitrary number of
/// SEQUENTIAL client connections against the same `remote`/`user` identity
/// (#571): pairing (session 1) followed by a checkpoint-restored session
/// (session 2) reconnect to the same relay URL as two independent TCP
/// connections. Returns the observed method-name sequence per connection so
/// a test can prove the restored connection never re-sends `connect`.
fn spawn_multi_session_signer_relay(
    remote: Keys,
    user: Keys,
) -> (String, mpsc::Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay_url = format!("ws://{}", listener.local_addr().unwrap());
    let (seen_tx, seen_rx) = mpsc::channel();

    thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let mut socket = tungstenite::accept(stream).unwrap();
            let mut subscription_id = None;
            let mut seen_methods = Vec::new();
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
                            "connect" => {
                                assert_eq!(params[0], remote.public_key().to_hex());
                                "ack".to_string()
                            }
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
                            other => panic!("unexpected method {other}"),
                        };
                        let response =
                            response_event(&remote, event.pubkey, id, Some(result), None);
                        socket
                            .send(Message::Text(
                                event_frame(subscription_id.as_deref().unwrap(), response).into(),
                            ))
                            .unwrap();
                    }
                    _ => {}
                }
            }
            let _ = seen_tx.send(seen_methods);
        }
    });

    (relay_url, seen_rx)
}

/// #571 falsifier: a checkpoint read from an already-authorized session
/// reconnects through `Nip46Signer::from_parts` with NO re-pairing
/// handshake (no second `connect` RPC), reaches the identical user pubkey,
/// and can still sign -- proving restore is a genuine reconnect, not a
/// disguised re-pair.
#[test]
fn checkpoint_then_from_parts_reconnects_without_repairing_and_signs() {
    let remote = Keys::generate();
    let user = Keys::generate();
    let (relay, seen) = spawn_multi_session_signer_relay(remote.clone(), user.clone());

    let uri = format!(
        "bunker://{}?relay={}&secret=checkpoint-proof",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let paired = Nip46Signer::connect_bunker(&uri, REMOTE_OPERATION).unwrap();
    assert_eq!(paired.user_public_key(), user.public_key());

    let checkpoint = paired.checkpoint();
    assert_eq!(checkpoint.user_public_key, user.public_key());
    assert_eq!(checkpoint.remote_signer_public_key, remote.public_key());
    assert_eq!(checkpoint.origin, Nip46Origin::Bunker);
    assert_eq!(
        checkpoint.relays,
        vec![nostr::RelayUrl::parse(&relay).unwrap()]
    );

    // Session 1 ends (its process would exit here); the checkpoint outlives
    // it.
    drop(paired);
    let first_session_methods = recorded_session(&seen, "first");
    assert_eq!(
        first_session_methods
            .iter()
            .filter(|m| *m == "connect")
            .count(),
        1
    );

    // Session 2: a fresh process restores from the checkpoint alone.
    let restored = Nip46Signer::from_parts(checkpoint, REMOTE_OPERATION).unwrap();
    assert_eq!(restored.user_public_key(), user.public_key());
    assert_eq!(restored.remote_signer_public_key(), remote.public_key());

    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_050),
        Kind::TextNote,
        Vec::new(),
        "resumed after restore",
    );
    let signed = restored
        .sign(signer_unsigned(&unsigned))
        .wait(REMOTE_OPERATION)
        .unwrap();
    let signed = nostr_signed(signed);
    signed.verify().unwrap();
    assert_eq!(signed.pubkey, user.public_key());
    drop(restored);

    let second_session_methods = recorded_session(&seen, "restored");
    assert!(
        !second_session_methods.contains(&"connect".to_string()),
        "restore must never re-send the pairing `connect` RPC: {second_session_methods:?}"
    );
    assert!(second_session_methods.contains(&"get_public_key".to_string()));
    assert!(second_session_methods.contains(&"sign_event".to_string()));
}

/// #571 falsifier: a checkpoint/import whose live `get_public_key` answer
/// does not match the expected identity fails closed -- no signer is ever
/// produced, so it can never be attached under another pubkey.
#[test]
fn from_parts_fails_closed_on_user_public_key_mismatch() {
    let remote = Keys::generate();
    let actual_user = Keys::generate();
    let wrong_expected_user = Keys::generate();
    let (relay, _seen) = spawn_multi_session_signer_relay(remote.clone(), actual_user.clone());

    let checkpoint = nmp_nip46::Nip46SessionCheckpoint {
        client_secret_key: Keys::generate().secret_key().clone(),
        user_public_key: wrong_expected_user.public_key(),
        remote_signer_public_key: remote.public_key(),
        relays: vec![nostr::RelayUrl::parse(&relay).unwrap()],
        origin: Nip46Origin::ClientInitiated,
    };

    let error = Nip46Signer::from_parts(checkpoint, REMOTE_OPERATION).unwrap_err();
    assert_eq!(
        error,
        nmp_nip46::Nip46Error::RestoredIdentityMismatch {
            expected: wrong_expected_user.public_key(),
            actual: actual_user.public_key(),
        }
    );
}

/// #571 falsifier -- the issue's HEADLINE path: a real `nostrconnect://`
/// client-initiated pairing (not `bunker://`), checkpointed and restored
/// through `from_parts` over a SECOND connection with NO re-pairing
/// handshake. Proves `Nip46Invitation::connect`'s generated `client_keys`
/// (the exact identity `checkpoint()` reads out) survives the full round
/// trip, reaches the identical user pubkey, and can still sign.
#[test]
fn client_initiated_checkpoint_then_from_parts_reconnects_without_repairing() {
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
    let remote = Keys::generate();
    let user = Keys::generate();
    let remote_thread = remote.clone();
    let user_thread = user.clone();

    // A two-phase mock over the SAME listener: session 1 completes the
    // `nostrconnect://` pairing handshake (an unsolicited valid `connect`
    // response keyed to the invitation secret, matching
    // `client_invitation_ignores_forged_secret_then_accepts_valid_signer`'s
    // precedent) then answers `get_public_key`/`switch_relays`; session 2
    // (the restore) never receives a `connect` response at all -- it only
    // answers `get_public_key`/`switch_relays`/`sign_event`.
    thread::spawn(move || {
        let remote = remote_thread;
        let user = user_thread;
        let mut paired = false;
        while let Ok((stream, _)) = listener.accept() {
            let mut socket = tungstenite::accept(stream).unwrap();
            let mut subscription_id = None;
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
                    Some("REQ") => {
                        subscription_id = parts.get(1).and_then(Value::as_str).map(str::to_string);
                        if !paired {
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
                        let result = match method {
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
                            other => panic!("unexpected method {other}"),
                        };
                        let response =
                            response_event(&remote, event.pubkey, id, Some(result), None);
                        socket
                            .send(Message::Text(
                                event_frame(subscription_id.as_deref().unwrap(), response).into(),
                            ))
                            .unwrap();
                        // `connect()` only synchronously waits on
                        // `get_public_key` -- `switch_relays` is a
                        // best-effort background request fired afterward
                        // (never awaited by the caller), so ending session 1
                        // here (rather than waiting for a `switch_relays`
                        // that may race with the test's own
                        // checkpoint+drop) matches the real dependency
                        // order and avoids a flaky teardown race.
                        if !paired && method == "get_public_key" {
                            paired = true;
                            break;
                        }
                        if paired && method == "sign_event" {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    let paired_signer = invitation.connect(REMOTE_OPERATION).unwrap();
    assert_eq!(paired_signer.user_public_key(), user.public_key());
    assert_eq!(
        paired_signer.remote_signer_public_key(),
        remote.public_key()
    );

    let checkpoint = paired_signer.checkpoint();
    assert_eq!(checkpoint.origin, Nip46Origin::ClientInitiated);
    assert_eq!(checkpoint.user_public_key, user.public_key());
    assert_eq!(checkpoint.remote_signer_public_key, remote.public_key());
    drop(paired_signer);

    let restored = Nip46Signer::from_parts(checkpoint, REMOTE_OPERATION).unwrap();
    assert_eq!(restored.user_public_key(), user.public_key());
    assert_eq!(restored.remote_signer_public_key(), remote.public_key());

    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_070),
        Kind::TextNote,
        Vec::new(),
        "resumed after client-initiated restore",
    );
    let signed = restored
        .sign(signer_unsigned(&unsigned))
        .wait(REMOTE_OPERATION)
        .unwrap();
    let signed = nostr_signed(signed);
    signed.verify().unwrap();
    assert_eq!(signed.pubkey, user.public_key());
}

#[test]
fn real_bunker_flow_auth_sign_and_crypto_round_trip() {
    let (relay, remote, user, seen) = spawn_mock_remote_signer(false);
    let uri = format!(
        "bunker://{}?relay={}&secret=one-use-secret",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&uri, REMOTE_OPERATION).unwrap();
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
        .sign(signer_unsigned(&unsigned))
        .wait(REMOTE_OPERATION)
        .unwrap();
    let signed = nostr_signed(signed);
    signed.verify().unwrap();
    assert_eq!(signed.pubkey, user.public_key());
    assert_eq!(signed.content, unsigned.content);
    assert_eq!(
        authorization_required_url(&events),
        "https://signer.example/approve"
    );

    let peer = Keys::generate();
    let ciphertext = signer
        .nip44_encrypt(
            SignerPublicKey::new(peer.public_key().to_bytes()),
            "secret payload",
        )
        .wait(REMOTE_OPERATION)
        .unwrap();
    let plaintext = signer
        .nip44_decrypt(
            SignerPublicKey::new(peer.public_key().to_bytes()),
            &ciphertext,
        )
        .wait(REMOTE_OPERATION)
        .unwrap();
    assert_eq!(plaintext, "secret payload");

    let methods = recorded_methods(&seen, 6);

    // #1036: the order claim is only over the RPCs the client AWAITS. The
    // `switch_relays` request is fired best-effort during pairing and never
    // awaited -- it is handed to the session by a spawned task, so under load
    // a later awaited RPC can reach the relay first. Pinning its exact
    // position asserted a scheduling accident, and that -- not any timeout --
    // is what broke on loaded CI.
    let awaited: Vec<&str> = methods
        .iter()
        .map(String::as_str)
        .filter(|method| *method != "switch_relays")
        .collect();
    assert_eq!(
        awaited,
        [
            "connect",
            "get_public_key",
            "sign_event",
            "nip44_encrypt",
            "nip44_decrypt",
        ],
        "the awaited NIP-46 RPCs ran out of order: {methods:?}"
    );

    assert!(
        methods.iter().any(|method| method == "switch_relays"),
        "the session never fired switch_relays: {methods:?}"
    );
}

#[test]
fn valid_but_mutated_signer_event_is_terminal_invalid_response() {
    let (relay, remote, user, _seen) = spawn_mock_remote_signer(true);
    let uri = format!(
        "bunker://{}?relay={}&secret=one-use-secret",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&uri, REMOTE_OPERATION).unwrap();
    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_001),
        Kind::TextNote,
        vec![],
        "the frozen body",
    );
    assert!(matches!(
        signer
            .sign(signer_unsigned(&unsigned))
            .wait(REMOTE_OPERATION),
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

    let signer = invitation.connect(REMOTE_OPERATION).unwrap();
    assert_eq!(signer.remote_signer_public_key(), expected_remote);
    assert_eq!(signer.user_public_key(), expected_user);
}

#[test]
fn client_invitation_reconnect_preamble_binds_the_accepted_signer() {
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
    let remote = Keys::generate();
    let user = Keys::generate();
    let expected_remote = remote.public_key();
    let expected_user = user.public_key();
    let remote_thread = remote.clone();
    let user_thread = user.clone();
    let (reconnect_filter_tx, reconnect_filter_rx) = mpsc::sync_channel(1);

    thread::spawn(move || {
        let (first_stream, _) = listener.accept().unwrap();
        let mut first = tungstenite::accept(first_stream).unwrap();
        let mut subscription_id = None;
        while let Ok(message) = first.read() {
            let Message::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_ref()).unwrap();
            let parts = frame.as_array().unwrap();
            match parts.first().and_then(Value::as_str) {
                Some("REQ") => {
                    subscription_id = parts.get(1).and_then(Value::as_str).map(str::to_string);
                    assert!(
                        parts[2].get("authors").is_none(),
                        "the pre-pairing filter must admit the not-yet-known signer"
                    );
                    let response = response_event(
                        &remote_thread,
                        client,
                        "connect-valid",
                        Some(secret.clone()),
                        None,
                    );
                    first
                        .send(Message::Text(
                            event_frame(subscription_id.as_deref().unwrap(), response).into(),
                        ))
                        .unwrap();
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
                    if method != "get_public_key" {
                        continue;
                    }
                    let response = response_event(
                        &remote_thread,
                        event.pubkey,
                        id,
                        Some(user_thread.public_key().to_hex()),
                        None,
                    );
                    first
                        .send(Message::Text(
                            event_frame(subscription_id.as_deref().unwrap(), response).into(),
                        ))
                        .unwrap();
                    first.close(None).unwrap();
                    break;
                }
                _ => {}
            }
        }

        let (second_stream, _) = listener.accept().unwrap();
        let mut second = tungstenite::accept(second_stream).unwrap();
        while let Ok(message) = second.read() {
            let Message::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_ref()).unwrap();
            let parts = frame.as_array().unwrap();
            if parts.first().and_then(Value::as_str) == Some("REQ") {
                reconnect_filter_tx.send(parts[2].clone()).unwrap();
                break;
            }
        }
    });

    let signer = invitation.connect(REMOTE_OPERATION).unwrap();
    assert_eq!(signer.remote_signer_public_key(), expected_remote);
    assert_eq!(signer.user_public_key(), expected_user);
    let reconnect_filter = reconnect_filter_rx
        .recv_timeout(OBSERVATION)
        .expect("the session reconnects with a refreshed subscription preamble");
    assert_eq!(
        reconnect_filter["authors"],
        json!([expected_remote.to_hex()]),
        "the refreshed reconnect preamble must reject every other author"
    );
}

#[test]
fn dormant_secondary_relay_reconnects_with_the_bound_signer_as_its_first_req() {
    let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let primary_relay = format!("ws://{}", primary_listener.local_addr().unwrap());
    let secondary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let secondary_relay = format!("ws://{}", secondary_listener.local_addr().unwrap());
    let invitation = Nip46Invitation::new(
        vec![
            nostr::RelayUrl::parse(&primary_relay).unwrap(),
            nostr::RelayUrl::parse(&secondary_relay).unwrap(),
        ],
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
    let remote = Keys::generate();
    let user = Keys::generate();
    let expected_remote = remote.public_key();

    let (allow_primary_handshake_tx, allow_primary_handshake_rx) = mpsc::channel();
    let remote_for_primary = remote.clone();
    let user_for_primary = user.clone();
    thread::spawn(move || {
        let (stream, _) = primary_listener.accept().unwrap();
        allow_primary_handshake_rx
            .recv_timeout(OBSERVATION)
            .expect("the test releases the primary relay after the secondary disconnects");
        let mut socket = tungstenite::accept(stream).unwrap();
        let mut subscription_id = None;
        let mut connect_sent = false;
        while let Ok(message) = socket.read() {
            let Message::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_ref()).unwrap();
            let parts = frame.as_array().unwrap();
            match parts.first().and_then(Value::as_str) {
                Some("REQ") => {
                    subscription_id = parts.get(1).and_then(Value::as_str).map(str::to_string);
                    assert!(
                        parts[2].get("authors").is_none(),
                        "pairing begins with a broad filter because the signer is not known yet"
                    );
                    if !connect_sent {
                        connect_sent = true;
                        let response = response_event(
                            &remote_for_primary,
                            client,
                            "connect-valid",
                            Some(secret.clone()),
                            None,
                        );
                        socket
                            .send(Message::Text(
                                event_frame(subscription_id.as_deref().unwrap(), response).into(),
                            ))
                            .unwrap();
                    }
                }
                Some("EVENT") => {
                    let event = Event::from_json(parts[1].to_string()).unwrap();
                    let plaintext = nip44::decrypt(
                        remote_for_primary.secret_key(),
                        &event.pubkey,
                        event.content.as_bytes(),
                    )
                    .unwrap();
                    let request: Value = serde_json::from_str(&plaintext).unwrap();
                    let id = request["id"].as_str().unwrap();
                    let method = request["method"].as_str().unwrap();
                    let result = match method {
                        "get_public_key" => user_for_primary.public_key().to_hex(),
                        "switch_relays" => "null".to_string(),
                        other => panic!("unexpected method {other}"),
                    };
                    let response =
                        response_event(&remote_for_primary, event.pubkey, id, Some(result), None);
                    socket
                        .send(Message::Text(
                            event_frame(subscription_id.as_deref().unwrap(), response).into(),
                        ))
                        .unwrap();
                }
                _ => {}
            }
        }
    });

    let (secondary_broad_tx, secondary_broad_rx) = mpsc::sync_channel(1);
    let (close_secondary_tx, close_secondary_rx) = mpsc::channel();
    let (allow_secondary_reconnect_tx, allow_secondary_reconnect_rx) = mpsc::channel();
    let (secondary_first_req_tx, secondary_first_req_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let (first_stream, _) = secondary_listener.accept().unwrap();
        let mut first = tungstenite::accept(first_stream).unwrap();
        loop {
            let Message::Text(text) = first.read().unwrap() else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_ref()).unwrap();
            let parts = frame.as_array().unwrap();
            if parts.first().and_then(Value::as_str) == Some("REQ") {
                assert!(
                    parts[2].get("authors").is_none(),
                    "the secondary relay also starts before signer ownership is established"
                );
                secondary_broad_tx.send(()).unwrap();
                break;
            }
        }
        close_secondary_rx
            .recv_timeout(OBSERVATION)
            .expect("the test closes the secondary only after observing session availability");
        first.close(None).unwrap();
        drop(first);

        let (second_stream, _) = secondary_listener.accept().unwrap();
        allow_secondary_reconnect_rx
            .recv_timeout(OBSERVATION)
            .expect("the test releases the dormant relay only after pairing completes");
        let mut second = tungstenite::accept(second_stream).unwrap();
        loop {
            let Message::Text(text) = second.read().unwrap() else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_ref()).unwrap();
            let parts = frame.as_array().unwrap();
            if parts.first().and_then(Value::as_str) == Some("REQ") {
                secondary_first_req_tx.send(parts[2].clone()).unwrap();
                break;
            }
        }
    });

    let (events_tx, events_rx) = mpsc::channel();
    let (connect_tx, connect_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = invitation.connect_observed(
            REMOTE_OPERATION,
            std::sync::Arc::new(move |event| {
                let _ = events_tx.send(event);
            }),
        );
        connect_tx.send(result).unwrap();
    });

    let deadline = Instant::now() + OBSERVATION;
    loop {
        match events_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Nip46ConnectionEvent::Available) => break,
            Ok(_) => {}
            Err(error) => panic!("the secondary relay never made the session available ({error})"),
        }
    }
    secondary_broad_rx
        .recv_timeout(OBSERVATION)
        .expect("the secondary relay observed its startup broad REQ");
    close_secondary_tx.send(()).unwrap();
    loop {
        match events_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Nip46ConnectionEvent::Unavailable) => break,
            Ok(_) => {}
            Err(error) => {
                panic!("the session never observed the secondary relay disconnect ({error})")
            }
        }
    }

    allow_primary_handshake_tx.send(()).unwrap();
    let signer = connect_rx
        .recv_timeout(OBSERVATION)
        .expect("the primary relay completes pairing")
        .expect("pairing succeeds");
    assert_eq!(signer.remote_signer_public_key(), expected_remote);

    allow_secondary_reconnect_tx.send(()).unwrap();
    let first_post_binding_req = secondary_first_req_rx
        .recv_timeout(OBSERVATION)
        .expect("the dormant secondary relay reconnects");
    assert_eq!(
        first_post_binding_req["authors"],
        json!([expected_remote.to_hex()]),
        "no broad startup preamble may precede the author-bound reconnect REQ"
    );
}

#[test]
fn unavailable_signer_operation_is_retryable() {
    let (sender, operation) = SignerOp::<String>::pending_channel();
    drop(sender);
    assert_eq!(
        operation.wait(Duration::from_millis(10)),
        Err(nmp_signer::SignerError::Disconnected)
    );
}

// #704: `engine_associated_connection_and_signing_peak_is_six_executor_tasks`
// was deleted. It asserted the per-session `nmp-executor` census reached
// exactly six admitted blocking tasks (connection, session, event-forward,
// switch-relays, mapper, engine waiter). Those tasks are now async futures on a
// runtime that hold no OS thread and expose no census/admission count, so there
// is nothing left to assert. The connect/sign round-trip it also exercised is
// covered by the other mock-relay tests in this file.

#[test]
fn ignored_switch_relays_cannot_keep_the_session_alive_after_signer_drop() {
    let (relay, remote, _user, closed, _sign_seen) = spawn_unresponsive_remote_signer();
    let uri = format!(
        "bunker://{}?relay={}&secret=ignored-switch",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );
    let signer = Nip46Signer::connect_bunker(&uri, REMOTE_OPERATION).unwrap();

    drop(signer);

    closed
        .recv_timeout(OBSERVATION)
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
    let signer = Nip46Signer::connect_bunker(&uri, REMOTE_OPERATION).unwrap();
    let unsigned = UnsignedEvent::new(
        user.public_key(),
        Timestamp::from(1_700_000_002),
        Kind::TextNote,
        Vec::new(),
        "never answered",
    );

    for _ in 0..64 {
        drop(signer.sign(signer_unsigned(&unsigned)));
    }
    thread::sleep(Duration::from_millis(100));

    assert_eq!(
        signer
            .sign(signer_unsigned(&unsigned))
            .wait(Duration::from_millis(50)),
        Err(SignerError::Timeout),
        "the next request is admitted; it is not rejected by leaked pending slots",
    );
}
