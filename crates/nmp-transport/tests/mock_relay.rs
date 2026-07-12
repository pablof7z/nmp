//! Integration test (M3 plan §5, "test 7"): a real round-trip against an
//! in-process test relay (`nostr-relay-builder`'s `LocalRelay`/`MockRelay`).
//! Connect, REQ, receive EVENT+EOSE, CLOSE, then force a disconnect and
//! verify the pool reconnects with a bumped generation and REPLAYS the
//! registered subscription via the reconnect-preamble hook — without the
//! test re-sending the REQ itself.
//!
//! Confined to `#[tokio::test]`: `nostr-relay-builder`/`nostr-sdk` are
//! async under the hood. This is dev-dependency-only — the production
//! `Pool` under test imposes no runtime on its own caller (D8); the tokio
//! runtime here drives the test relay, not `nmp-transport`.

use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nmp_transport::{
    AttemptCorrelation, HandoffResult, Pool, PoolConfig, PoolEvent, RelayFrame, WireFrame,
};
// Deliberately NOT a glob import: `nostr_relay_builder::prelude::*` re-exports
// `nostr::prelude::*` from ITS OWN `nostr` dependency (0.45-alpha, distinct
// from this workspace's pinned `nostr = "0.44.4"` that `nmp-transport`'s
// public API uses). A glob import shadows the extern-prelude crate name
// (2018 name resolution gives `use`-imported items priority over the
// extern prelude), so `nostr::RelayUrl` below would silently resolve to the
// WRONG (0.45) `RelayUrl` type. Import only the specific test-relay items we
// need instead, so `nostr::` unambiguously means this crate's own dependency.
use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{Event, EventBuilder, FinalizeEvent, Keys};

/// Block (on the calling OS thread — `Pool`'s events arrive on a plain
/// `mpsc::Sender`, never through tokio) until an event matching `pred`
/// arrives, or panic after `timeout`.
fn recv_matching(
    rx: &mpsc::Receiver<PoolEvent>,
    timeout: Duration,
    pred: impl Fn(&PoolEvent) -> bool,
) -> PoolEvent {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for a matching PoolEvent"
        );
        match rx.recv_timeout(remaining) {
            Ok(event) if pred(&event) => return event,
            Ok(other) => eprintln!("[test] draining non-matching event: {other:?}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {} // loop back to the outer deadline check
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("PoolEvent channel closed while waiting")
            }
        }
    }
}

fn is_connected(event: &PoolEvent) -> bool {
    matches!(event, PoolEvent::Connected { .. })
}

fn frame_contains(event: &PoolEvent, needle: &str) -> bool {
    matches!(event, PoolEvent::Frame { frame: RelayFrame::Text(text), .. } if text.contains(needle))
}

/// Reserve an ephemeral TCP port by binding then immediately dropping the
/// listener, so the *second* relay instance in the reconnect half of a test
/// can rebind the exact same port the first one used.
///
/// This crate's tests run concurrently by default (Rust's own harness, no
/// `#[serial]`), and MULTIPLE tests in this file now reuse a port this way
/// (issue #93 added a second one). A bare `bind(0)`-then-drop is a genuine
/// cross-test TOCTOU: two tests' own `free_port()` calls landing close
/// enough in wall-clock time could observe (and later rebind) the SAME
/// just-released port before either test's own relay claims it, since the
/// OS's ephemeral allocator has no notion of "this process's OWN separate
/// test threads." A monotonically-increasing, process-wide counter removes
/// that cross-test collision entirely: no two calls in this binary, no
/// matter how they interleave, can ever be handed the same port number.
static NEXT_PORT_HINT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(20_000);

fn free_port() -> u16 {
    use std::sync::atomic::Ordering;
    loop {
        let hint = NEXT_PORT_HINT.fetch_add(1, Ordering::Relaxed);
        if hint < 20_000 {
            // Wrapped past u16::MAX -- reset into range and retry.
            NEXT_PORT_HINT.store(20_000, Ordering::Relaxed);
            continue;
        }
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", hint)) {
            drop(listener);
            return hint;
        }
        // Already bound by something else on the machine (never by another
        // call in THIS binary, since the counter never repeats) -- try the
        // next hint.
    }
}

// Multi-thread flavor is load-bearing here, not a style choice: the test
// body blocks synchronously (`recv_matching` calls `mpsc::Receiver::recv_timeout`,
// never `.await`) while waiting for `nmp-transport`'s own OS threads to do
// their work. `LocalRelay::run` spawns its accept/session loop onto the
// AMBIENT tokio runtime; on the default current-thread flavor, blocking the
// one runtime thread here would also freeze the relay's ability to accept
// our connection or respond to REQ, deadlocking the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_req_event_eose_close_then_reconnect_replays_subscription() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port = free_port();
    let keys = Keys::generate();
    let event: Event = EventBuilder::text_note("hello from nmp-transport's test 7")
        .finalize(&keys)
        .expect("sign test event");

    let relay_a = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay_a.run().await.expect("run relay_a");
    eprintln!("[test] relay_a running on port {port}");
    relay_a
        .add_event(event.clone())
        .await
        .expect("seed event into relay_a");
    eprintln!("[test] seeded event into relay_a");
    let relay_url_str = relay_a.url().await.to_string();
    eprintln!("[test] relay_a url = {relay_url_str}");

    let url = nostr::RelayUrl::parse(&relay_url_str).expect("parse relay url");

    let (tx, rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
        tx,
    );

    // Act: connect and observe the fresh (generation-1) handle.
    let h1 = pool.ensure_open(&url);
    let connected1 = recv_matching(&rx, Duration::from_secs(5), is_connected);
    let PoolEvent::Connected {
        handle: observed1, ..
    } = connected1
    else {
        unreachable!("is_connected guard")
    };
    assert_eq!(
        observed1, h1,
        "Connected must carry the handle ensure_open returned"
    );

    // REQ the seeded event.
    let sub_id = "sub1";
    let req = format!(
        r#"["REQ","{sub_id}",{{"authors":["{}"]}}]"#,
        event.pubkey.to_hex()
    );
    assert!(pool.send(h1, WireFrame::Text(req.clone())), "send REQ");

    let ev_frame = recv_matching(&rx, Duration::from_secs(5), |e| {
        frame_contains(e, "\"EVENT\"")
    });
    match ev_frame {
        PoolEvent::Frame {
            frame: RelayFrame::Text(text),
            ..
        } => {
            assert!(
                text.contains(&event.id.to_hex()),
                "EVENT frame carries our seeded event"
            );
        }
        other => panic!("expected a Frame, got {other:?}"),
    }
    recv_matching(&rx, Duration::from_secs(5), |e| {
        frame_contains(e, "\"EOSE\"")
    });

    assert!(
        pool.send(h1, WireFrame::Text(format!(r#"["CLOSE","{sub_id}"]"#))),
        "send CLOSE"
    );

    // Register the reconnect preamble AFTER the manual REQ above — this is
    // what the engine does once it has observed Connected and issued its
    // live subscriptions: register them so a FUTURE reconnect replays them
    // automatically.
    assert!(pool.set_reconnect_preamble(h1, vec![req.clone()]));

    // Force a disconnect: tear down relay_a, then rebind a fresh relay_b on
    // the SAME port, seeded with the same event (a fresh instance has its
    // own database). The pool's worker must detect the drop, back off
    // briefly, and redial — no NMP code drives this, only the harvested
    // reconnect loop.
    relay_a.shutdown();
    let relay_b = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    // Give the OS a brief moment to release the port after relay_a's
    // shutdown before relay_b binds it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    relay_b.run().await.expect("run relay_b");
    relay_b
        .add_event(event.clone())
        .await
        .expect("seed event into relay_b");

    // Assert: a NEW Connected with a bumped generation (test 6/7's core
    // falsifier), then — with NO further `pool.send` from this test — the
    // replayed REQ yields a fresh EVENT+EOSE from relay_b.
    let connected2 = recv_matching(&rx, Duration::from_secs(15), is_connected);
    let PoolEvent::Connected { handle: h2, .. } = connected2 else {
        unreachable!("is_connected guard")
    };
    assert_ne!(
        h1.generation, h2.generation,
        "reconnect must mint a fresh generation"
    );

    recv_matching(&rx, Duration::from_secs(5), |e| {
        frame_contains(e, "\"EVENT\"")
    });
    recv_matching(&rx, Duration::from_secs(5), |e| {
        frame_contains(e, "\"EOSE\"")
    });

    // The old (pre-reconnect) handle is now structurally stale.
    assert!(
        !pool.send(h1, WireFrame::Text("[\"CLOSE\",\"sub1\"]".to_string())),
        "a superseded handle must be rejected"
    );
    assert!(
        pool.send(h2, WireFrame::Text(format!(r#"["CLOSE","{sub_id}"]"#))),
        "the current handle must still work"
    );

    eprintln!("[test] calling pool.shutdown()");
    pool.shutdown();
    eprintln!("[test] pool.shutdown() returned");
    relay_b.shutdown();
    eprintln!("[test] relay_b.shutdown() returned, test complete");
}

/// Issue #93's core falsifier, over REAL sockets: a durable `EVENT`
/// submitted via [`Pool::send_durable`] must NEVER survive into a new
/// connection generation. Unlike a REQ (which legitimately replays via the
/// reconnect preamble -- proved in the SAME test, mirroring test 7 above,
/// so this seam is shown orthogonal to that one, not a replacement for
/// it), an EVENT still in flight when the connection ends resolves its
/// `AttemptCorrelation` (never silently as if nothing happened, and never
/// `Written` once the generation has already ended) and is never written
/// to the NEW connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_event_never_survives_reconnect_while_req_preamble_does() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port = free_port();
    let keys = Keys::generate();

    let relay_a = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay_a.run().await.expect("run relay_a");
    let relay_url_str = relay_a.url().await.to_string();
    let url = nostr::RelayUrl::parse(&relay_url_str).expect("parse relay url");

    let (tx, rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
        tx,
    );

    let h1 = pool.ensure_open(&url);
    let connected1 = recv_matching(&rx, Duration::from_secs(5), is_connected);
    let PoolEvent::Connected {
        handle: observed1, ..
    } = connected1
    else {
        unreachable!("is_connected guard")
    };
    assert_eq!(observed1, h1);

    // Register a REQ preamble, mirroring test 7 above -- proves this seam
    // is orthogonal to the existing REQ-replay mechanism, not a
    // replacement for it.
    let sub_id = "sub-durable-test";
    let req = format!(r#"["REQ","{sub_id}",{{"kinds":[1]}}]"#);
    assert!(pool.set_reconnect_preamble(h1, vec![req]));

    // The durable EVENT this test proves never survives reconnect.
    let stranded: Event = EventBuilder::text_note("must never reach relay_b")
        .finalize(&keys)
        .expect("sign stranded event");
    // Built as a raw `["EVENT", ...]` wire string, not via this crate's own
    // pinned `nostr::ClientMessage` -- `stranded` is `nostr-relay-builder`'s
    // OWN (0.45-alpha) `Event` type, a distinct crate version from this
    // workspace's pinned `nostr = "0.44.4"` (see the module doc's "no glob
    // import" note); only its own JSON serialization is safe to use here.
    let stranded_json = format!(r#"["EVENT",{}]"#, stranded.as_json());
    let correlation = AttemptCorrelation(1);

    // Tear relay_a down and wait for the pool to actually OBSERVE the
    // drop (`Disconnected`) before submitting the durable EVENT -- racing
    // it against a socket that is merely in the process of closing (as
    // `relay_a.shutdown()` alone would) is not reliable: a local write can
    // still land in the OS send buffer before the TCP teardown completes,
    // legitimately resolving `Written` and defeating the point of this
    // test. Waiting for the worker's OWN detected disconnect guarantees
    // there is no live connection left for the command to reach at all.
    relay_a.shutdown();
    let disconnected = recv_matching(&rx, Duration::from_secs(5), |e| {
        matches!(e, PoolEvent::Disconnected { .. })
    });
    assert!(matches!(disconnected, PoolEvent::Disconnected { .. }));
    let _ = pool.send_durable(h1, correlation, WireFrame::Text(stranded_json));

    // Whatever the immediate `bool` said, the authoritative answer is the
    // async `EventHandoff` — it must arrive (never silently dropped), and
    // it must NEVER be `Written`: this generation had already ended (or
    // was ending) before any relay could plausibly have kept the frame.
    let handoff = recv_matching(
        &rx,
        Duration::from_secs(10),
        |e| matches!(e, PoolEvent::EventHandoff { correlation: c, .. } if *c == correlation),
    );
    match handoff {
        PoolEvent::EventHandoff { result, .. } => {
            assert!(
                !matches!(result, HandoffResult::Written),
                "a durable EVENT submitted against an ending generation must never resolve \
                 Written, got {result:?}"
            );
        }
        other => panic!("expected EventHandoff, got {other:?}"),
    }

    // Bring relay_b up on the SAME port and let the pool reconnect.
    let relay_b = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    tokio::time::sleep(Duration::from_millis(100)).await;
    relay_b.run().await.expect("run relay_b");

    let connected2 = recv_matching(&rx, Duration::from_secs(15), is_connected);
    let PoolEvent::Connected { handle: h2, .. } = connected2 else {
        unreachable!("is_connected guard")
    };
    assert_ne!(
        h1.generation, h2.generation,
        "reconnect must mint a fresh generation"
    );

    // The REQ preamble DID replay (untouched by this seam): relay_b
    // receives the subscription without the test resending it — proved by
    // seeding a FRESH matching event into relay_b and observing it flow
    // back over the wire unprompted, exactly like test 7's own pattern.
    let confirm: Event = EventBuilder::text_note("proves the REQ preamble replayed")
        .finalize(&keys)
        .expect("sign confirm event");
    relay_b
        .add_event(confirm.clone())
        .await
        .expect("seed confirm event into relay_b");
    let confirm_frame = recv_matching(&rx, Duration::from_secs(5), |e| {
        frame_contains(e, &confirm.id.to_hex())
    });
    assert!(matches!(confirm_frame, PoolEvent::Frame { .. }));

    // The stranded EVENT must NEVER have reached relay_b: drain every
    // remaining event for a bounded grace window and assert its id never
    // appears anywhere on the wire.
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(event) => assert!(
                !frame_contains(&event, &stranded.id.to_hex()),
                "the stranded EVENT must never appear on relay_b's connection: {event:?}"
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    pool.shutdown();
    relay_b.shutdown();
}

/// A durable `EVENT` handed off to a live, healthy connection resolves
/// `Written` exactly once -- never a second `EventHandoff` for the same
/// `AttemptCorrelation`, over a real socket round trip (issue #93's
/// "duplicate result" falsifier).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_event_resolves_written_exactly_once() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port = free_port();
    let keys = Keys::generate();

    let relay = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay.run().await.expect("run relay");
    let url = nostr::RelayUrl::parse(&relay.url().await.to_string()).expect("parse relay url");

    let (tx, rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(PoolConfig::default(), tx);
    let h = pool.ensure_open(&url);
    recv_matching(&rx, Duration::from_secs(5), is_connected);

    let event: Event = EventBuilder::text_note("resolves exactly once")
        .finalize(&keys)
        .expect("sign test event");
    let json = format!(r#"["EVENT",{}]"#, event.as_json());
    let correlation = AttemptCorrelation(1);
    assert!(pool.send_durable(h, correlation, WireFrame::Text(json)));

    let first = recv_matching(
        &rx,
        Duration::from_secs(5),
        |e| matches!(e, PoolEvent::EventHandoff { correlation: c, .. } if *c == correlation),
    );
    assert!(matches!(
        first,
        PoolEvent::EventHandoff {
            result: HandoffResult::Written,
            ..
        }
    ));

    // Drain everything else for a bounded grace window -- the SAME
    // correlation must never appear a second time.
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(PoolEvent::EventHandoff { correlation: c, .. }) if c == correlation => {
                panic!("the same AttemptCorrelation must never resolve a second time")
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    pool.shutdown();
    relay.shutdown();
}
