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
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nmp_transport::{AttemptCorrelation, HandoffResult, Pool, PoolConfig, PoolEvent, WireFrame};
use nostr::{JsonUtil, RelayMessage};
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
    matches!(event, PoolEvent::Frame { frame, .. } if frame.clone().into_message().as_json().contains(needle))
}

/// Reserve an ephemeral TCP port by binding then immediately dropping the
/// listener, so the *second* relay instance in the reconnect half of this
/// test can rebind the exact same port the first one used.
static NEXT_TEST_PORT: AtomicU16 = AtomicU16::new(0);

fn free_port() -> u16 {
    let base = 20_000 + (std::process::id() % 20_000) as u16;
    let _ = NEXT_TEST_PORT.compare_exchange(0, base, Ordering::Relaxed, Ordering::Relaxed);
    loop {
        let port = NEXT_TEST_PORT.fetch_add(1, Ordering::Relaxed);
        if port < 20_000 {
            NEXT_TEST_PORT.store(base, Ordering::Relaxed);
            continue;
        }
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// `LocalRelay::run` starts its listener task asynchronously. Prove the TCP
/// accept loop is live before asking the pool to dial, so a loaded CI host
/// cannot turn startup scheduling into a reconnect-test timeout.
async fn wait_for_listener(port: u16) {
    tokio::time::timeout(Duration::from_secs(5), async move {
        loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => {
                    drop(stream);
                    return;
                }
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("test relay listener did not become ready");
}

/// `LocalRelay::shutdown()` is fire-and-forget: it calls `tokio::sync::
/// Notify::notify_waiters` on the listener task and returns immediately --
/// it never awaits that task actually waking up, ceasing to accept, or
/// releasing the OS-level TCP socket. A fixed `sleep` after it is a guess
/// at how long that async teardown takes, not a fact about it (the #60
/// anti-pattern this rewrite removes: condition on what is actually true,
/// never on how long something usually takes). This is `wait_for_listener`'s
/// mirror image: poll for the OBSERVABLE condition that actually matters --
/// the port is bindable again -- rather than assuming a delay was enough.
async fn wait_for_port_released(port: u16) {
    tokio::time::timeout(Duration::from_secs(5), async move {
        loop {
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("port was not released by the previous relay instance");
}

/// Rust's test harness runs the test fns in this file concurrently by
/// default, and each `#[tokio::test(flavor = "multi_thread", ...)]` below
/// gets its OWN dedicated tokio runtime -- so left unguarded, this one file
/// can put up to three separate `LocalRelay` ecosystems (the two reconnect
/// tests' relay_a+relay_b pairs, plus test 3's single relay) on the CPU at
/// the exact same moment. That is a purely SELF-inflicted source of the
/// scheduling contention that turns "wait for a fresh reconnect" into a
/// flake under CI load: `pool::connect::open_relay_socket`'s
/// `CONNECT_TIMEOUT` (10s) bounds ONE dial attempt against a relay whose
/// own accept/handshake task is starved of CPU, so a single stalled
/// attempt during heavy contention can already eat most of a 15s budget --
/// see the `Connected`-wait bounds below, sized around that mechanism, not
/// guessed. Serializing this file's tests removes the self-inflicted half
/// of that contention for free (each test normally finishes in well under
/// a second, so serializing costs nothing observable); it does nothing
/// about contention from OTHER crates' test binaries running concurrently
/// in the same CI job, which the generous bounds below still have to
/// absorb on their own.
/// `tokio::sync::Mutex`, not `std::sync::Mutex`: this guard is held across
/// `.await` points (every relay setup/teardown call below), which clippy's
/// `await_holding_lock` correctly refuses to allow for a blocking mutex --
/// an async-aware one is the sound way to hold a lock across awaits.
static RECONNECT_TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// Multi-thread flavor is load-bearing here, not a style choice: the test
// body blocks synchronously (`recv_matching` calls `mpsc::Receiver::recv_timeout`,
// never `.await`) while waiting for `nmp-transport`'s own OS threads to do
// their work. `LocalRelay::run` spawns its accept/session loop onto the
// AMBIENT tokio runtime; on the default current-thread flavor, blocking the
// one runtime thread here would also freeze the relay's ability to accept
// our connection or respond to REQ, deadlocking the test. `worker_threads =
// 3` (not the bare minimum 2) so the relay's own accept/session tasks keep
// a genuinely free thread to run on even while `recv_matching` parks one
// thread in a blocking wait -- this test drives TWO live relay instances
// (relay_a, then relay_b) and needs headroom for whichever one is live.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn connect_req_event_eose_close_then_reconnect_replays_subscription() {
    let _serial_guard = RECONNECT_TEST_GUARD.lock().await;
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
    wait_for_listener(port).await;
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
            // `backoff::jittered`'s per-URL offset is a FIXED value (up to
            // ~5s in production), re-paid on EVERY retry against this URL
            // until one connects -- not re-rolled per attempt. relay_a and
            // relay_b share the exact same URL (same host:port), and the
            // pool's own worker thread starts redialing within milliseconds
            // of `relay_a.shutdown()` -- well before this test's own
            // `wait_for_port_released`/`wait_for_listener` dance can
            // possibly finish bringing relay_b up, so at least one doomed
            // connect attempt against this URL is effectively guaranteed
            // every run. Whether that URL's hash happens to land near the
            // ~5s ceiling is pure per-port luck (`free_port` picks a fresh
            // ephemeral port each run) -- on an unlucky port, two such
            // taxed retries alone can eat ~10s, which is what was blowing
            // this test's `Connected` wait under CI load. Disabling jitter
            // here removes the lottery rather than padding the timeout to
            // out-wait it.
            reconnect_jitter_max: Some(Duration::ZERO),
            // The load-bearing fix for the RESIDUAL flake (recurred on
            // #109/#112/#120 even after the jitter fix above):
            // `LocalRelay::shutdown()` (read directly from
            // `nostr-relay-builder`'s `local/inner.rs`) only notifies the
            // ACCEPT loop to stop taking new connections -- it never
            // touches an already-established per-connection session task.
            // So `relay_a.shutdown()` below does NOT reliably sever our
            // ALREADY-OPEN connection to relay_a; that usually happens
            // quickly anyway (the socket gets a read error/EOF from the OS
            // teardown), but confirmed-by-instrumentation on a rare run
            // (~1 in 25-30, reproduced with ZERO added load) it does not,
            // and the worker's ONLY remaining way to notice relay_a is gone
            // is the keepalive idle-ping/pong-timeout fallback -- whose
            // PRODUCTION defaults (`KEEPALIVE_IDLE_THRESHOLD` /
            // `KEEPALIVE_PONG_TIMEOUT`, 30s each) sum to a ~60s worst case,
            // dwarfing any timeout this test could reasonably afford and
            // producing a hang that looks identical to "never reconnects"
            // from the test's side (zero further `PoolEvent`s of any kind
            // until the fallback finally fires). Overriding both to a small
            // value makes that fallback path fast and bounded instead of
            // production-scale slow, exactly mirroring why
            // `reconnect_delay_initial` above is overridden the same way.
            keepalive_idle: Some(Duration::from_millis(200)),
            keepalive_pong_timeout: Some(Duration::from_millis(200)),
            ..PoolConfig::default()
        },
        tx,
    );

    // Act: connect and observe the fresh (generation-1) handle. 15s, not
    // 5s: `open_relay_socket`'s `CONNECT_TIMEOUT` (10s) bounds a single dial
    // attempt, so a fresh first connect that races a starved accept task
    // under CI load can alone eat close to that whole budget before this
    // bound would even be exercised -- 15s is the smallest round number
    // that still comfortably clears one full stalled attempt plus margin.
    let h1 = pool.ensure_open(&url);
    let connected1 = recv_matching(&rx, Duration::from_secs(15), is_connected);
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
        PoolEvent::Frame { frame, .. } => match frame.into_message() {
            RelayMessage::Event {
                event: received, ..
            } => assert_eq!(
                received.id.to_hex(),
                event.id.to_hex(),
                "EVENT frame carries our seeded event"
            ),
            other => panic!("expected an EVENT message, got {other:?}"),
        },
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
    wait_for_port_released(port).await;
    let relay_b = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay_b.run().await.expect("run relay_b");
    wait_for_listener(port).await;
    relay_b
        .add_event(event.clone())
        .await
        .expect("seed event into relay_b");

    // Assert: a NEW Connected with a bumped generation (test 6/7's core
    // falsifier), then — with NO further `pool.send` from this test — the
    // replayed REQ yields a fresh EVENT+EOSE from relay_b. 30s, not 15s:
    // with `reconnect_jitter_max` zeroed above, the remaining exposure is
    // `CONNECT_TIMEOUT` (10s) per stalled dial attempt, and a reconnect can
    // plausibly need more than one attempt under heavy CI contention before
    // relay_b's accept task actually gets scheduled -- 30s comfortably
    // covers two full stalled attempts plus margin, not a guess.
    let connected2 = recv_matching(&rx, Duration::from_secs(30), is_connected);
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
// `worker_threads = 3` for the same reason as the test above: this test
// also drives two live relay instances (relay_a, then relay_b) across a
// real reconnect, and the extra thread keeps the relay's own accept/
// session tasks from contending with `recv_matching`'s blocking wait for
// CPU time.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn durable_event_never_survives_reconnect_while_req_preamble_does() {
    let _serial_guard = RECONNECT_TEST_GUARD.lock().await;
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
    wait_for_listener(port).await;
    let relay_url_str = relay_a.url().await.to_string();
    let url = nostr::RelayUrl::parse(&relay_url_str).expect("parse relay url");

    let (tx, rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            // `backoff::jittered`'s per-URL offset is a FIXED value (up to
            // ~5s in production), re-paid on EVERY retry against this URL
            // until one connects -- not re-rolled per attempt. relay_a and
            // relay_b share the exact same URL (same host:port), and the
            // pool's own worker thread starts redialing within milliseconds
            // of `relay_a.shutdown()` -- well before this test's own
            // `wait_for_port_released`/`wait_for_listener` dance can
            // possibly finish bringing relay_b up, so at least one doomed
            // connect attempt against this URL is effectively guaranteed
            // every run. Whether that URL's hash happens to land near the
            // ~5s ceiling is pure per-port luck (`free_port` picks a fresh
            // ephemeral port each run) -- on an unlucky port, two such
            // taxed retries alone can eat ~10s, which is what was blowing
            // this test's `Connected` wait under CI load. Disabling jitter
            // here removes the lottery rather than padding the timeout to
            // out-wait it.
            reconnect_jitter_max: Some(Duration::ZERO),
            // The load-bearing fix for the RESIDUAL flake (recurred on
            // #109/#112/#120 even after the jitter fix above):
            // `LocalRelay::shutdown()` (read directly from
            // `nostr-relay-builder`'s `local/inner.rs`) only notifies the
            // ACCEPT loop to stop taking new connections -- it never
            // touches an already-established per-connection session task.
            // So `relay_a.shutdown()` below does NOT reliably sever our
            // ALREADY-OPEN connection to relay_a; that usually happens
            // quickly anyway (the socket gets a read error/EOF from the OS
            // teardown), but confirmed-by-instrumentation on a rare run
            // (~1 in 25-30, reproduced with ZERO added load) it does not,
            // and the worker's ONLY remaining way to notice relay_a is gone
            // is the keepalive idle-ping/pong-timeout fallback -- whose
            // PRODUCTION defaults (`KEEPALIVE_IDLE_THRESHOLD` /
            // `KEEPALIVE_PONG_TIMEOUT`, 30s each) sum to a ~60s worst case,
            // dwarfing any timeout this test could reasonably afford and
            // producing a hang that looks identical to "never reconnects"
            // from the test's side (zero further `PoolEvent`s of any kind
            // until the fallback finally fires). Overriding both to a small
            // value makes that fallback path fast and bounded instead of
            // production-scale slow, exactly mirroring why
            // `reconnect_delay_initial` above is overridden the same way.
            keepalive_idle: Some(Duration::from_millis(200)),
            keepalive_pong_timeout: Some(Duration::from_millis(200)),
            ..PoolConfig::default()
        },
        tx,
    );

    // 15s, not 5s -- see test 7's identical `connected1` wait above for why
    // (CONNECT_TIMEOUT-bounded first-dial exposure).
    let h1 = pool.ensure_open(&url);
    let connected1 = recv_matching(&rx, Duration::from_secs(15), is_connected);
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
            assert_eq!(
                result,
                HandoffResult::NotHandedOff,
                "a durable EVENT submitted after the worker observed disconnect never reached \
                 socket.write and must resolve exactly NotHandedOff"
            );
        }
        other => panic!("expected EventHandoff, got {other:?}"),
    }

    // Bring relay_b up on the SAME port and let the pool reconnect.
    wait_for_port_released(port).await;
    let relay_b = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay_b.run().await.expect("run relay_b");
    wait_for_listener(port).await;

    // 30s -- see test 7's identical `connected2` wait above for why (two
    // CONNECT_TIMEOUT-bounded stalled attempts, not a guess).
    let connected2 = recv_matching(&rx, Duration::from_secs(30), is_connected);
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
// No reconnect dance here (a single relay, never torn down), so this test
// doesn't need the extra thread the two reconnect tests above do -- but it
// still shares the serialization guard so this file never puts more than
// one `LocalRelay` ecosystem on the CPU at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_event_resolves_written_exactly_once() {
    let _serial_guard = RECONNECT_TEST_GUARD.lock().await;
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
    wait_for_listener(port).await;
    let url = nostr::RelayUrl::parse(&relay.url().await.to_string()).expect("parse relay url");

    let (tx, rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(PoolConfig::default(), tx);
    let h = pool.ensure_open(&url);
    // 15s, not 5s -- see test 7's identical `connected1` wait for why
    // (CONNECT_TIMEOUT-bounded first-dial exposure).
    recv_matching(&rx, Duration::from_secs(15), is_connected);

    let event: Event = EventBuilder::text_note("resolves exactly once")
        .finalize(&keys)
        .expect("sign test event");
    let json = format!(r#"["EVENT",{}]"#, event.as_json());
    let correlation = AttemptCorrelation(1);
    assert_eq!(
        pool.send_durable(h, correlation, WireFrame::Text(json)),
        nmp_transport::DurableSendOutcome::Queued
    );

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
