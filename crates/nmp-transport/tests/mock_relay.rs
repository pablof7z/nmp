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

use nmp_transport::{Pool, PoolConfig, PoolEvent, RelayFrame, WireFrame};
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
/// listener, so the *second* relay instance in the reconnect half of this
/// test can rebind the exact same port the first one used.
fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
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
