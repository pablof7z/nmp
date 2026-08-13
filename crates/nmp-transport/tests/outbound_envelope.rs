//! Issue #506, outbound-memory half: one relay worker's *retained* ordinary
//! outbound state must be finite.
//!
//! PR #511 bounded the transit channel (`PoolConfig::command_queue_capacity`)
//! and made `WorkerHandle::push` refuse a full one. That is a bound on how
//! many commands may be *in flight to* the worker, not on how much the worker
//! retains once it has taken them. A running worker continuously drains the
//! channel into its own uncapped `VecDeque`, so every receive frees a channel
//! slot and a producer can refill it forever while `Pool::send` keeps
//! returning `true` — the lying return value at the sharp end of #506.
//!
//! Both tests below drive a REAL worker, which is what distinguishes them
//! from the existing `push_reports_backpressure_once_the_bounded_queue_is_full`
//! unit test: that one fills a channel whose receiver never runs, so it can
//! only ever prove finite transit capacity.
//!
//! Neither test knows the declared envelope's value. Each asserts an
//! independent ceiling that any honest finite bound sits below, so raising the
//! internal constant to something absurd fails here rather than silently
//! passing.

use std::net::{Ipv4Addr, TcpListener};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use nmp_transport::{Pool, PoolConfig, PoolEvent, WireFrame};

const FRAME_BYTES: usize = 4 * 1024;

/// Attempts, not milliseconds. Each loop terminates on a fact — either the
/// accepted total passes the ceiling (the defect) or the budget is spent
/// (bounded). A refused attempt yields so a real worker gets to run.
const SEND_ATTEMPTS: usize = 50_000;

fn test_pool_config() -> PoolConfig {
    PoolConfig {
        command_queue_capacity: 4,
        reconnect_delay_initial: Some(Duration::from_millis(1)),
        reconnect_jitter_max: Some(Duration::ZERO),
        ..PoolConfig::default()
    }
}

/// A port nothing listens on: every dial fails at once with `ECONNREFUSED`,
/// which is not a permanent failure, so the slot stays `Connecting` and
/// `Pool::send` keeps handing frames to the worker.
fn dead_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral port");
    let port = listener.local_addr().expect("ephemeral address").port();
    drop(listener);
    port
}

fn spam_ordinary_frames(pool: &Pool, handle: nmp_transport::RelayHandle, ceiling: usize) -> usize {
    let frame = "x".repeat(FRAME_BYTES);
    let mut accepted = 0usize;
    for _ in 0..SEND_ATTEMPTS {
        if pool.send(handle, WireFrame::Text(frame.clone())) {
            accepted += 1;
            assert!(
                accepted * FRAME_BYTES <= ceiling,
                "the worker accepted {accepted} ordinary frames ({} bytes) \
                 without ever putting one on the wire; retained outbound state \
                 is unbounded (#506)",
                accepted * FRAME_BYTES,
            );
        } else {
            thread::yield_now();
        }
    }
    accepted
}

/// Falsifier 2 of #506: a real worker whose dials keep failing buffers every
/// ordinary `Send` into worker-local state that nothing drains. Not one byte
/// of this ever reaches a socket, so the whole accepted total is memory NMP
/// is holding.
#[test]
fn a_reconnecting_worker_retains_a_bounded_ordinary_outbound_envelope() {
    // Nothing has flushed, so everything accepted is retained. Any honest
    // per-worker envelope is well under this.
    const RETAINED_CEILING_BYTES: usize = 4 * 1024 * 1024;

    let url = nostr::RelayUrl::parse(&format!("ws://127.0.0.1:{}", dead_port()))
        .expect("parse relay url");
    let (tx, rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(test_pool_config(), tx).expect("test pool construction");
    let handle = pool.ensure_open(&url).expect("admitted");

    let accepted = spam_ordinary_frames(&pool, handle, RETAINED_CEILING_BYTES);
    assert!(
        accepted >= 8,
        "the envelope must admit a useful working set before refusing (only \
         {accepted} frames admitted); a bound that tight would mean the \
         four-slot transit channel refused, not the envelope"
    );

    pool.shutdown();
    drop(rx);
}

/// Falsifier 3 of #506: a peer that completes the WebSocket handshake and
/// then never reads a byte. The socket's write side blocks, `flush_writes`
/// keeps returning `Blocked`, and every further ordinary frame is retained —
/// first in the worker's deque, then in tungstenite's own write buffer, whose
/// `max_write_buffer_size` defaults to `usize::MAX`.
#[test]
fn a_connected_peer_that_never_reads_cannot_grow_the_process() {
    // Everything NMP itself may retain for this relay, plus a generous
    // allowance for the OS socket buffers at both ends of a loopback pair:
    // those bytes have genuinely left NMP, and they are finite whatever NMP
    // does.
    const RETAINED_CEILING_BYTES: usize = 16 * 1024 * 1024;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind slow relay");
    let port = listener.local_addr().expect("slow relay address").port();
    let peer = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("slow relay accepts one client");
        // Complete the handshake, then hold the socket open and never read
        // another byte. This is a slow peer, not a dead one.
        let socket = tungstenite::accept(stream).expect("slow relay handshake");
        thread::park_timeout(Duration::from_secs(120));
        drop(socket);
    });

    let url =
        nostr::RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("parse slow relay url");
    let (tx, rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(test_pool_config(), tx).expect("test pool construction");
    let handle = pool.ensure_open(&url).expect("admitted");

    let accepted = spam_ordinary_frames(&pool, handle, RETAINED_CEILING_BYTES);
    assert!(accepted >= 1, "at least one frame must be admitted");

    pool.shutdown();
    drop(rx);
    peer.thread().unpark();
    let _ = peer.join();
}
