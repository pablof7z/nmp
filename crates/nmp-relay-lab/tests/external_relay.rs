//! NMP against a relay nobody in this workspace wrote, in a process nobody
//! here shares.
//!
//! Requires the `external-relay` feature and a `nostr-rs-relay` binary; see
//! `nmp_relay_lab::external`. Deliberately a feature rather than a runtime
//! skip -- a test that no-ops when a binary is missing is a green run that
//! proves nothing.

mod support;

use std::time::Duration;

use nmp_relay_lab::external::ExternalRelay;
use nmp_relay_lab::probe::probe;
use nostr::Keys;
use support::{kind1_by_on, publish_note, rows_within, SETTLE};

fn engine_for(url: &nostr::RelayUrl) -> nmp::Engine {
    nmp::Engine::new(nmp::EngineConfig {
        app_relays: vec![url.to_string()],
        ..nmp::EngineConfig::default()
    })
    .expect("an engine with one app relay builds")
}

/// The interoperability question, end to end: NMP publishes to an upstream
/// relay, and NMP reads it back. Neither half is answered by a fixture
/// written alongside NMP, because such a fixture agrees with NMP by
/// construction -- including wherever both are wrong.
#[tokio::test(flavor = "multi_thread")]
async fn nmp_publishes_to_a_third_party_relay_and_reads_it_back() {
    let relay = ExternalRelay::start();
    let author = Keys::generate();

    let engine = engine_for(relay.url());
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("the account registers");
    let receipt = publish_note(&engine, "written to a relay nobody here wrote");
    let facts = support::relay_facts(&receipt.statuses, SETTLE);
    assert!(
        facts.contains(&nmp::RelayState::Published),
        "an upstream relay acknowledged the write: {facts:?}"
    );

    let subscription = engine
        .observe(kind1_by_on(&author, relay.url()), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(10));
    assert_eq!(
        rows.len(),
        1,
        "and serves it back on a real REQ/EOSE round trip"
    );
}

/// A real process, really killed, whose store really survived.
///
/// The relay is SIGKILLed -- no shutdown, no flush, no `Drop` -- and a second
/// process is started on the same data directory and the same port. What it
/// serves came off SQLite written by a process that no longer exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_sigkilled_relay_comes_back_holding_what_it_acknowledged() {
    let dir = std::env::temp_dir().join(format!("nmp-relay-lab-ext-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut relay = ExternalRelay::start_in(&dir);
    let port = relay.port();
    let url = relay.url().clone();
    let author = Keys::generate();

    let engine = engine_for(&url);
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("the account registers");
    let receipt = publish_note(&engine, "must outlive the process that took it");
    let facts = support::relay_facts(&receipt.statuses, SETTLE);
    assert!(facts.contains(&nmp::RelayState::Published), "{facts:?}");

    let pid = relay.pid().expect("the relay has a pid");
    relay.kill();
    assert!(
        probe(relay.addr(), Duration::from_secs(1)).is_definitely_shut(),
        "pid {pid} is gone and its port is refused by the kernel"
    );

    // A sidecar can see the durable contents with nothing serving them.
    let db = dir.join("nostr.db");
    assert!(
        db.is_file() && db.metadata().expect("db metadata").len() > 0,
        "the relay's SQLite store is on disk at {}",
        db.display()
    );

    // A DIFFERENT process, same store, same port.
    let relay = ExternalRelay::start_in_on_port(&dir, port);
    assert_ne!(
        relay.pid(),
        Some(pid),
        "this is a new process, not the old one"
    );

    let reader = engine_for(&url);
    let subscription = reader
        .observe(kind1_by_on(&author, &url), None)
        .expect("the observation opens");
    let rows = rows_within(&subscription, Duration::from_secs(10));
    assert_eq!(
        rows.len(),
        1,
        "and the write survived a SIGKILL and a process boundary"
    );

    drop(relay);
    let _ = std::fs::remove_dir_all(&dir);
}
