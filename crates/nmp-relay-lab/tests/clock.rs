//! The time door, exercised. Read `nmp_relay_lab::clock` first: this file is
//! the executable half of the finding recorded there.

mod support;

use std::time::Duration;

use nmp_relay_lab::{clock::PRODUCTION_RECONNECT_FLOOR, Ev, RelayLab, Reply, Req, Script};
use nostr::{Keys, Timestamp};
use support::{engine_against, kind1_by, notes, publish_note, publishing_engine, QUIET, SETTLE};

/// A stated instant reaches the wire. The engine stamps a write it is asked
/// to time itself with the reducer's clock, so pinning that clock is
/// observable to the relay -- which is what makes a scenario about a device
/// clock assertable at all.
///
/// This is `Engine::clock()`: `#[doc(hidden)]`, behind `unstable-mechanism`,
/// and therefore not something an application may do.
#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_engine_clock_stamps_the_write_that_reaches_the_relay() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new()).await;
    let engine = publishing_engine(&relay, &author);

    let stated = Timestamp::from_secs(1_600_000_000);
    engine.clock().expect("the engine is open").set(stated);

    let _receipt = publish_note(&engine, "written at a stated instant");
    assert!(
        relay
            .wire()
            // The OK, not the EVENT: the relay's reply is emitted AFTER the
            // ingest step, so waiting on it is what makes `held()` settled.
            .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
            .await,
        "the write must reach the relay and be acknowledged"
    );

    let held = relay.held();
    assert_eq!(held.len(), 1, "one write reached the relay");
    assert_eq!(
        held[0].created_at, stated,
        "the event carries the STATED instant, not the real one"
    );
}

/// The clock may move BACKWARD. A device whose clock is behind is a case the
/// write plane has to survive, and `EngineClock::set` documents the backward
/// jump as deliberate. Two writes, the second stamped earlier than the first.
#[tokio::test(flavor = "multi_thread")]
async fn an_engine_clock_may_be_moved_backwards_across_two_writes() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new()).await;
    let engine = publishing_engine(&relay, &author);
    let clock = engine.clock().expect("the engine is open");

    clock.set(Timestamp::from_secs(1_700_000_000));
    let _first = publish_note(&engine, "written on a clock that is ahead");
    assert!(
        relay
            .wire()
            .wait_for(SETTLE, |record| record.oks_sent().len() == 1)
            .await
    );

    clock.set(Timestamp::from_secs(1_600_000_000));
    let _second = publish_note(&engine, "written after the clock went backwards");
    assert!(
        relay
            .wire()
            .wait_for(SETTLE, |record| record.oks_sent().len() == 2)
            .await
    );

    let mut held = relay.held();
    held.sort_by_key(|event| event.created_at);
    assert_eq!(held.len(), 2);
    assert_eq!(held[0].created_at, Timestamp::from_secs(1_600_000_000));
    assert_eq!(held[1].created_at, Timestamp::from_secs(1_700_000_000));
    assert!(
        held[0].content.contains("backwards"),
        "the LATER write carries the EARLIER stamp, which is the whole point"
    );
}

/// **The finding.** The engine clock is the REDUCER's clock. Advancing it by
/// thirty days does not move the transport by one millisecond: reconnect
/// backoff is `Instant::now()` in `nmp-transport`'s worker, and nothing an
/// app or a harness can reach intercepts it.
///
/// So a scenario about a background gap or a backoff cannot be written as
/// "thirty days pass". It can only be written as "wait", and the wait is
/// [`PRODUCTION_RECONNECT_FLOOR`], because `PoolConfig::reconnect_delay_initial`
/// and `PoolConfig::reconnect_jitter_max` -- which exist precisely so a test
/// need not -- are unreachable from `EngineConfig`.
#[tokio::test(flavor = "multi_thread")]
async fn advancing_the_engine_clock_does_not_shorten_a_reconnect_backoff() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().seed(notes(&author, 1, 1_700_000_000)).on_req(
        Req::any(),
        Reply::new()
            .then_stored()
            .then_eose()
            .after(Duration::from_millis(50))
            .then_bytes(vec![0x83, 0x00]),
    ))
    .await;

    let engine = engine_against(&relay);
    let _subscription = engine
        .observe(kind1_by(&author, &relay), None)
        .expect("the observation opens");

    // Wait for the session to have been failed by the illegal frame.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while relay.record().served_event_ids().is_empty()
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(relay.session_count(), 1, "one session so far");

    // Thirty days pass, as far as the ENGINE is concerned.
    engine
        .clock()
        .expect("the engine is open")
        .advance(Duration::from_secs(30 * 24 * 60 * 60));

    // The transport is unmoved: no redial has happened, and none will for as
    // long as the real backoff takes.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        relay.session_count(),
        1,
        "thirty days of STATED time bought nothing: the transport is on \
         `Instant`, and no door projects a clock onto it"
    );

    // And it does redial, on real time, once the real schedule elapses.
    let deadline = std::time::Instant::now() + PRODUCTION_RECONNECT_FLOOR * 3;
    while relay.session_count() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        relay.session_count() >= 2,
        "the redial happens on the wall clock, never on the stated one"
    );
}

/// The clock is reachable only AFTER construction, so nothing that happens
/// during store recovery can be given a stated time. This pins the shape of
/// the gap rather than a workaround for it: a write published before any
/// `set` carries the REAL clock, and no ordering of calls changes that.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_published_before_the_clock_is_stated_carries_the_real_time() {
    let author = Keys::generate();
    let relay = RelayLab::start(Script::new().on_event(Ev::any(), Reply::ok())).await;
    let engine = publishing_engine(&relay, &author);

    let before = Timestamp::now();
    let _receipt = publish_note(&engine, "published before anyone stated the time");
    assert!(
        relay
            .wire()
            .wait_for(SETTLE, |record| !record.oks_sent().is_empty())
            .await
    );
    let after = Timestamp::now();

    let held = relay.held();
    assert_eq!(held.len(), 1);
    assert!(
        held[0].created_at >= before && held[0].created_at <= after,
        "an unpinned engine reads the real clock, exactly as it always did: \
         {:?} is not within {before:?}..={after:?}",
        held[0].created_at
    );

    relay.wire().wait_quiet(QUIET, SETTLE).await;
}
