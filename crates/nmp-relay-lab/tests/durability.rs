//! A rebind is not a restart.
//!
//! Every "the relay comes back" fixture this crate replaces built an EMPTY
//! relay: a fresh in-memory database, so a relay that came back had forgotten
//! everything it ever held. "The relay gained events while the client was
//! disconnected" was therefore not a sentence the Rust tree could say, and it
//! is the assertion an offline scenario is entirely built around.

mod support;

use std::time::Duration;

use nmp_relay_lab::clock::PRODUCTION_RECONNECT_FLOOR;
use nmp_relay_lab::probe::{black_hole, probe, PortVerdict};
use nmp_relay_lab::{RelayLab, RelayStore, Script};
use nostr::Keys;
use support::{engine_against, kind1_by, note, rows_within, QUIET, SETTLE};

fn store_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nmp-relay-lab-{}-{}",
        name,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("the scenario's own directory");
    dir.join("relay.jsonl")
}

/// The scenario the tree could not express: a feed is open, the relay's port
/// is severed, a SECOND WRITER adds events while it is dead, the relay comes
/// back on the same port, and the events arrive.
///
/// The second writer is a sidecar over the durable file, not a second engine:
/// the relay is not running, so there is nothing to publish to. That is the
/// whole point of the store being a path.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_gained_events_while_it_was_dead_delivers_them_on_return() {
    let author = Keys::generate();
    let path = store_path("gained-while-dead");
    let _ = std::fs::remove_file(&path);
    let store = RelayStore::at(&path);
    store.append([note(&author, "held before the outage", 1_700_000_000)]);

    let relay = RelayLab::start(Script::new().durable(&path)).await;
    let engine = engine_against(&relay);
    let subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");
    assert_eq!(
        rows_within(&subscription, Duration::from_secs(3)).len(),
        1,
        "the pre-outage event arrives"
    );

    // The relay goes away. The port is severed, not merely idle.
    let port = relay.disconnect().await;
    let addr = format!("127.0.0.1:{port}").parse().expect("loopback address");
    assert_eq!(
        probe(addr, Duration::from_secs(1)),
        PortVerdict::Refused,
        "the port is definitively shut -- refused by the kernel, not merely \
         slow to answer"
    );

    // A sidecar writes to the relay's durable store while nothing is serving
    // it. This is the half no in-memory fixture can do at all.
    let during_outage = note(&author, "written by a sidecar during the outage", 1_700_000_500);
    store.append([during_outage.clone()]);
    assert_eq!(
        store.read().len(),
        2,
        "the durable store gained an event with no relay running"
    );

    // Back on the same address, holding what it held plus what it gained.
    let relay = RelayLab::start_on_port(port, Script::new().durable(&path)).await;
    assert_eq!(
        relay.held().len(),
        2,
        "a restart is a restart: the relay came back with its contents"
    );

    let budget = PRODUCTION_RECONNECT_FLOOR * 3;
    assert!(
        relay
            .wire()
            .wait_for(budget, |record| !record.reqs().is_empty())
            .await,
        "NMP reconnects and replays its subscription within {budget:?}"
    );
    let rows = rows_within(&subscription, Duration::from_secs(6));
    assert!(
        rows.iter().any(|row| row.id() == during_outage.id),
        "and the event written while it was dead reaches the app: {} rows",
        rows.len()
    );

    let _ = std::fs::remove_file(&path);
}

/// A durable relay keeps what it acknowledged across a full restart. The
/// falsifier for the store itself: an in-memory relay on the same script
/// comes back empty.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_durable_relay_acknowledges_survives_a_restart_and_memory_does_not() {
    let author = Keys::generate();
    let path = store_path("acknowledged-survives");
    let _ = std::fs::remove_file(&path);

    let relay = RelayLab::start(Script::new().durable(&path)).await;
    let engine = support::publishing_engine(&relay, &author);
    let _receipt = support::publish_note(&engine, "a write that must outlive the relay");
    assert!(
        relay
            .wire()
            .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
            .await,
        "the write is acknowledged"
    );
    let port = relay.disconnect().await;

    let reopened = RelayLab::start_on_port(port, Script::new().durable(&path)).await;
    assert_eq!(
        reopened.held().len(),
        1,
        "the acknowledged write is still there after a restart"
    );
    drop(reopened);

    // The control: the same sequence without a durable store forgets.
    let volatile = RelayLab::start(Script::new()).await;
    let volatile_engine = support::publishing_engine(&volatile, &author);
    let _ = support::publish_note(&volatile_engine, "a write nothing will remember");
    assert!(
        volatile
            .wire()
            .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
            .await
    );
    let volatile_port = volatile.disconnect().await;
    let rebound = RelayLab::start_on_port(volatile_port, Script::new()).await;
    assert!(
        rebound.held().is_empty(),
        "an in-memory relay comes back empty, which is exactly what made \
         'gained events while disconnected' unsayable"
    );

    let _ = std::fs::remove_file(&path);
}

/// A refused port and a black hole are different observations, and the
/// difference is errno rather than elapsed time.
///
/// The timings are the assertion's teeth: a refusal returns far inside the
/// budget because something answered, and the black hole consumes the whole
/// budget because nothing did.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_and_a_black_hole_are_told_apart_by_errno_not_by_waiting() {
    let relay = RelayLab::start(Script::new()).await;
    let addr = format!("127.0.0.1:{}", relay.port())
        .parse()
        .expect("loopback address");
    assert_eq!(probe(addr, Duration::from_secs(1)), PortVerdict::Open);

    let port = relay.disconnect().await;
    let addr = format!("127.0.0.1:{port}").parse().expect("loopback address");

    let started = std::time::Instant::now();
    let refused = probe(addr, Duration::from_secs(5));
    let refusal_took = started.elapsed();
    assert_eq!(refused, PortVerdict::Refused);
    assert!(refused.is_definitely_shut());
    assert!(
        refusal_took < Duration::from_millis(500),
        "a refusal is immediate because something answered; took {refusal_took:?}"
    );

    let started = std::time::Instant::now();
    let nothing = probe(black_hole(), Duration::from_millis(600));
    let wait = started.elapsed();
    assert!(
        matches!(nothing, PortVerdict::NoAnswer { .. }),
        "a dropped SYN is not a refusal: {nothing:?}"
    );
    assert!(
        !nothing.is_definitely_shut(),
        "and must never be reported as one -- that is the whole distinction"
    );
    assert!(
        wait >= Duration::from_millis(500),
        "it cost the whole budget, because nothing ever answered; took {wait:?}"
    );
}

/// A sidecar can READ the durable contents during an outage, so a scenario
/// can assert what a relay holds at a moment when nothing is serving it.
#[tokio::test(flavor = "multi_thread")]
async fn a_sidecar_reads_the_durable_contents_while_nothing_is_serving_them() {
    let author = Keys::generate();
    let path = store_path("sidecar-read");
    let _ = std::fs::remove_file(&path);

    let relay = RelayLab::start(Script::new().durable(&path)).await;
    let engine = support::publishing_engine(&relay, &author);
    let _ = support::publish_note(&engine, "one durable write");
    assert!(
        relay
            .wire()
            .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
            .await
    );
    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let port = relay.disconnect().await;

    let addr = format!("127.0.0.1:{port}").parse().expect("loopback address");
    assert_eq!(
        probe(addr, Duration::from_secs(1)),
        PortVerdict::Refused,
        "nothing is serving the store"
    );
    let store = RelayStore::at(&path);
    assert_eq!(
        store.read().len(),
        1,
        "yet its contents are readable, by path, with the relay gone"
    );
    assert_eq!(
        store.read()[0].content,
        "one durable write",
        "and they are the events themselves, not a count"
    );

    let _ = std::fs::remove_file(&path);
}
