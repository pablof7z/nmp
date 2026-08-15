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
//!
//! The two differ in how they decide they have offered enough, because the two
//! workers differ in what they let an outside observer see. A redialing worker
//! publishes one `PoolEvent::Health` per failed dial, so
//! [`saturate_reconnecting_worker`] can end on a fact that worker reached:
//! whole redial cycles in which nothing it was offered was taken. A worker
//! CONNECTED to a peer that never reads publishes nothing at all while it
//! stalls — no drain, no queue depth, no refusal count — so the second test
//! has no such fact to wait on and spends a fixed offer budget instead. Its
//! only claim is a ceiling, which a budget can carry: an unbounded envelope
//! passes 16 MiB inside the budget whatever the scheduling. It is deliberately
//! NOT given a floor: giving it one would need a stall the worker publishes,
//! and there is no such thing to wait on, so the floor would be a claim about
//! which thread ran first. That gap is stated on #1333, not papered over.

use std::net::{Ipv4Addr, TcpListener};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use nmp_transport::{Pool, PoolConfig, PoolEvent, WireFrame};

const FRAME_BYTES: usize = 4 * 1024;

/// Attempts, not milliseconds. Enough that an unbounded envelope passes the
/// ceiling long before the budget is spent: at 4 KiB a frame this offers
/// 200 MB against a 16 MiB ceiling. A refused attempt yields so a real worker
/// gets to run.
const SEND_ATTEMPTS: usize = 50_000;

/// Whole redial cycles a worker must complete with every offer refused before
/// [`saturate_reconnecting_worker`] calls its retained set saturated.
///
/// One would already be sound: a worker that took anything at all during a
/// cycle freed a transit slot, and the offer this thread issues microseconds
/// later in a tight loop lands in it. Three removes any dependence on this
/// thread being scheduled inside one particular cycle.
const SATURATION_IDLE_CYCLES: usize = 3;

fn test_pool_config() -> PoolConfig {
    PoolConfig {
        command_queue_capacity: 4,
        reconnect_delay_initial: Some(Duration::from_millis(1)),
        reconnect_jitter_max: Some(Duration::ZERO),
        ..PoolConfig::default()
    }
}

fn test_verifier() -> nmp_transport::Verifier {
    nmp_transport::Verifier::new(
        nmp_transport::VerifyConfig::default(),
        std::sync::Arc::new(nmp_transport::NullKnownSig),
    )
    .expect("test verifier construction must succeed")
}

/// A port nothing listens on: every dial fails at once with `ECONNREFUSED`,
/// which is not a permanent failure, so the slot stays `Connecting` and
/// `Pool::send` keeps handing frames to the worker.
///
/// Ports whose decimal digits contain `401` or `403` are skipped, and that is
/// a real defect this fixture is dodging rather than a superstition:
/// `open_relay_socket` reports a refused dial as `tcp connect
/// 127.0.0.1:59401: Connection refused`, and `backoff::is_permanent_error`
/// looks for `401`/`403` as SUBSTRINGS of that whole string. The address puts
/// them there. About 1% of the ephemeral range therefore reads as an HTTP
/// denial, retires the worker for good, and leaves this test's premise —
/// a worker that keeps redialing — false, at which point only the four
/// transit slots are ever accepted. That is the intermittent failure #1333
/// was filed for. #1335 owns the misclassification; this keeps the fixture
/// off it so the test measures the envelope and not that bug.
fn dead_port() -> u16 {
    for _ in 0..64 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral port");
        let port = listener.local_addr().expect("ephemeral address").port();
        drop(listener);
        let digits = port.to_string();
        if !digits.contains("401") && !digits.contains("403") {
            return port;
        }
    }
    panic!("no ephemeral port outside the 401/403 misclassification in 64 binds")
}

/// Offer ordinary frames to a worker whose every dial fails until its retained
/// set saturates, and return how many frames it is holding at that point.
///
/// Terminates on a fact the worker reaches, never on a clock or an attempt
/// budget. Each failed dial surfaces one [`PoolEvent::Health`], so the events
/// this reads are the worker's own redial cycles; between two of them the
/// worker sits in its reconnect wait draining every queued command into the
/// state it retains. [`SATURATION_IDLE_CYCLES`] consecutive cycles in which
/// this thread offered continuously and nothing was taken therefore mean the
/// transit channel was empty throughout — so the refusals are the envelope's,
/// not the four transit slots'.
///
/// That is what makes the returned count a property of the envelope instead of
/// a race. How slowly the worker dials and how often either thread is
/// scheduled change how many cycles this takes; they cannot change the count
/// it lands on.
///
/// Events queued before an accepted offer are discarded with it: a cycle that
/// overlapped an acceptance says nothing about an idle one.
fn saturate_reconnecting_worker(
    pool: &Pool,
    handle: nmp_transport::RelayHandle,
    events: &mpsc::Receiver<PoolEvent>,
    ceiling: usize,
) -> usize {
    let frame = "x".repeat(FRAME_BYTES);
    let mut accepted = 0usize;
    let mut idle_cycles = 0usize;
    loop {
        if pool.send(handle, WireFrame::Text(frame.clone())) {
            accepted += 1;
            assert!(
                accepted * FRAME_BYTES <= ceiling,
                "the worker accepted {accepted} ordinary frames ({} bytes) \
                 without ever putting one on the wire; retained outbound state \
                 is unbounded (#506)",
                accepted * FRAME_BYTES,
            );
            while events.try_recv().is_ok() {}
            idle_cycles = 0;
            continue;
        }
        thread::yield_now();
        match events.try_recv() {
            Ok(PoolEvent::Health { .. }) => idle_cycles += 1,
            Ok(other) => panic!(
                "this relay must stay in the redial loop that retains frames; \
                 got {other:?} after {accepted} accepted frames"
            ),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                panic!("the pool stopped reporting redials after {accepted} accepted frames")
            }
        }
        if idle_cycles == SATURATION_IDLE_CYCLES {
            return accepted;
        }
    }
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
///
/// Both assertions read the count the worker saturates at, which
/// [`saturate_reconnecting_worker`] establishes from the worker's own redial
/// cycles. Neither is a statement about which of the two threads ran first.
#[test]
fn a_reconnecting_worker_retains_a_bounded_ordinary_outbound_envelope() {
    // Nothing has flushed, so everything accepted is retained. Any honest
    // per-worker envelope is well under this.
    const RETAINED_CEILING_BYTES: usize = 4 * 1024 * 1024;

    let url = nostr::RelayUrl::parse(&format!("ws://127.0.0.1:{}", dead_port()))
        .expect("parse relay url");
    let (tx, rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(test_pool_config(), test_verifier(), tx).expect("test pool construction");
    let handle = pool.ensure_open(&url).expect("admitted");

    let retained = saturate_reconnecting_worker(&pool, handle, &rx, RETAINED_CEILING_BYTES);
    assert!(
        retained >= 8,
        "the envelope must admit a useful working set before refusing (it \
         saturated at {retained} frames); a bound that tight would mean the \
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
    let pool = Pool::new(test_pool_config(), test_verifier(), tx).expect("test pool construction");
    let handle = pool.ensure_open(&url).expect("admitted");

    let accepted = spam_ordinary_frames(&pool, handle, RETAINED_CEILING_BYTES);
    assert!(accepted >= 1, "at least one frame must be admitted");

    pool.shutdown();
    drop(rx);
    peer.thread().unpark();
    let _ = peer.join();
}
