//! Loopback relay destinations are ordinary Nostr destinations (#1429).
//! This suite reaches one through the supported engine facade over a real
//! websocket, with no destination-policy configuration surface.

use std::collections::BTreeSet;
use std::time::Duration;

use nmp::{
    Engine, EngineConfig, Identity, RelayState, WriteFact, WriteIntent, WritePayload, WriteRouting,
};
use nmp_runtime::FifoReceiver;
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{Keys, Kind};

const SETTLE: Duration = Duration::from_secs(30);

fn note(content: &str) -> WritePayload {
    WritePayload::Event(nmp::EventBuilder {
        kind: Kind::TextNote,
        tags: Vec::new(),
        content: content.to_string(),
        created_at: None,
    })
}

fn acked_relays(receipts: &FifoReceiver<WriteFact>, budget: Duration) -> BTreeSet<String> {
    let deadline = std::time::Instant::now() + budget;
    let mut acked = BTreeSet::new();
    let mut seen = Vec::new();
    while acked.is_empty() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("no relay acknowledged the write; saw {seen:?}");
        }
        match receipts.recv_timeout(remaining) {
            Ok(WriteFact::Relay {
                relay,
                state: RelayState::Published,
                ..
            }) => {
                acked.insert(relay.to_string());
            }
            Ok(status) => seen.push(status),
            Err(error) => panic!("the receipt stream ended early ({error:?}); saw {seen:?}"),
        }
    }
    acked
}

/// End to end: the app declares a loopback relay and nothing else, then the
/// real transport reaches it and receives a publish acknowledgement.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_declared_loopback_relay_is_reached_without_opt_in() {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let url = relay.url.clone();

    let engine = Engine::new(EngineConfig {
        app_relays: vec![url.to_string()],
        ..EngineConfig::default()
    })
    .expect("an engine with one app relay builds");

    let keys = Keys::generate();
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), true)
        .expect("the account and local provider register");

    let tracked = engine
        .publish(WriteIntent {
            payload: note("reaching the relay this app declared"),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
        })
        .expect("the write is accepted");

    let acked = acked_relays(&tracked.statuses, SETTLE);
    assert!(
        acked.contains(&url.to_string()),
        "an app relay on loopback must be reached like any other target; acked: {acked:?}"
    );
}
