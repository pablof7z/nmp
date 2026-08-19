//! Stating what time the engine is running at.
//!
//! This module used to record a finding: there was no door. `Engine::clock()`
//! had been `#[doc(hidden)]` behind an `unstable-mechanism` feature that also
//! pulled a test-fixture crate, and when that feature was deleted with the
//! testkits the door went with it -- leaving the mechanism reachable only by
//! abandoning `nmp::Engine` for `Handle`.
//!
//! `EngineConfig::clock` is that finding closed. It is a public field holding
//! a public `EngineClock`, with no feature gate and no `doc(hidden)`, in place
//! BEFORE store recovery rather than settable only afterwards. These
//! scenarios are what the finding asked for, now that they can be written.
//!
//! The transport's clocks stayed shut, deliberately: reconnect backoff is
//! `Instant` and the background-gap detector is `SystemTime`, and one knob
//! for both would make a reconnect scenario lie -- a reconnect that "took"
//! thirty days of stated time and zero real time never exercised the backoff.

use std::time::Duration;

use nmp::{Engine, EngineClock, EngineConfig, Timestamp};
use nostr::Keys;

use crate::fixtures::{kind1_by, publish_note, rows_within, QUIET, SETTLE};
use crate::scenario::Report;
use crate::{Ev, RelayLab, Reply, Script};

fn engine_at(relay: &RelayLab, keys: &Keys, clock: &EngineClock) -> Engine {
    let engine = Engine::new(EngineConfig {
        app_relays: vec![relay.url().to_string()],
        clock: clock.clone(),
        ..EngineConfig::default()
    })
    .expect("an engine with a stated clock builds");
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), true)
        .expect("the account registers");
    engine
}

/// A stated instant reaches the wire. The engine stamps a write it is asked
/// to time itself with the clock the app handed it at construction.
pub async fn stated_instant(mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("stated-instant");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new()).await;

    let clock = EngineClock::new();
    let stated = Timestamp::from_secs(1_600_000_000);
    if mutation != Some("never-state-it") {
        clock.set(stated);
    }
    let engine = engine_at(&relay, &author, &clock);

    let _receipt = publish_note(&engine, "written at a stated instant");
    report.that(
        "the write reaches the relay and is acknowledged",
        relay
            .wire()
            // The OK, not the EVENT: the reply is emitted AFTER the ingest
            // step, so waiting on it is what makes `held()` settled.
            .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
            .await,
        relay.record().oks_sent().len(),
    );

    let held = relay.held();
    report.eq("one write reached the relay", held.len(), 1);
    report.eq(
        "and it carries the STATED instant, not the real one",
        held[0].created_at,
        stated,
    );
    report
}

/// The clock may move BACKWARD. A device whose clock is behind is a case the
/// write plane has to survive; `EngineClock::set` documents the backward jump
/// as deliberate.
pub async fn backward_jump(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("backward-jump");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new()).await;

    let clock = EngineClock::new();
    clock.set(Timestamp::from_secs(1_700_000_000));
    let engine = engine_at(&relay, &author, &clock);

    let _first = publish_note(&engine, "written on a clock that is ahead");
    let _ = relay
        .wire()
        .wait_for(SETTLE, |record| record.oks_sent().len() == 1)
        .await;

    clock.set(Timestamp::from_secs(1_600_000_000));
    let _second = publish_note(&engine, "written after the clock went backwards");
    report.that(
        "both writes are acknowledged",
        relay
            .wire()
            .wait_for(SETTLE, |record| record.oks_sent().len() == 2)
            .await,
        relay.record().oks_sent().len(),
    );

    let mut held = relay.held();
    held.sort_by_key(|event| event.created_at);
    report.eq("two writes reached the relay", held.len(), 2);
    report.that(
        "the LATER write carries the EARLIER stamp, which is the whole point",
        held[0].content.contains("backwards")
            && held[0].created_at == Timestamp::from_secs(1_600_000_000),
        held.iter()
            .map(|e| (e.created_at.as_secs(), e.content.clone()))
            .collect::<Vec<_>>(),
    );
    report
}

/// A clock installed at construction is in place BEFORE store recovery, which
/// a setter reachable only on a running engine could never be.
pub async fn stated_before_recovery(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("stated-before-recovery");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().on_event(Ev::any(), Reply::ok())).await;

    let clock = EngineClock::new();
    let stated = Timestamp::from_secs(1_500_000_000);
    clock.set(stated);

    // Nothing is called on the engine between construction and the write: the
    // clock was already true when the store opened.
    let engine = engine_at(&relay, &author, &clock);
    let _receipt = publish_note(&engine, "the first thing this engine ever did");
    let _ = relay
        .wire()
        .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
        .await;

    let held = relay.held();
    report.eq(
        "the engine's very first write already carries the stated instant",
        held.first().map(|e| e.created_at),
        Some(stated),
    );
    relay.wire().wait_quiet(QUIET, SETTLE).await;
    report
}

/// **Still true, and deliberately so.** The engine clock governs the REDUCER.
/// Advancing it thirty days moves the transport by nothing.
///
/// A scenario about an EXPIRY wants a stated instant; a scenario about a
/// RECONNECT wants a compressed schedule. One knob for both would make the
/// second lie.
pub async fn transport_is_unmoved(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("transport-is-unmoved");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().seed(crate::fixtures::notes(&author, 1, 1_700_000_000)).on_req(
        crate::Req::any(),
        Reply::new()
            .then_stored()
            .then_eose()
            .after(Duration::from_millis(50))
            .then_bytes(vec![0x83, 0x00]),
    ))
    .await;

    let clock = EngineClock::new();
    let engine = Engine::new(EngineConfig {
        app_relays: vec![relay.url().to_string()],
        clock: clock.clone(),
        ..EngineConfig::default()
    })
    .expect("the engine builds");
    let _subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while relay.record().served_event_ids().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    report.eq("one session so far", relay.session_count(), 1);

    // Thirty days pass, as far as the ENGINE is concerned.
    clock.advance(Duration::from_secs(30 * 24 * 60 * 60));
    tokio::time::sleep(Duration::from_millis(400)).await;
    report.eq(
        "thirty days of STATED time bought nothing: the transport is on \
         Instant, and no door projects a clock onto it",
        relay.session_count(),
        1,
    );

    let deadline = std::time::Instant::now() + crate::clock::PRODUCTION_RECONNECT_FLOOR * 3;
    while relay.session_count() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    report.that(
        "and the redial happens on the WALL clock, never on the stated one",
        relay.session_count() >= 2,
        relay.session_count(),
    );
    report
}

/// An engine given no clock reads the real one, byte for byte as before.
pub async fn unpinned_reads_real_time(_mutation: Option<&'static str>) -> Report {
    let mut report = Report::new("unpinned-reads-real-time");
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().on_event(Ev::any(), Reply::ok())).await;
    let engine = crate::fixtures::publishing_engine(&relay, &author);

    let before = Timestamp::now();
    let _receipt = publish_note(&engine, "published on whatever clock the machine has");
    let _ = relay
        .wire()
        .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
        .await;
    let after = Timestamp::now();

    let held = relay.held();
    report.that(
        "a default EngineConfig still stamps with the real system clock",
        held.len() == 1 && held[0].created_at >= before && held[0].created_at <= after,
        held.first().map(|e| e.created_at.as_secs()),
    );
    let _ = rows_within;
    report
}
