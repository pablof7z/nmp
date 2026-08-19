//! NMP against a relay nobody in this workspace wrote, in a process nobody
//! here shares.
//!
//! A fixture written alongside the client agrees with the client by
//! construction, including wherever both are wrong. These are the only
//! scenarios here that can disagree.

use std::time::Duration;

use nostr::Keys;

use crate::external::{discover, ExternalRelay};
use crate::fixtures::{kind_by_on, publish_note, relay_facts, rows_within, SETTLE};
use crate::probe::probe;
use crate::scenario::Report;

fn engine_for(url: &nostr::RelayUrl) -> nmp::Engine {
    nmp::Engine::new(nmp::EngineConfig {
        app_relays: vec![url.to_string()],
        ..nmp::EngineConfig::default()
    })
    .expect("an engine with one app relay builds")
}

fn missing() -> Option<String> {
    discover().is_none().then(|| {
        "no relay binary: set $NMP_RELAY_LAB_RELAY_BIN, or \
         `cargo install nostr-rs-relay` (without --locked)"
            .to_string()
    })
}

/// The interoperability question, end to end.
pub async fn interop(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("external-interop");
    if let Some(why) = missing() {
        report.skip(why);
        return report;
    }

    let relay = ExternalRelay::start();
    let author = Keys::generate();
    let engine = engine_for(relay.url());
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("the account registers");

    let receipt = publish_note(&engine, "written to a relay nobody here wrote");
    let facts = relay_facts(&receipt.statuses, SETTLE);
    report.that(
        "an upstream relay acknowledged the write",
        facts.contains(&nmp::RelayState::Published),
        &facts,
    );

    let subscription = engine
        .observe(kind_by_on(&author, relay.url(), 1), None)
        .expect("the observation opens");
    report.eq(
        "and serves it back on a real REQ/EOSE round trip",
        rows_within(&subscription, Duration::from_secs(10)).len(),
        1,
    );
    report
}

/// A real process, really killed, whose store really survived.
pub async fn sigkill_restart(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("external-sigkill-restart");
    if let Some(why) = missing() {
        report.skip(why);
        return report;
    }

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
    let facts = relay_facts(&receipt.statuses, SETTLE);
    report.that(
        "the upstream relay acknowledged it",
        facts.contains(&nmp::RelayState::Published),
        &facts,
    );

    let pid = relay.pid().expect("the relay has a pid");
    relay.kill();
    report.that(
        "the process is gone and its port is refused by the kernel",
        probe(relay.addr(), Duration::from_secs(1)).is_definitely_shut(),
        pid,
    );

    let db = dir.join("nostr.db");
    report.that(
        "its SQLite store is on disk, readable with nothing serving it",
        db.is_file() && db.metadata().map(|m| m.len()).unwrap_or(0) > 0,
        db.metadata().map(|m| m.len()).unwrap_or(0),
    );

    let relay = ExternalRelay::start_in_on_port(&dir, port);
    report.that(
        "a DIFFERENT process now serves the same store on the same port",
        relay.pid() != Some(pid),
        (pid, relay.pid()),
    );

    let reader = engine_for(&url);
    let subscription = reader
        .observe(kind_by_on(&author, &url, 1), None)
        .expect("the observation opens");
    report.eq(
        "and the write survived a SIGKILL and a process boundary",
        rows_within(&subscription, Duration::from_secs(10)).len(),
        1,
    );

    drop(relay);
    let _ = std::fs::remove_dir_all(&dir);
    report
}
