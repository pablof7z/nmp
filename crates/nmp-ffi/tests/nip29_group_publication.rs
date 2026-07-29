//! #1015 native capstone: the opaque FFI Group reaches the real wire through
//! the one tracked Engine lifecycle. The relay's received event, not an
//! engine-side intent inspection, proves the context is signed and the
//! retained host is the only route.

use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nmp_ffi::nip29::NmpGroup;
use nmp_ffi::types::{FfiEventBuilder, FfiWriteStatus};
use nostr::{Event, JsonUtil};
use tungstenite::Message;

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";

struct AckRelay {
    url: String,
    received: Receiver<Event>,
    thread: thread::JoinHandle<()>,
}

fn ack_relay() -> AckRelay {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind group relay");
    let address = listener.local_addr().unwrap();
    let (event_tx, received) = mpsc::channel();
    let thread = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("group relay accepts");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).expect("websocket handshake");
        loop {
            match socket.read().expect("group relay reads a client message") {
                Message::Text(text) => {
                    let value: serde_json::Value =
                        serde_json::from_str(text.as_str()).expect("valid Nostr client JSON");
                    if value.get(0).and_then(serde_json::Value::as_str) != Some("EVENT") {
                        continue;
                    }
                    let event = Event::from_json(value[1].to_string())
                        .expect("EVENT carries a canonical signed event");
                    event.verify().expect("wire event signature verifies");
                    event_tx.send(event.clone()).unwrap();
                    let ack = serde_json::json!([
                        "OK",
                        event.id.to_hex(),
                        true,
                        "accepted by native group capstone"
                    ]);
                    socket.send(Message::Text(ack.to_string().into())).unwrap();
                    return;
                }
                Message::Ping(payload) => {
                    socket.send(Message::Pong(payload)).unwrap();
                }
                Message::Close(_) => return,
                _ => {}
            }
        }
    });
    AckRelay {
        url: format!("ws://{address}"),
        received,
        thread,
    }
}

struct BystanderRelay {
    url: String,
    address: std::net::SocketAddr,
    connected: Receiver<()>,
    cancel: Sender<()>,
    thread: thread::JoinHandle<()>,
}

fn bystander_relay() -> BystanderRelay {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind bystander relay");
    let address = listener.local_addr().unwrap();
    let (connected_tx, connected) = mpsc::channel();
    let (cancel, cancel_rx) = mpsc::channel();
    let thread = thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("bystander accept wakes");
        if cancel_rx.try_recv().is_err() {
            connected_tx.send(()).unwrap();
        }
    });
    BystanderRelay {
        url: format!("ws://{address}"),
        address,
        connected,
        cancel,
        thread,
    }
}

async fn next_status(receipt: &nmp_ffi::facade::NmpReceiptStream) -> Option<FfiWriteStatus> {
    tokio::time::timeout(Duration::from_secs(10), receipt.next())
        .await
        .expect("receipt advances within the capstone bound")
        .expect("receipt pull is not concurrent")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_group_write_reaches_only_its_retained_host_with_one_signed_context() {
    let group_relay = ack_relay();
    let bystander = bystander_relay();
    let engine = NmpEngine::new(NmpEngineConfig {
        fallback_relays: vec![bystander.url.clone()],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("engine builds");
    let registration = engine
        .add_account(TEST_SECRET_KEY_HEX.to_string())
        .expect("test signer registers");
    engine
        .set_active_account(Some(registration.public_key()))
        .expect("test signer activates");
    let group = NmpGroup::new(group_relay.url.clone(), "photographers".to_string())
        .expect("group retains a valid host");

    let receipt = group
        .publish(
            engine.clone(),
            FfiEventBuilder {
                kind: 27_272,
                tags: vec![vec!["subject".to_string(), "sunset".to_string()]],
                content: "first light".to_string(),
                created_at: Some(1_700_000_000),
            },
            Some("native-wire-capstone".to_string()),
        )
        .expect("group contextualizes and enters tracked publish");

    let mut routed = None;
    let mut signed_id = None;
    let mut acked = false;
    while let Some(status) = next_status(&receipt).await {
        match status {
            FfiWriteStatus::Routed { relays } => routed = Some(relays),
            FfiWriteStatus::Signed { event_id } => signed_id = Some(event_id),
            FfiWriteStatus::Acked { relay } => {
                assert_eq!(relay, group_relay.url);
                acked = true;
            }
            FfiWriteStatus::Rejected { relay, reason } => {
                panic!("group host {relay} rejected capstone event: {reason}")
            }
            FfiWriteStatus::Failed { reason } => {
                panic!("native group publication failed: {reason}")
            }
            _ => {}
        }
    }
    assert!(acked, "ordinary receipt must carry the host ACK");
    assert_eq!(
        routed,
        Some(vec![group_relay.url.clone()]),
        "the retained Group host is the complete route"
    );

    let event = group_relay
        .received
        .recv_timeout(Duration::from_secs(1))
        .expect("the retained host records the actual signed wire event");
    let context_rows: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
        .collect();
    assert_eq!(context_rows.len(), 1, "exactly one group context is signed");
    assert_eq!(
        context_rows[0].as_slice(),
        &["h".to_string(), "photographers".to_string()]
    );
    assert_eq!(signed_id, Some(event.id.to_hex()));
    assert_eq!(event.kind.as_u16(), 27_272, "Group stays kind-blind");
    assert_eq!(
        event.tags[0].as_slice(),
        &["subject".to_string(), "sunset".to_string()],
        "foreign schema bytes survive before the appended context"
    );

    assert!(
        bystander.connected.try_recv().is_err(),
        "the configured fallback relay receives no group connection"
    );

    engine.shutdown();
    group_relay.thread.join().unwrap();
    bystander.cancel.send(()).unwrap();
    TcpStream::connect(bystander.address).expect("wake bystander accept for teardown");
    bystander.thread.join().unwrap();
}
