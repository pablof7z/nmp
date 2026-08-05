//! The provenance grant has to survive the whole way down (#1251).
//!
//! Two other suites prove the halves in isolation: `nmp::nip65::provenance`
//! proves the engine decides admission with the author's identity, and
//! `nmp_transport::pool::connect`'s
//! `our_own_declaration_reaches_a_local_relay_with_no_allowlist` proves the
//! socket honours a declaration with an empty allowlist. Neither proves they
//! are actually wired to each other.
//!
//! This one does, against a real loopback relay over a real websocket, with
//! `allowed_local_relay_hosts` left EMPTY. Before #1251 the same configuration
//! reached the dial guard and was refused there, so the app was told its own
//! configured relay was a destination and then could never reach it — routing
//! and the socket answering one provenance question two different ways.

use std::collections::BTreeSet;
use std::time::Duration;

use nmp::mechanism::runtime::FifoReceiver;
use nmp::{
    Engine, EngineConfig, Identity, RelayState, WriteFact, WriteIntent, WritePayload, WriteRouting,
};
use nmp_local_signer::LocalKeySigner;
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
            }) => {
                acked.insert(relay.to_string());
            }
            Ok(status) => seen.push(status),
            Err(error) => panic!("the receipt stream ended early ({error:?}); saw {seen:?}"),
        }
    }
    acked
}

/// `ROUTING-ADMISSION-004`, end to end: the app declares a loopback relay and
/// nothing else. No local host is on the admission allowlist, so every
/// address-only gate in the stack would refuse this dial.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_declared_loopback_relay_is_reached_with_an_empty_allowlist() {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let url = relay.url.clone();

    let engine = Engine::new(EngineConfig {
        app_relays: vec![url.to_string()],
        // Deliberately empty. The app naming the relay IS the declaration;
        // needing to name it a second time here was the incoherence.
        allowed_local_relay_hosts: Vec::new(),
        tor_reachable: false,
        ..EngineConfig::default()
    })
    .expect("an engine with one app relay builds");

    let keys = Keys::generate();
    engine
        .set_active_account(Some(keys.public_key()))
        .expect("the account activates");
    engine
        .add_signer(
            LocalKeySigner::from_secret_bytes(keys.secret_key().as_secret_bytes())
                .expect("fixture keys are valid secp256k1 scalars"),
        )
        .expect("a local signer registers");

    let tracked = engine
        .publish(WriteIntent {
            payload: note("reaching the relay this app declared"),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("the write is accepted");

    let acked = acked_relays(&tracked.statuses, SETTLE);
    assert!(
        acked.contains(&url.to_string()),
        "an app relay on loopback must be REACHED, not merely routed to, on the \
         app's own declaration alone; acked: {acked:?}"
    );
}
