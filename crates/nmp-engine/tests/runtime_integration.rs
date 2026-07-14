//! Runtime (C) integration tests: `Handle`/`EngineThread` driven against a
//! real in-process relay (M3 plan §5 test 6 + the C build brief's
//! end-to-end ask: subscribe -> rows arrive, publish -> receipt acked,
//! reconnect mid-subscription -> subs replayed with no gap). Mirrors
//! `nmp-transport`'s own `tests/mock_relay.rs` pattern -- see that file's
//! doc comment for why `#[tokio::test(flavor = "multi_thread")]` is
//! required even though `EngineThread`/`Pool` themselves impose no runtime
//! on their caller (D8): only `LocalRelay`'s accept loop needs the ambient
//! tokio runtime, the engine/pool machinery under test is plain OS threads
//! + blocking `mpsc` throughout.
//!
//! Deliberately NOT a glob import of `nostr_relay_builder::prelude::*`: that
//! re-exports a DIFFERENT `nostr` (0.45-alpha) than this workspace's pinned
//! `nostr = "0.44.4"`, which would silently shadow the extern-prelude name
//! (see `nmp-transport/tests/mock_relay.rs`'s identical comment). Every
//! cross-version value (keypairs, seeded events) is bridged explicitly by
//! hex/id string round-trip below rather than by sharing a single `Keys`/
//! `Event` type across both crate versions.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use nmp_engine::core::RelayAdmissionPolicy;
use nmp_engine::core::RowDelta;
use nmp_engine::outbox::WriteStatus;
use nmp_engine::runtime::{EngineThread, ReceiptReattachment, RowsMsg};
use nmp_grammar::{Binding, Demand, Derived, Filter, IdentityField, Selector};
use nmp_grammar::{Durability, WriteIntent, WritePayload, WriteRouting};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_signer::LocalKeySigner;
use nmp_store::{
    sentinel_signature, AcceptWrite, EventStore, IntentSigState, MemoryStore, RedbStore,
    RelayObserved, WriteDurability,
};
use nmp_test_support::ConnectionOwner;
use nmp_transport::PoolConfig;
use nostr::{
    EventId, JsonUtil, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Tag, Timestamp,
    UnsignedEvent,
};

use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    Event as RelayEvent, EventBuilder as RelayEventBuilder, FinalizeEvent, Keys as RelayKeys,
    Tag as RelayTag, Timestamp as RelayTimestamp,
};

fn expect_attached(result: ReceiptReattachment) -> Receiver<WriteStatus> {
    match result {
        ReceiptReattachment::Attached(statuses) => statuses,
        ReceiptReattachment::NotFound => panic!("known receipt was not found"),
        ReceiptReattachment::RetainedButUnreadable => {
            panic!("known receipt evidence was unreadable")
        }
    }
}

use tungstenite::Message;

/// Reserve an ephemeral backend port. The reconnect test's client-facing
/// address is owned separately by [`ConnectionOwner`].
fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// Re-derive the identical keypair under `nostr-relay-builder`'s OWN (0.45-
/// alpha) `nostr` dependency, so events seeded directly into the test relay
/// are attributable to the SAME author the engine (0.44.4 `nostr`) knows
/// about. Hex secret-key round-trip is the only safe bridge between the two
/// crate instances (see the module doc).
fn mirror_keys(k: &Keys) -> RelayKeys {
    RelayKeys::parse(&k.secret_key().to_secret_hex())
        .expect("mirror keypair across nostr crate versions")
}

/// A literal (non-reactive) `kinds:[1], authors:[author_hex]` query -- the
/// same shape `integration_capstone.rs`'s own `literal_kind1` uses, needed
/// here by the #39 deadline-driver tests below, which have no reason to
/// exercise the `Derived`/reactive-authors machinery the module's flagship
/// test does.
fn literal_kind1(author_hex: &str) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        ..Filter::default()
    })
}

/// Block (on the calling OS thread -- this crate's `Receiver`s are plain
/// `std::sync::mpsc`, never tokio) until the ACCUMULATED row set (built by
/// replaying every `Added`/`Removed` delta this channel has delivered so
/// far, exactly as a real app must -- `Handle::subscribe`'s wire is deltas,
/// not snapshots, per `nmp_engine::core::RowDelta`'s doc) matches `pred`, or
/// return `false` after `timeout`.
fn wait_for_rows(
    rx: &Receiver<RowsMsg>,
    timeout: Duration,
    pred: impl Fn(&[nostr::Event]) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut current: BTreeMap<EventId, nostr::Event> = BTreeMap::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok((deltas, _coverage)) => {
                for delta in deltas {
                    match delta {
                        RowDelta::Added(row) => {
                            current.insert(row.event.id, row.event);
                        }
                        RowDelta::SourcesGrew { .. } => {}
                        RowDelta::Removed(id) => {
                            current.remove(&id);
                        }
                    }
                }
                let snapshot: Vec<nostr::Event> = current.values().cloned().collect();
                if pred(&snapshot) {
                    return true;
                }
            }
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Same shape as [`wait_for_rows`], for the receipt-status stream.
fn wait_for_status(
    rx: &Receiver<WriteStatus>,
    timeout: Duration,
    pred: impl Fn(&WriteStatus) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(status) if pred(&status) => return true,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

// Multi-thread flavor is load-bearing (mirrors `nmp-transport`'s test 7):
// the test body blocks synchronously on plain `mpsc::Receiver::recv_timeout`
// while `LocalRelay::run` needs the ambient tokio runtime free to accept
// connections and answer REQs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_publish_and_reconnect_replay_over_a_real_relay() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port_a = free_port();

    let a = Keys::generate();
    let b = Keys::generate();
    let b_relay_keys = mirror_keys(&b);

    let relay_a = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port_a)
        .build();
    relay_a.run().await.expect("run relay_a");
    let connection_owner = ConnectionOwner::bind(loopback(0), loopback(port_a))
        .await
        .expect("bind client-facing relay connection owner");
    let public_addr = connection_owner.local_addr();
    let url = RelayUrl::parse(&format!("ws://{public_addr}")).expect("parse relay url");

    // b's post is seeded BEFORE anyone follows b -- store holds it, but it
    // must not surface until a's contact list widens demand to include it
    // (same shape as `core_headless.rs`'s `ingest_frame_recompiles_wire_and_
    // emits_rows`, just driven over a real relay + the full runtime stack
    // instead of scripted `EngineMsg`s).
    let b_post: RelayEvent = RelayEventBuilder::text_note("hello from b, over a real relay")
        .finalize(&b_relay_keys)
        .expect("sign b's post");
    relay_a
        .add_event(b_post.clone())
        .await
        .expect("seed b's post into relay_a");

    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [url.clone()])
        .with_write(b.public_key().to_hex(), [url.clone()]);

    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            reconnect_jitter_max: Some(Duration::ZERO),
            ..PoolConfig::default()
        },
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    handle
        .add_signer(LocalKeySigner::new(a.clone()))
        .expect("local signer has a public key");
    handle.set_active_account(Some(a.public_key()));

    // $myFollows shape: kind:1 authored by whoever `a`'s kind:3 contact
    // list (#p-projected) currently names -- identical shape to M1's own
    // contract-test query and `core_headless.rs`'s analog.
    let my_follows = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    });

    let (_query_handle, rows_rx) = handle
        .subscribe(my_follows)
        .expect("test subscription construction");

    // b's post must NOT be visible yet -- a hasn't followed b.
    assert!(
        !wait_for_rows(&rows_rx, Duration::from_millis(500), |rows| rows
            .iter()
            .any(|r| r.id.to_hex() == b_post.id.to_hex())),
        "b's post must not surface before a follows b"
    );

    // Publish a's contact list naming b. The engine already holds an open
    // REQ for kind:3-by-a at this relay (part of $myFollows's own demand),
    // so once the relay echoes this back live, ingest should widen demand
    // to b's kind:1 and the pre-seeded post should surface.
    let contact_list = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::ContactList,
        vec![Tag::public_key(b.public_key())],
        "",
    );
    let receipt_rx = handle
        .publish(WriteIntent {
            payload: WritePayload::Unsigned(contact_list),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        })
        .expect("receipt id allocation");

    assert!(
        wait_for_status(&receipt_rx, Duration::from_secs(10), |s| matches!(
            s,
            WriteStatus::Acked(r) if r == &url
        )),
        "a durable publish to the seeded relay must reach Acked"
    );

    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| rows
            .iter()
            .any(|r| r.id.to_hex() == b_post.id.to_hex())),
        "b's pre-seeded post must surface once a's contact list names b"
    );

    // -- reconnect: start a fresh relay backend with its own empty database,
    // then synchronously shut down the owner of the exact live TCP stream and
    // rebind the same public address to the new backend. The test never calls
    // `subscribe` again: the only way this new post can reach `rows_rx` is if
    // the production reconnect path replayed the engine's current wire
    // subscriptions onto the new generation.
    let port_b = free_port();
    let relay_b = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port_b)
        .build();
    relay_b.run().await.expect("run relay_b");

    let second_post: RelayEvent = RelayEventBuilder::text_note("b's second post, post-reconnect")
        .finalize(&b_relay_keys)
        .expect("sign b's second post");
    relay_b
        .add_event(second_post.clone())
        .await
        .expect("seed b's second post into relay_b");
    connection_owner
        .shutdown()
        .await
        .expect("sever the exact established relay connection");
    let connection_owner_b = ConnectionOwner::bind(public_addr, loopback(port_b))
        .await
        .expect("rebind the public relay address to relay_b");
    relay_a.shutdown();

    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| rows
            .iter()
            .any(|r| r.id.to_hex() == second_post.id.to_hex())),
        "reconnect must replay the current subs with no gap -- b's post-reconnect note must surface without the app resubscribing"
    );

    handle.shutdown();
    engine_thread.join();
    connection_owner_b
        .shutdown()
        .await
        .expect("shut down relay_b connection owner");
    relay_b.shutdown();
}

// ---- #39: the deadline-armed driver (design §3.3) ------------------------
//
// `EngineThread::spawn`'s `Handle` exposes no manual-tick verb at all (see
// `handle_surface_is_closed_and_receipt_reattachment_is_explicit` below) -- so any
// `RowDelta::Removed` this crate's own tests observe with no further
// command sent can only have come from `runtime::engine_loop`'s own
// `recv_timeout` arming itself off `core::EngineCore::next_deadline()` and
// firing `EngineMsg::Tick` on its own, exactly the property #39 asks for.

/// Wall-clock CPU time consumed by THIS process so far (`getrusage`,
/// user+sys). Used only by [`no_deadlines_blocks_indefinitely`] below --
/// see that test's doc for why this is the one black-box way to falsify a
/// busy-spinning `recv_timeout(0)` loop from outside `nmp-engine`.
fn process_cpu_time() -> Duration {
    // SAFETY: `libc::getrusage` writes into a `libc::rusage` we own and
    // fully overwrite before reading any field back out of it; `usage` is
    // never read before `getrusage` populates it.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        let rc = libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) must succeed");
        let user = Duration::new(
            usage.ru_utime.tv_sec as u64,
            (usage.ru_utime.tv_usec as u32) * 1_000,
        );
        let sys = Duration::new(
            usage.ru_stime.tv_sec as u64,
            (usage.ru_stime.tv_usec as u32) * 1_000,
        );
        user + sys
    }
}

/// #39 test obligation `no_deadlines_blocks_indefinitely`: an engine thread
/// with zero subscriptions has no wire demand, hence no expiring content and
/// no open negentropy session -- `core::EngineCore::next_deadline()` is
/// `None` from the moment it is built, so `engine_loop` must be blocking on
/// a plain `cmd_rx.recv()`, never a hot `recv_timeout(0)` loop (D8). No
/// `Effect` crosses the wire in this scenario (a spurious tick with nothing
/// due produces an empty effect vec -- see `EngineCore::tick`), so the only
/// way to falsify a busy-spin from OUTSIDE the crate is to measure real
/// process CPU consumed across an idle window: blocking `recv()` costs
/// (near) zero CPU no matter how long it waits, while a hot loop would burn
/// roughly one core's worth of CPU time per unit of wall time.
#[test]
fn no_deadlines_blocks_indefinitely() {
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        FixtureDirectory::new(),
        10,
        PoolConfig::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    // Let the engine thread settle onto its idle `recv()` before sampling.
    std::thread::sleep(Duration::from_millis(100));
    let before = process_cpu_time();
    std::thread::sleep(Duration::from_millis(500));
    let after = process_cpu_time();

    assert!(
        after.saturating_sub(before) < Duration::from_millis(150),
        "an engine thread with no deadlines must block on a plain recv() -- \
         a busy-spinning recv_timeout(0) loop would consume CPU on the order \
         of the whole 500ms idle window instead: consumed {:?}",
        after.saturating_sub(before)
    );

    handle.shutdown();
    engine_thread.join();
}

/// A minimal, deliberately UNCOOPERATIVE relay: a real `TcpListener` + a
/// real `tungstenite` WebSocket handshake (the same version `nmp-transport`'s
/// own production `Pool` speaks on the wire), used only by
/// [`neg_liveness_deadline_does_not_busy_spin`] below. `LocalRelay`/
/// `nostr-relay-builder` cannot play this role: it is a fully NIP-77-
/// compliant relay and would answer/converge a real reconciliation long
/// before the 30s liveness window is ever reached, so no real session would
/// ever stay open long enough to exercise the sweep at all.
///
/// On connect it immediately pushes one scripted `EVENT` frame (`seed`) --
/// `EngineCore::on_relay_frame`'s `Event` arm ingests any inbound event
/// unconditionally, with no sub-id check, so this needs no matching REQ to
/// have been sent first. It then replies to exactly the FIRST `NEG-OPEN` it
/// sees (the capability probe) with a `NEG-MSG` -- any payload classifies
/// `Supported`, per `Prober::on_neg_msg`'s own contract -- and goes silent
/// for every `NEG-OPEN` after that while holding the TCP connection open
/// (never closing it, which would just make `EngineCore::on_relay_disconnected`
/// silently drop the session instead of exercising the liveness sweep at
/// all). Every text frame it reads is forwarded to `frames_tx` so the test
/// can observe both halves of the regression: no busy-spin, AND the session
/// actually getting abandoned (a `NEG-CLOSE` + fallback `REQ`) at the
/// deadline.
fn run_uncooperative_neg_relay(
    listener: TcpListener,
    seed: nostr::Event,
    frames_tx: Sender<String>,
) {
    let (stream, _) = listener
        .accept()
        .expect("accept the engine's one connection");
    stream.set_nodelay(true).ok();
    let mut ws = tungstenite::accept(stream).expect("complete the WS handshake");

    let seed_frame = RelayMessage::event(SubscriptionId::new("s"), seed).as_json();
    ws.send(Message::text(seed_frame))
        .expect("push the seed EVENT frame");

    let mut neg_open_count = 0u32;
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let text = text.as_str().to_string();
                if text.contains("\"NEG-OPEN\"") {
                    neg_open_count += 1;
                    if neg_open_count == 1 {
                        if let Some(sub_id) = neg_open_sub_id(&text) {
                            let reply = format!("[\"NEG-MSG\",{sub_id},\"6100\"]");
                            let _ = ws.send(Message::text(reply));
                        }
                    }
                    // The second (and any further) `NEG-OPEN` is the real
                    // widened session -- never reply, never close: exactly
                    // "the relay is slow to answer negentropy", the
                    // scenario the liveness sweep exists for.
                }
                let _ = frames_tx.send(text);
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// Extract `sub_id` (still JSON-quoted, ready to splice straight back into
/// a reply array) from a `["NEG-OPEN", sub_id, filter, hex]` wire frame.
fn neg_open_sub_id(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let arr = value.as_array()?;
    if arr.first()?.as_str()? != "NEG-OPEN" {
        return None;
    }
    serde_json::to_string(arr.get(1)?.as_str()?).ok()
}

/// Sleep (short increments, not a hot spin) until real wall-clock has JUST
/// crossed a fresh whole-second boundary, within `margin` of it.
///
/// Used only to make [`neg_liveness_deadline_does_not_busy_spin`]'s timing
/// deterministic. `duration_until` always computes a CLEAN whole-second
/// `recv_timeout` duration (`deadline.as_secs() - now.as_secs()`, both
/// floored) -- so sleeping that duration wakes the loop up at very nearly
/// the SAME sub-second phase it started from (a clean N-second sleep
/// preserves phase; only scheduler wake-up jitter, a few ms, perturbs it).
/// That means the fraction of a second left over when a stale negentropy
/// session's deadline is finally reached -- i.e. how long the pre-fix
/// busy-spin lasts before wall-clock ticks into the next whole second --
/// is inherited almost unchanged from whatever phase real wall-clock
/// happened to be at the moment `open_neg_session` captured `started_at`.
/// Left uncontrolled, that phase is effectively random each run: the spin
/// could as easily last 900ms as 10ms, making a fixed CPU-time threshold
/// flaky in EITHER direction. Aligning to near-zero phase right before the
/// widen-`subscribe` that opens the session (which itself resolves in low
/// milliseconds once sent, since the engine thread is parked on a plain
/// `recv()` with nothing else pending at that point) means a real spin, if
/// one occurs at all, reliably lasts close to the full ~1 second instead.
fn align_to_next_second_boundary(margin: Duration) {
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must be past the Unix epoch");
        let into_current_second = Duration::from_nanos((now.as_nanos() % 1_000_000_000) as u64);
        if into_current_second <= margin {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Block until a frame containing `needle` arrives on `frames_rx`, or panic
/// after `timeout` -- draining (and discarding) every non-matching frame
/// along the way, same discipline as `nmp-transport`'s own `recv_matching`.
fn wait_for_frame_containing(frames_rx: &Receiver<String>, timeout: Duration, needle: &str) {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for a frame containing {needle:?}"
        );
        match frames_rx.recv_timeout(remaining) {
            Ok(text) if text.contains(needle) => return,
            Ok(_) => {} // non-matching frame -- keep waiting.
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {needle:?}"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("stub relay's frame channel closed while waiting for {needle:?}")
            }
        }
    }
}

/// #39 fix-up regression (review finding on PR #42): the neg-liveness
/// sweep predicate used to compare `now.as_secs().saturating_sub(started_
/// at.as_secs()) > NEG_LIVENESS_DEADLINE_SECS` (strict `>`, truncated to
/// whole seconds) against a DIFFERENT threshold than the one `next_deadline`
/// arms the driver for (the exact `Timestamp` `started_at +
/// NEG_LIVENESS_DEADLINE_SECS`). At `now == started_at + 30` that mismatch
/// meant: `next_deadline()` still returns the same deadline, `duration_until`
/// floors it to `Duration::ZERO`, `recv_timeout(0)` times out immediately,
/// `tick()` runs, the sweep's strict `>` is still false (`30 > 30` is
/// false) so the session survives, and the loop goes straight back around
/// to the identical zero-duration timeout -- a hot `recv_timeout(0)` spin
/// burning roughly one core until the wall clock ticks over into the NEXT
/// whole second (`as_secs()` finally reading `31 > 30`). Only a negentropy
/// session crossing ITS liveness deadline hits this; NIP-40 expiry never did
/// (`expire_due`'s own `range(..=now)` was already inclusive), which is why
/// `no_deadlines_blocks_indefinitely` (zero deadlines at all) didn't catch
/// it. Fixed by comparing the identical threshold both places (`now >=
/// started_at + NEG_LIVENESS_DEADLINE_SECS`).
///
/// An unrelated, short NIP-40 expiry is driven FIRST (the `seed` event
/// [`run_uncooperative_neg_relay`] pushes on connect) so `EngineCore`'s
/// internal clock -- which `core::mod`'s `self.clock` shows is ONLY ever
/// advanced by `tick()`, never by an ordinary ingest/EOSE -- is synced close
/// to real wall-clock time before the neg session opens. Skip that step and
/// the very first session any fresh engine ever opens starts from
/// `started_at == 0` (the `EngineCore::new` default, Unix epoch): its
/// liveness deadline (`30`) would already be enormously in the past relative
/// to real time, so `now_real_secs - 0` trivially and identically exceeds
/// 30 under EITHER the buggy or the fixed predicate -- masking the exact
/// ~1-second-window regression this test exists to catch.
#[test]
fn neg_liveness_deadline_does_not_busy_spin() {
    let port = free_port();
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind stub relay port");

    let a = Keys::generate();
    let b = Keys::generate();
    // Built with this crate's OWN (0.44.4) `nostr` types directly -- unlike
    // the other tests in this file, this stub speaks the workspace's native
    // wire format itself (via `RelayMessage::event`, not `nostr-relay-
    // builder`), so there is no cross-version bridge to cross here.
    let now_secs = Timestamp::now().as_secs();
    let seed = nmp_resolver::testkit::expiring_kind1(
        &a,
        "syncs EngineCore's clock before the neg session opens",
        now_secs,
        now_secs + 2,
    );
    let seed_id = seed.id;

    let (frames_tx, frames_rx) = mpsc::channel::<String>();
    let stub = thread::spawn(move || run_uncooperative_neg_relay(listener, seed, frames_tx));

    let url = RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("parse stub relay url");
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [url.clone()])
        .with_write(b.public_key().to_hex(), [url]);

    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            // The stub never disconnects and this test never kills it early,
            // so no reconnect should ever fire -- a long delay just keeps
            // the default reconnect machinery out of the way.
            reconnect_delay_initial: Some(Duration::from_secs(3600)),
            ..PoolConfig::default()
        },
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    let (_qh_a, rows_rx) = handle
        .subscribe(literal_kind1(&a.public_key().to_hex()))
        .expect("test subscription construction");

    // The seed's `Added` then `Removed` (with zero further commands sent --
    // same proof shape as `expiring_event_retracts_with_no_further_input`)
    // confirms a real `tick()` has now run at a wall-clock time close to
    // "now", so `EngineCore`'s internal clock is synced.
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| rows
            .iter()
            .any(|r| r.id.to_hex() == seed_id.to_hex())),
        "the seed event must arrive as Added first"
    );
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| !rows
            .iter()
            .any(|r| r.id.to_hex() == seed_id.to_hex())),
        "the seed event must retract on its own -- this is what syncs \
         EngineCore's clock close to real wall-clock time"
    );

    // Align phase (see `align_to_next_second_boundary`'s doc) right before
    // the widen so `started_at` -- captured a few milliseconds later --
    // lands close to a fresh whole-second boundary, making a real spin (if
    // one occurs) last close to the full ~1 second rather than an
    // unpredictable fraction of one.
    align_to_next_second_boundary(Duration::from_millis(30));

    // Now open the real negentropy session: b's kind:1 widens the same
    // (kind:1) skeleton under the sub-id the capability probe already
    // proved `Supported` -- same probe-then-widen dance the headless tests
    // use, just driven over the real wire this time.
    let (_qh_b, _rows_rx_b) = handle
        .subscribe(literal_kind1(&b.public_key().to_hex()))
        .expect("test subscription construction");
    wait_for_frame_containing(&frames_rx, Duration::from_secs(10), "\"NEG-OPEN\"");
    // The first `NEG-OPEN` (the capability probe) already arrived before
    // this subscribe -- the second is the real, now-open session this test
    // holds open. `neg_open_sub_id`'s own count inside the stub thread is
    // what actually distinguishes them; here we just need real time to have
    // moved on enough that the widen has landed, which the SECOND
    // occurrence proves.
    wait_for_frame_containing(&frames_rx, Duration::from_secs(10), "\"NEG-OPEN\"");

    // The session is now open with `started_at` close to a fresh whole-
    // second boundary (per the phase alignment above). Measure CPU across a
    // window that comfortably straddles its `started_at + 30` liveness
    // deadline either way: a pre-fix busy-spin burns roughly one core for
    // close to a full second inside this window; blocking `recv`/
    // `recv_timeout` costs (near) zero no matter how long any of it waits.
    let before = process_cpu_time();
    thread::sleep(Duration::from_secs(33));
    let after = process_cpu_time();
    assert!(
        after.saturating_sub(before) < Duration::from_millis(400),
        "the neg-liveness deadline crossing must not busy-spin -- a \
         recv_timeout(0) hot loop stuck on the pre-fix off-by-one would \
         burn close to a full core-second inside this window: consumed {:?}",
        after.saturating_sub(before)
    );

    // Companion assertion: the session was actually abandoned at the
    // deadline (not just "nothing happened at all", which would also show
    // zero CPU but would be a false pass) -- the fallback-to-REQ effect
    // closes the negentropy sub-id then reopens it as a plain REQ.
    wait_for_frame_containing(&frames_rx, Duration::from_secs(5), "\"NEG-CLOSE\"");
    wait_for_frame_containing(&frames_rx, Duration::from_secs(5), "\"REQ\"");

    handle.shutdown();
    engine_thread.join();
    let _ = stub.join();
}

/// #39 test obligation `expiring_event_retracts_with_no_further_input`:
/// insert an event expiring soon via the NORMAL path (a real relay echoing
/// it back, exactly like every other row this crate's tests ingest), then
/// prove it retracts (`RowDelta::Removed`) with zero further commands sent
/// -- no manual tick exists on `Handle` to fake it with, so this can only be
/// the `recv_timeout` driver firing `EngineMsg::Tick` on its own.
///
/// NIP-40 `expiration` is second-resolution (not millisecond -- `Timestamp`
/// itself is `u64` seconds), so "soon" here is `now + 2` rather than the
/// issue's illustrative "~100ms"; the property under test (fires with no
/// further input, not on any fixed cadence) is identical at either
/// granularity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiring_event_retracts_with_no_further_input() {
    let port = free_port();
    let a = Keys::generate();
    let a_relay_keys = mirror_keys(&a);

    let relay = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay.run().await.expect("run relay");
    let url = RelayUrl::parse(&relay.url().await.to_string()).expect("parse relay url");

    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [url.clone()]);
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    let (_qh, rows_rx) = handle
        .subscribe(literal_kind1(&a.public_key().to_hex()))
        .expect("test subscription construction");

    let expiring: RelayEvent = RelayEventBuilder::text_note("expires soon, over a real relay")
        .tag(RelayTag::expiration(RelayTimestamp::now() + 2))
        .finalize(&a_relay_keys)
        .expect("sign a's expiring post");
    relay
        .add_event(expiring.clone())
        .await
        .expect("live-push a's expiring post");

    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| rows
            .iter()
            .any(|r| r.id.to_hex() == expiring.id.to_hex())),
        "the expiring note must arrive as Added first, over the normal relay-echo path"
    );

    // No further command is ever sent from here -- only the driver's own
    // `recv_timeout` can produce what happens next.
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| !rows
            .iter()
            .any(|r| r.id.to_hex() == expiring.id.to_hex())),
        "the deadline-armed driver must retract the expired note on its own, \
         with no further command ever sent"
    );

    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}

/// #39 test obligation `earlier_expiration_from_ingest_rearms`: a far-future
/// expiry is ingested first (arming the driver's `recv_timeout` for roughly
/// an hour out), then a near one arrives for the SAME subscription -- the
/// near one must still retract promptly. If the loop only ever armed once
/// (using the stale far-future deadline) rather than recomputing
/// `next_deadline()` on every iteration, the near expiry would never fire
/// within this test's bounded wait and the assertion would time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn earlier_expiration_from_ingest_rearms() {
    let port = free_port();
    let a = Keys::generate();
    let a_relay_keys = mirror_keys(&a);

    let relay = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay.run().await.expect("run relay");
    let url = RelayUrl::parse(&relay.url().await.to_string()).expect("parse relay url");

    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [url.clone()]);
    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    let (_qh, rows_rx) = handle
        .subscribe(literal_kind1(&a.public_key().to_hex()))
        .expect("test subscription construction");

    let far: RelayEvent = RelayEventBuilder::text_note("expires in about an hour")
        .tag(RelayTag::expiration(RelayTimestamp::now() + 3_600))
        .finalize(&a_relay_keys)
        .expect("sign a's far-future post");
    relay
        .add_event(far.clone())
        .await
        .expect("live-push the far-future post");
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| rows
            .iter()
            .any(|r| r.id.to_hex() == far.id.to_hex())),
        "the far-future post must arrive first (arms next_deadline ~an hour out)"
    );

    let near: RelayEvent = RelayEventBuilder::text_note("expires very soon")
        .tag(RelayTag::expiration(RelayTimestamp::now() + 2))
        .finalize(&a_relay_keys)
        .expect("sign a's near-future post");
    relay
        .add_event(near.clone())
        .await
        .expect("live-push the near-future post");
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| rows
            .iter()
            .any(|r| r.id.to_hex() == near.id.to_hex())),
        "the near-future post must arrive Added too"
    );

    // The near expiry firing at all within this bounded wait IS the proof
    // of rearming -- a driver stuck on the far-future deadline would leave
    // this timed out (false) for the length of the test, not merely slow.
    // (`wait_for_rows` starts its accumulator fresh on each call -- `far`'s
    // own `Added` was already drained by an earlier call above and will
    // never be redelivered, so this call only re-asserts `near`'s absence;
    // `far` surviving is structural, not re-checked here -- its expiration
    // is ~an hour out and nothing else in this test could retract it.)
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| !rows
            .iter()
            .any(|r| r.id.to_hex() == near.id.to_hex())),
        "ingesting a nearer expiration must re-arm the driver off the NEW \
         next_deadline, not the stale far-future one it started with"
    );

    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}

/// #39 test obligation `boot_catches_up_past_due_expiry`: an expiring event
/// is persisted to a real on-disk `RedbStore` while still valid, the process
/// "restarts" (the store is closed and reopened, same pattern as
/// `integration_capstone.rs`'s `watermark_cold_start_offline`), and enough
/// wall-clock time passes offline that its expiration is already past BEFORE
/// `EngineThread::spawn` ever runs. The very first loop iteration must still
/// catch it up: `next_deadline()` reads the persisted index and returns a
/// deadline already in the past, `duration_until` floors that to
/// `Duration::ZERO`, and the immediate timeout fires `Tick` before any
/// command (including this test's own `subscribe`) is guaranteed to have
/// been processed -- proven here by subscribing to BOTH the expired row and
/// a control row from the same author and asserting only the control
/// survives.
#[test]
fn boot_catches_up_past_due_expiry() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("boot_catch_up.redb");

    // ---- build the persisted state directly (no engine thread yet) -------
    let expiring_id;
    let control_id;
    {
        let mut store = RedbStore::open(&db_path).expect("open redb store (build phase)");
        let expiring =
            nmp_resolver::testkit::expiring_kind1(&a, "expires almost immediately", 100, 101);
        let control = nmp_resolver::testkit::kind1(&a, "a plain, non-expiring note", 100);
        expiring_id = expiring.id;
        control_id = control.id;
        let observed = RelayObserved::new(relay0.clone(), Timestamp::from(100u64));
        store.insert(expiring, observed.clone()).unwrap();
        store.insert(control, observed).unwrap();
        // `store` drops here -- redb flushes/closes on drop, same as
        // `watermark_cold_start_offline`'s own phase boundary.
    }

    // Real wall-clock time must pass so the persisted deadline (101) is
    // genuinely in the past by the time the engine boots -- `expire_due`
    // works off wall time via `EngineMsg::Tick(Timestamp::now())`, not the
    // fixture's synthetic seconds.
    std::thread::sleep(Duration::from_secs(2));

    // ---- "restart": reopen the SAME file, spawn a fresh engine thread ----
    let store = RedbStore::open(&db_path).expect("reopen redb store (boot phase)");
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0]);
    let (engine_thread, handle) = EngineThread::spawn(
        store,
        dir,
        10,
        PoolConfig {
            // No real relay is ever reachable at `relay0` in this test --
            // a long reconnect delay just keeps background dial attempts
            // out of the way (same rationale as `watermark_cold_start_
            // offline`'s phase 2).
            reconnect_delay_initial: Some(Duration::from_secs(3600)),
            ..PoolConfig::default()
        },
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    let (_qh, rows_rx) = handle
        .subscribe(literal_kind1(&a.public_key().to_hex()))
        .expect("test subscription construction");

    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| {
            let ids: BTreeSet<EventId> = rows.iter().map(|r| r.id).collect();
            ids.contains(&control_id) && !ids.contains(&expiring_id)
        }),
        "a deadline already past at boot must retract on the very first loop \
         iteration -- the control row must survive, the expired row must not"
    );

    handle.shutdown();
    engine_thread.join();
}

/// Structural grep-guard (M3 plan §5 test 14, widened by M4/M5 and #3 U4):
/// `Handle`'s public surface is the original verbs plus diagnostics and the
/// two stable-receipt operations (`publish_tracked`/`reattach_receipt`) and
/// the governed sign-only operation's blocking/completion doors -- no
/// `relays:` parameter, no open-REQ method anywhere on it
/// (ledger #2/#3 preserved at the top edge; `add_signer`/`remove_signer` are
/// M4's deliberate lifecycle widening, closing the multi-account and remote
/// signer detach gaps; `observe_diagnostics` is M5's --
/// read-only, off the data path, never influences routing/delivery). Asserted
/// by reading this crate's own source rather than by reflection (Rust has
/// none) -- the same "grep-guard" idiom the plan itself names.
#[test]
fn handle_surface_is_closed_and_receipt_reattachment_is_explicit() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime/mod.rs"))
        .expect("read runtime/mod.rs");

    let impl_block_start = src
        .find("impl Handle {")
        .expect("Handle must have an impl block");
    let handle_impl = &src[impl_block_start..];
    let impl_block_end = handle_impl
        .find("\n}\n")
        .expect("Handle's impl block must close");
    let handle_impl = &handle_impl[..impl_block_end];

    let mut methods: Vec<&str> = Vec::new();
    for line in handle_impl.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pub fn ") {
            let name = rest.split(['(', '<']).next().unwrap_or_default();
            methods.push(name);
        }
    }
    methods.sort_unstable();
    let mut expected = vec![
        "add_signer",
        "observe_diagnostics",
        "publish",
        "publish_tracked",
        "reattach_receipt",
        "relay_information",
        "remove_signer",
        "set_active_account",
        "shutdown",
        "sign_event",
        "sign_event_with_completion",
        "subscribe",
        "unsubscribe",
    ];
    expected.sort_unstable();
    assert_eq!(
        methods, expected,
        "Handle must expose only the reviewed verbs -- no relays:/open-REQ method"
    );

    // Scan CODE lines only (skip `///`/`//` doc/comment prose, which is
    // free to describe the absence of these things in words) for the actual
    // structural violations: a `relays:` parameter or an open-REQ method.
    let code_lines: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .collect();
    assert!(
        !code_lines.iter().any(|l| l.contains("relays:")),
        "no method signature on the runtime surface may take a bare `relays:` parameter"
    );
    assert!(
        !code_lines
            .iter()
            .any(|l| l.contains("fn open_req") || l.contains("fn open(")),
        "no open-REQ method may exist anywhere in the runtime module"
    );
}

#[test]
fn runtime_exposes_stable_receipt_id_and_supports_multiple_reattach_observers() {
    let keys = Keys::generate();
    let (thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        FixtureDirectory::new(),
        10,
        PoolConfig::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    handle.set_active_account(Some(keys.public_key()));
    let tracked = handle
        .publish_tracked(WriteIntent {
            payload: WritePayload::Unsigned(UnsignedEvent::new(
                keys.public_key(),
                Timestamp::now(),
                Kind::TextNote,
                vec![],
                "tracked",
            )),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        })
        .expect("receipt id allocation");
    assert!(
        tracked.id.0 < (1u64 << 63),
        "accepted ids use store namespace"
    );
    assert_eq!(tracked.statuses.recv().unwrap(), WriteStatus::Accepted);

    let first = expect_attached(handle.reattach_receipt(tracked.id));
    let second = expect_attached(handle.reattach_receipt(tracked.id));
    assert_eq!(
        first.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteStatus::Accepted
    );
    assert_eq!(
        first.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteStatus::AwaitingCapability
    );
    assert_eq!(
        second.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteStatus::Accepted
    );
    assert_eq!(
        second.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteStatus::AwaitingCapability
    );
    handle
        .add_signer(LocalKeySigner::new(keys.clone()))
        .expect("local signer has a public key");
    assert!(wait_for_status(
        &first,
        Duration::from_secs(2),
        |status| matches!(status, WriteStatus::Signed(_))
    ));
    assert!(wait_for_status(
        &second,
        Duration::from_secs(2),
        |status| matches!(status, WriteStatus::Signed(_))
    ));
    assert!(matches!(
        handle.reattach_receipt(nmp_engine::core::ReceiptId(999_999)),
        ReceiptReattachment::NotFound
    ));

    handle.shutdown();
    thread.join();
}

#[test]
fn runtime_boot_recovery_precedes_first_reattach_command() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("boot-before-command.redb");
    let keys = Keys::generate();
    let unsigned = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "boot first",
    );
    let id = EventId::new(
        &unsigned.pubkey,
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );
    let receipt = {
        let mut store = RedbStore::open(&path).unwrap();
        let outcome = store
            .accept_write(AcceptWrite {
                frozen: nostr::Event::new(
                    id,
                    unsigned.pubkey,
                    unsigned.created_at,
                    unsigned.kind,
                    unsigned.tags,
                    unsigned.content,
                    sentinel_signature(),
                ),
                replaceable_base: None,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: keys.public_key().to_hex(),
                durability: WriteDurability::Durable,
                routing: "author-outbox".into(),
                sig_state: IntentSigState::AwaitingSigner,
                accepted_at: Timestamp::now(),
            })
            .unwrap();
        nmp_engine::core::ReceiptId(outcome.journaled_receipt_id().unwrap())
    };
    let (thread, handle) = EngineThread::spawn(
        RedbStore::open(&path).unwrap(),
        FixtureDirectory::new(),
        10,
        PoolConfig::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");
    // This is literally the first command sent to the new engine thread.
    let statuses = expect_attached(handle.reattach_receipt(receipt));
    assert_eq!(
        statuses.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteStatus::Accepted
    );
    assert_eq!(
        statuses.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteStatus::AwaitingCapability
    );
    handle
        .add_signer(LocalKeySigner::new(keys))
        .expect("local signer has a public key");
    assert!(wait_for_status(
        &statuses,
        Duration::from_secs(2),
        |status| matches!(status, WriteStatus::Signed(event_id) if *event_id == id)
    ));
    handle.shutdown();
    thread.join();
}
