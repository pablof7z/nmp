//! A rebind is not a restart.
//!
//! Every "the relay comes back" fixture this crate replaces built an EMPTY
//! relay: a fresh in-memory database, so a relay that came back had forgotten
//! everything. "The relay gained events while the client was disconnected"
//! was therefore not a sentence the Rust tree could say, and it is the
//! assertion an offline scenario is entirely built around.

use std::time::Duration;

use nostr::Keys;

use crate::clock::PRODUCTION_RECONNECT_FLOOR;
use crate::fixtures::{
    engine_against, kind1_by, note, publish_note, publishing_engine, rows_within, store_path,
    QUIET, SETTLE,
};
use crate::probe::{black_hole, probe, PortVerdict};
use crate::scenario::Report;
use crate::{RelayLab, RelayStore, Script};

/// The scenario the tree could not express: a feed is open, the port is
/// severed, a SECOND WRITER adds events while the relay is dead, it comes
/// back on the same port, and the events arrive.
///
/// The second writer is a sidecar over the durable file, not a second engine:
/// the relay is not running, so there is nothing to publish to. That is the
/// whole point of the store being a path.
pub async fn gained_while_dead(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("gained-while-dead");
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
    report.eq(
        "the pre-outage event arrives",
        rows_within(&subscription, Duration::from_secs(3)).len(),
        1,
    );

    let port = relay.disconnect().await;
    let addr = format!("127.0.0.1:{port}").parse().expect("loopback address");
    report.eq(
        "the port is definitively shut -- refused by the kernel, not merely \
         slow to answer",
        probe(addr, Duration::from_secs(1)),
        PortVerdict::Refused,
    );

    let during_outage = note(&author, "written by a sidecar during the outage", 1_700_000_500);
    store.append([during_outage.clone()]);
    report.eq(
        "the durable store gained an event with NO relay running",
        store.read().len(),
        2,
    );

    let returning = if mutation == Some("return-volatile") {
        Script::new()
    } else {
        Script::new().durable(&path)
    };
    let relay = RelayLab::start_on_port(port, returning).await;
    report.eq(
        "a restart is a restart: the relay came back with its contents",
        relay.held().len(),
        2,
    );

    let budget = PRODUCTION_RECONNECT_FLOOR * 3;
    report.that(
        format!("NMP reconnects and replays within {budget:?}"),
        relay
            .wire()
            .wait_for(budget, |record| !record.reqs().is_empty())
            .await,
        relay.record().reqs().len(),
    );
    let rows = rows_within(&subscription, Duration::from_secs(6));
    report.that(
        "and the event written while it was dead reaches the app",
        rows.iter().any(|row| row.id() == during_outage.id),
        rows.len(),
    );

    let _ = std::fs::remove_file(&path);
    report
}

/// A durable relay keeps what it acknowledged across a restart, and the
/// in-memory control comes back empty.
pub async fn acknowledged_survives(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("acknowledged-survives");
    let author = Keys::generate();
    let path = store_path("acknowledged-survives");
    let _ = std::fs::remove_file(&path);

    let relay = RelayLab::start(Script::new().durable(&path)).await;
    let engine = publishing_engine(&relay, &author);
    let _receipt = publish_note(&engine, "a write that must outlive the relay");
    report.that(
        "the write is acknowledged",
        relay
            .wire()
            .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
            .await,
        relay.record().oks_sent(),
    );
    let port = relay.disconnect().await;

    let reopened = RelayLab::start_on_port(port, Script::new().durable(&path)).await;
    report.eq(
        "the acknowledged write is still there after a restart",
        reopened.held().len(),
        1,
    );
    drop(reopened);

    let volatile = RelayLab::start(Script::new()).await;
    let volatile_engine = publishing_engine(&volatile, &author);
    let _ = publish_note(&volatile_engine, "a write nothing will remember");
    let _ = volatile
        .wire()
        .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
        .await;
    let volatile_port = volatile.disconnect().await;
    let rebound = RelayLab::start_on_port(volatile_port, Script::new()).await;
    report.that(
        "the control: an in-memory relay comes back EMPTY, which is exactly \
         what made 'gained events while disconnected' unsayable",
        rebound.held().is_empty(),
        rebound.held().len(),
    );

    let _ = std::fs::remove_file(&path);
    report
}

/// A refused port and a black hole are different observations, told apart by
/// errno rather than by elapsed time.
///
/// The timings are the claim's teeth: a refusal returns far inside the budget
/// because something answered, and the black hole consumes the whole budget
/// because nothing did.
pub async fn refusal_vs_black_hole(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("refusal-vs-black-hole");
    let relay = RelayLab::start(Script::new()).await;
    let addr = format!("127.0.0.1:{}", relay.port())
        .parse()
        .expect("loopback address");
    report.eq(
        "a listening relay is Open",
        probe(addr, Duration::from_secs(1)),
        PortVerdict::Open,
    );

    let port = relay.disconnect().await;
    let addr = format!("127.0.0.1:{port}").parse().expect("loopback address");

    let started = std::time::Instant::now();
    let refused = probe(addr, Duration::from_secs(5));
    let refusal_took = started.elapsed();
    report.eq("a severed port is Refused", refused.clone(), PortVerdict::Refused);
    report.that(
        "a refusal is immediate, because something answered",
        refused.is_definitely_shut() && refusal_took < Duration::from_millis(500),
        refusal_took,
    );

    let started = std::time::Instant::now();
    let nothing = probe(black_hole(), Duration::from_millis(600));
    let wait = started.elapsed();
    report.that(
        "a dropped SYN is NOT a refusal, and is never reported as one",
        matches!(nothing, PortVerdict::NoAnswer { .. }) && !nothing.is_definitely_shut(),
        format!("{nothing:?}"),
    );
    report.that(
        "and it cost the whole budget, because nothing ever answered",
        wait >= Duration::from_millis(500),
        wait,
    );
    report
}

/// A sidecar READS the durable contents during an outage, so a scenario can
/// assert what a relay holds when nothing is serving it.
pub async fn sidecar_reads_during_outage(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("sidecar-reads-during-outage");
    let author = Keys::generate();
    let path = store_path("sidecar-read");
    let _ = std::fs::remove_file(&path);

    let relay = RelayLab::start(Script::new().durable(&path)).await;
    let engine = publishing_engine(&relay, &author);
    let _ = publish_note(&engine, "one durable write");
    let _ = relay
        .wire()
        .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
        .await;
    relay.wire().wait_quiet(QUIET, SETTLE).await;
    let port = relay.disconnect().await;

    let addr = format!("127.0.0.1:{port}").parse().expect("loopback address");
    report.that(
        "nothing is serving the store",
        probe(addr, Duration::from_secs(1)).is_definitely_shut(),
        "refused",
    );
    let store = RelayStore::at(&path);
    let held = store.read();
    report.that(
        "yet its contents are readable, by path, with the relay gone -- and \
         they are the events themselves, not a count",
        held.len() == 1 && held[0].content == "one durable write",
        held.iter().map(|e| e.content.clone()).collect::<Vec<_>>(),
    );

    let _ = std::fs::remove_file(&path);
    report
}
