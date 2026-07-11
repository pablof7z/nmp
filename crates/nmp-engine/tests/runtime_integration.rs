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
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use nmp_engine::core::RowDelta;
use nmp_engine::outbox::{Durability, WriteIntent, WritePayload, WriteRouting, WriteStatus};
use nmp_engine::runtime::{EngineThread, RowsMsg};
use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector, TagName};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_signer::LocalKeySigner;
use nmp_store::{EventStore, MemoryStore, RedbStore, RelayObserved};
use nmp_transport::PoolConfig;
use nostr::{EventId, Keys, Kind, RelayUrl, Tag, Timestamp, UnsignedEvent};

use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    Event as RelayEvent, EventBuilder as RelayEventBuilder, FinalizeEvent, Keys as RelayKeys,
    Tag as RelayTag, Timestamp as RelayTimestamp,
};

/// Reserve an ephemeral TCP port by binding then immediately dropping the
/// listener, so a *second* relay instance (the reconnect half of the test)
/// can rebind the exact same port the first one used.
fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
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
    LiveQuery(Filter {
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
                        RowDelta::Added(event) => {
                            current.insert(event.id, event);
                        }
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
    let port = free_port();

    let a = Keys::generate();
    let b = Keys::generate();
    let b_relay_keys = mirror_keys(&b);

    let relay_a = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay_a.run().await.expect("run relay_a");
    let url = RelayUrl::parse(&relay_a.url().await.to_string()).expect("parse relay url");

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
            ..PoolConfig::default()
        },
    );

    handle.add_signer(LocalKeySigner::new(a.clone()));
    handle.set_active_account(Some(a.public_key()));

    // $myFollows shape: kind:1 authored by whoever `a`'s kind:3 contact
    // list (#p-projected) currently names -- identical shape to M1's own
    // contract-test query and `core_headless.rs`'s analog.
    let my_follows = LiveQuery(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            project: Selector::Tag(TagName::new('p').unwrap()),
        }))),
        ..Filter::default()
    });

    let (_query_handle, rows_rx) = handle.subscribe(my_follows);

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
    let receipt_rx = handle.publish(WriteIntent {
        payload: WritePayload::Unsigned(contact_list),
        durability: Durability::Durable,
        routing: WriteRouting::AuthorOutbox,
    });

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

    // -- reconnect: kill relay_a, rebind a fresh relay_b on the SAME port
    // (a fresh instance has its own, empty database -- exactly like
    // `nmp-transport`'s own test 7), then seed a NEW post from b directly
    // into it. The test never calls `subscribe` again: the only way this
    // new post can reach `rows_rx` is if the reconnect replayed the engine's
    // current wire subs (both kind:3-by-a and kind:1-by-b) onto the new
    // generation -- test 6's exact falsifier, driven through the full
    // Handle/EngineThread stack instead of raw `Pool`.
    relay_a.shutdown();
    let relay_b = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    tokio::time::sleep(Duration::from_millis(150)).await;
    relay_b.run().await.expect("run relay_b");

    let second_post: RelayEvent = RelayEventBuilder::text_note("b's second post, post-reconnect")
        .finalize(&b_relay_keys)
        .expect("sign b's second post");
    relay_b
        .add_event(second_post.clone())
        .await
        .expect("seed b's second post into relay_b");

    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(15), |rows| rows
            .iter()
            .any(|r| r.id.to_hex() == second_post.id.to_hex())),
        "reconnect must replay the current subs with no gap -- b's post-reconnect note must surface without the app resubscribing"
    );

    handle.shutdown();
    engine_thread.join();
    relay_b.shutdown();
}

// ---- #39: the deadline-armed driver (design §3.3) ------------------------
//
// `EngineThread::spawn`'s `Handle` exposes no manual-tick verb at all (see
// `handle_surface_is_exactly_five_verbs_plus_shutdown` below) -- so any
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
    );

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
    );

    let (_qh, rows_rx) = handle.subscribe(literal_kind1(&a.public_key().to_hex()));

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
    );

    let (_qh, rows_rx) = handle.subscribe(literal_kind1(&a.public_key().to_hex()));

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
        store.insert(expiring, observed.clone());
        store.insert(control, observed);
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
    );

    let (_qh, rows_rx) = handle.subscribe(literal_kind1(&a.public_key().to_hex()));

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

/// Structural grep-guard (M3 plan §5 test 14, widened by M4 §5 and M5):
/// `Handle`'s public surface is exactly the five verbs (`subscribe`/
/// `unsubscribe`/`add_signer`/`set_active_account`/`publish`) plus
/// `shutdown` -- no `relays:` parameter, no open-REQ method anywhere on it
/// (ledger #2/#3 preserved at the top edge; `add_signer` is M4's deliberate
/// widening, closing the multi-account gap; `observe_diagnostics` is M5's --
/// read-only, off the data path, never influences routing/delivery). Asserted
/// by reading this crate's own source rather than by reflection (Rust has
/// none) -- the same "grep-guard" idiom the plan itself names.
#[test]
fn handle_surface_is_exactly_five_verbs_plus_shutdown() {
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
        "set_active_account",
        "shutdown",
        "subscribe",
        "unsubscribe",
    ];
    expected.sort_unstable();
    assert_eq!(
        methods, expected,
        "Handle must expose exactly the five verbs + shutdown -- no relays:/open-REQ method"
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
