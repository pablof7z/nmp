//! M3 Step F — the integration capstone: the full falsifier suite driven
//! against a live in-process relay, headlined by [`watermark_cold_start_offline`]
//! (plan `docs/plans/M3-store-transport-outbox-plan.md` §5 test 9, THE M3
//! pass criterion — ledger #7, "cache-miss treated as empty"). The other
//! three tests here round out the LIVE tier of the remaining ledger
//! falsifiers that weren't already exercised end-to-end: #5 (provenance/
//! dedup across two relays), #9 (enqueue != converged, per-relay ack split),
//! and the depth-2 grammar (`SetOp(Diff, …)`, M1 contract test 9's shape)
//! actually resolving over a real relay rather than scripted `EngineMsg`s.
//!
//! Same version-shadowing precaution as `runtime_integration.rs`/
//! `negentropy_live.rs`: never `use nostr_relay_builder::prelude::*` (it
//! re-exports a DIFFERENT `nostr` than this workspace's pinned `0.44.4`);
//! every cross-version value is bridged by explicit hex/id round-trip.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use nmp_engine::core::{QueryCoverage, RowDelta};
use nmp_engine::outbox::{Durability, WriteIntent, WritePayload, WriteRouting, WriteStatus};
use nmp_engine::runtime::{EngineThread, RowsMsg};
use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector, SetAlgebra, SetOp};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_signer::LocalKeySigner;
use nmp_store::RedbStore;
use nmp_transport::PoolConfig;
use nostr::{EventId, Keys, Kind, RelayUrl, Tag, Timestamp, UnsignedEvent};

use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    BoxedFuture, Event as RelayEvent, EventBuilder as RelayEventBuilder, FinalizeEvent,
    Keys as RelayKeys, MachineReadablePrefix, WritePolicy, WritePolicyResult,
};

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

fn mirror_keys(k: &Keys) -> RelayKeys {
    RelayKeys::parse(&k.secret_key().to_secret_hex())
        .expect("mirror keypair across nostr crate versions")
}

fn literal_kind1(author_hex: &str) -> LiveQuery {
    LiveQuery(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        ..Filter::default()
    })
}

/// Accumulates the channel's `Added`/`Removed` deltas into the row set they
/// currently describe (exactly as a real app must -- `Handle::subscribe`'s
/// wire is deltas, not snapshots, per `nmp_engine::core::RowDelta`'s doc) and
/// blocks until that accumulated set + the latest coverage satisfy `pred`,
/// or `timeout` lapses. Replaying `Removed` deltas (not just tracking "ever
/// added") is load-bearing for `follows_minus_mutes_resolves_over_a_real_
/// relay` below, whose predicate needs the settled CURRENT membership, not
/// a monotonic history.
fn wait_for_rows(
    rx: &Receiver<RowsMsg>,
    timeout: Duration,
    pred: impl Fn(&[nostr::Event], QueryCoverage) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut current: BTreeMap<EventId, nostr::Event> = BTreeMap::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok((deltas, coverage)) => {
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
                if pred(&snapshot, coverage) {
                    return true;
                }
            }
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

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

fn spawn_relay(port: u16) -> LocalRelay {
    LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build()
}

// ===========================================================================
// THE FLAGSHIP: watermark_cold_start_offline (plan §5 test 9, ledger #7)
// ===========================================================================

/// Phase 1 (online): subscribe against a real relay, wait for the plain
/// REQ/EOSE round trip to land 3 seeded events AND the query's coverage to
/// read `CompleteUpTo` (an authoritative-complete watermark, persisted to
/// the `RedbStore` file). Phase 2 (offline): shut the relay down, spawn a
/// brand-new engine on the SAME redb file, subscribe the SAME query again.
/// The FIRST batch on the fresh subscription is computed entirely inside
/// `EngineCore::on_subscribe` (`recompile` + `refresh_handle`) -- both pure,
/// no I/O -- so it is available with zero network round trips: this test
/// asserts that batch already shows the 3 persisted rows AND
/// `CompleteUpTo`, never `Unknown`, proving the watermark survived the
/// restart and makes a cold, offline read authoritative rather than a
/// (wrongly) empty cache-miss.
///
/// A second, never-queried-before shape (kind:1 authored by `b`, whose
/// write relay is registered up front but has no coverage row) is asserted
/// to read `Unknown` in the SAME offline engine -- the falsifier's other
/// half: "no row = not covered" must still hold, offline, distinguishing a
/// genuine unknown from a proven-empty watermark. If either half regresses
/// (offline reads `Unknown` when it should be `CompleteUpTo`, or a
/// never-synced shape reads `CompleteUpTo` when it should be `Unknown`),
/// ledger #7 is not real and this assertion fails loudly rather than being
/// softened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watermark_cold_start_offline() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port = free_port();

    let a = Keys::generate();
    let b = Keys::generate();
    let a_relay_keys = mirror_keys(&a);

    let relay = spawn_relay(port);
    relay.run().await.expect("run relay");
    let url = RelayUrl::parse(&relay.url().await.to_string()).expect("parse relay url");

    // Seed 3 of `a`'s own posts directly into the relay's database BEFORE
    // anyone subscribes -- the plain REQ/EOSE round trip (not negentropy;
    // this relay is not yet known `Supported`, same bootstrap-ordering note
    // as `negentropy_live.rs`) must fetch and prove all 3.
    let posts: Vec<RelayEvent> = (0..3)
        .map(|i| {
            RelayEventBuilder::text_note(format!("cold-start post #{i}"))
                .finalize(&a_relay_keys)
                .expect("sign a's post")
        })
        .collect();
    for post in &posts {
        relay
            .add_event(post.clone())
            .await
            .expect("seed a's post into the relay");
    }
    let post_ids: BTreeSet<String> = posts.iter().map(|p| p.id.to_hex()).collect();

    // `b` is registered in the SAME directory (write relay = the same, soon
    // to be dead, url) but is never queried in phase 1 -- its shape has no
    // coverage row anywhere, which is exactly what makes it the "no row =
    // Unknown" control case in phase 2.
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [url.clone()])
        .with_write(b.public_key().to_hex(), [url.clone()]);

    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("cold_start.redb");

    // ---- Phase 1: online -------------------------------------------------
    {
        let store = RedbStore::open(&db_path).expect("open redb store (phase 1)");
        let (engine_thread, handle) = EngineThread::spawn(
            store,
            dir.clone(),
            10,
            PoolConfig {
                reconnect_delay_initial: Some(Duration::from_millis(20)),
                ..PoolConfig::default()
            },
        );

        let (_qh, rows_rx) = handle.subscribe(literal_kind1(&a.public_key().to_hex()));

        assert!(
            wait_for_rows(&rows_rx, Duration::from_secs(10), |rows, coverage| {
                let ids: BTreeSet<String> = rows.iter().map(|r| r.id.to_hex()).collect();
                ids == post_ids && matches!(coverage, QueryCoverage::CompleteUpTo(_))
            }),
            "phase 1 (online) must fetch all 3 seeded posts and reach CompleteUpTo via a real EOSE"
        );

        handle.shutdown();
        engine_thread.join();
    }

    // ---- Offline: kill the relay, zero relays reachable from here on -----
    relay.shutdown();

    // ---- Phase 2: cold, offline restart on the SAME redb file ------------
    {
        let store = RedbStore::open(&db_path).expect("reopen redb store (phase 2, offline)");
        let (engine_thread, handle) = EngineThread::spawn(
            store,
            dir.clone(),
            10,
            // A long reconnect delay: the relay is gone, so background
            // reconnect attempts against the dead port would otherwise
            // retry on a tight loop for the test's duration -- irrelevant
            // to correctness (subscribe's first batch is computed purely
            // from the local store + router plan, no network involved) but
            // this keeps the test's own log output/CPU usage sane.
            PoolConfig {
                reconnect_delay_initial: Some(Duration::from_secs(3600)),
                ..PoolConfig::default()
            },
        );

        // THE assertion: a's shape reads back from cache, offline, as
        // CompleteUpTo -- never Unknown -- with zero network round trips
        // (this batch is available the instant `subscribe` returns; the
        // bounded wait below is a safety margin, not evidence of a network
        // wait having occurred).
        let (_qh_a, rows_rx_a) = handle.subscribe(literal_kind1(&a.public_key().to_hex()));
        assert!(
            wait_for_rows(&rows_rx_a, Duration::from_secs(5), |rows, coverage| {
                let ids: BTreeSet<String> = rows.iter().map(|r| r.id.to_hex()).collect();
                ids == post_ids && matches!(coverage, QueryCoverage::CompleteUpTo(_))
            }),
            "offline cold read must be AUTHORITATIVE: CompleteUpTo from the persisted watermark, \
             serving the 3 cached rows with zero network -- if this reads Unknown, ledger #7 is \
             not real"
        );

        // Control: b's shape has no coverage row anywhere and must read
        // Unknown, not CompleteUpTo -- "no row = not covered" must survive
        // the restart just as faithfully as the proven case does.
        let (_qh_b, rows_rx_b) = handle.subscribe(literal_kind1(&b.public_key().to_hex()));
        assert!(
            wait_for_rows(&rows_rx_b, Duration::from_secs(5), |rows, coverage| {
                rows.is_empty() && matches!(coverage, QueryCoverage::Unknown)
            }),
            "a never-synced shape must read Unknown, never CompleteUpTo -- a proven-empty \
             watermark must not be confused with a genuine cache-miss"
        );

        handle.shutdown();
        engine_thread.join();
    }
}

// ===========================================================================
// Ledger #5 (live corroboration) -- the SAME event, delivered by TWO real
// relays, surfaces as exactly one row (dedup, never a duplicate read).
// ===========================================================================

/// The authoritative ledger-#5 falsifier (insert-time provenance MERGE, not
/// a second stored row) already lives at the store's own public surface --
/// `nmp-store/tests/store_contract.rs::provenance_merges_across_relays`,
/// exercised against BOTH `MemoryStore` and this exact `RedbStore` backend
/// -- because `nmp_engine::core::RowDelta` deliberately carries no
/// provenance field (M3 plan §7: raw rows + coverage only; provenance is a
/// store-internal fact, not part of the two-noun read result), so there is
/// no live `Handle`-level surface to assert the provenance field against.
///
/// What IS a genuine live falsifier at this tier: the SAME event, delivered
/// end-to-end from TWO independent real relay connections, must surface
/// through a live subscription as exactly ONE row, never a duplicate --
/// proving the two-relay wiring in this crate doesn't leak a second,
/// un-deduplicated copy into the read result on its way from the wire to
/// the app.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_event_from_two_relays_surfaces_as_exactly_one_row() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port_1 = free_port();
    let port_2 = free_port();

    let a = Keys::generate();
    let a_relay_keys = mirror_keys(&a);

    let relay_1 = spawn_relay(port_1);
    relay_1.run().await.expect("run relay_1");
    let url_1 = RelayUrl::parse(&relay_1.url().await.to_string()).expect("parse relay_1 url");

    let relay_2 = spawn_relay(port_2);
    relay_2.run().await.expect("run relay_2");
    let url_2 = RelayUrl::parse(&relay_2.url().await.to_string()).expect("parse relay_2 url");

    // The IDENTICAL signed event, seeded into BOTH relays -- both will
    // independently EVENT/EOSE it back to the engine.
    let shared_post: RelayEvent = RelayEventBuilder::text_note("seen on two relays at once")
        .finalize(&a_relay_keys)
        .expect("sign shared post");
    relay_1
        .add_event(shared_post.clone())
        .await
        .expect("seed into relay_1");
    relay_2
        .add_event(shared_post.clone())
        .await
        .expect("seed into relay_2");

    let dir =
        FixtureDirectory::new().with_write(a.public_key().to_hex(), [url_1.clone(), url_2.clone()]);

    let (engine_thread, handle) = EngineThread::spawn(
        nmp_store::MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
    );
    handle.add_signer(LocalKeySigner::new(a.clone()));
    handle.set_active_account(Some(a.public_key()));

    let (_qh, rows_rx) = handle.subscribe(literal_kind1(&a.public_key().to_hex()));

    let shared_post_id = shared_post.id.to_hex();

    // Wait until both relays have proven complete (CompleteUpTo requires
    // EVERY covering relay to be proven -- ruling §6's unanimity rule) AND
    // the falsifier itself: exactly ONE row for this id, never two, despite
    // two independent relay deliveries of the identical event.
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows, coverage| {
            let matching = rows
                .iter()
                .filter(|r| r.id.to_hex() == shared_post_id)
                .count();
            matching == 1 && matches!(coverage, QueryCoverage::CompleteUpTo(_))
        }),
        "the shared post must surface as EXACTLY ONE row once both relays have proven complete \
         -- a duplicate row here would mean the two-relay delivery leaked a second, \
         un-deduplicated copy into the read result"
    );

    handle.shutdown();
    engine_thread.join();
    relay_1.shutdown();
    relay_2.shutdown();
}

// ===========================================================================
// Ledger #9 (live) -- enqueue != converged, per-relay ack split.
// ===========================================================================

/// A `WritePolicy` that rejects every event -- used to make one of the two
/// relays a guaranteed `Rejected`, so the receipt stream provably carries
/// BOTH outcomes for a single durable publish and "is it sent?" is only
/// answerable by reading the per-relay terminal states, never a single
/// bool.
#[derive(Debug)]
struct RejectAllWrites;

impl WritePolicy for RejectAllWrites {
    fn admit_event<'a>(
        &'a self,
        _event: &'a RelayEvent,
        _addr: &'a SocketAddr,
    ) -> BoxedFuture<'a, WritePolicyResult> {
        Box::pin(async move {
            WritePolicyResult::reject(MachineReadablePrefix::Blocked, "test: reject all writes")
        })
    }
}

/// Plan §5 test 11, live tier: publish a `Durable` intent to TWO real
/// relays, one of which accepts and one of which is configured to reject
/// every event. Asserts the FULL shape of ledger #9: the first status is
/// never a terminal (`Accepted` only), both relays are individually `Sent`
/// to, and the two relays resolve to DIFFERENT terminals (`Acked` vs
/// `Rejected`) -- there is no way to read "is it sent" except by observing
/// this per-relay stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_ack_per_relay_over_real_relays() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port_ok = free_port();
    let port_bad = free_port();

    let a = Keys::generate();

    let relay_ok = spawn_relay(port_ok);
    relay_ok.run().await.expect("run relay_ok");
    let url_ok = RelayUrl::parse(&relay_ok.url().await.to_string()).expect("parse relay_ok url");

    let relay_bad = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port_bad)
        .write_policy(RejectAllWrites)
        .build();
    relay_bad.run().await.expect("run relay_bad");
    let url_bad = RelayUrl::parse(&relay_bad.url().await.to_string()).expect("parse relay_bad url");

    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [url_ok.clone(), url_bad.clone()]);

    let (engine_thread, handle) = EngineThread::spawn(
        nmp_store::MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
    );
    handle.add_signer(LocalKeySigner::new(a.clone()));
    handle.set_active_account(Some(a.public_key()));

    let unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "durable publish, two relays, one rejects",
    );
    let receipt_rx = handle.publish(WriteIntent {
        payload: WritePayload::Unsigned(unsigned),
        durability: Durability::Durable,
        routing: WriteRouting::AuthorOutbox,
    });

    let mut seen: Vec<WriteStatus> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut acked_ok = false;
    let mut rejected_bad = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || (acked_ok && rejected_bad) {
            break;
        }
        match receipt_rx.recv_timeout(remaining) {
            Ok(status) => {
                match &status {
                    WriteStatus::Acked(r) if r == &url_ok => acked_ok = true,
                    WriteStatus::Rejected(r, _) if r == &url_bad => rejected_bad = true,
                    _ => {}
                }
                seen.push(status);
            }
            Err(_) => break,
        }
    }

    assert!(
        matches!(seen.first(), Some(WriteStatus::Accepted)),
        "the receipt stream's FIRST status must never be a terminal -- enqueue != converged \
         (got: {seen:?})"
    );
    assert!(
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Sent(r) if r == &url_ok)),
        "must observe Sent(relay_ok) (got: {seen:?})"
    );
    assert!(
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Sent(r) if r == &url_bad)),
        "must observe Sent(relay_bad) (got: {seen:?})"
    );
    assert!(acked_ok, "relay_ok must reach Acked (got stream: {seen:?})");
    assert!(
        rejected_bad,
        "relay_bad (RejectAllWrites) must reach Rejected, DISTINCT from relay_ok's Acked \
         (got stream: {seen:?})"
    );

    handle.shutdown();
    engine_thread.join();
    relay_ok.shutdown();
    relay_bad.shutdown();
}

// ===========================================================================
// Depth-2 grammar over a real relay: SetOp(Diff, [Derived(follows),
// Derived(mutes)]) -- M1 contract test 9's shape, actually resolving live.
// ===========================================================================

fn follows_minus_mutes_filter() -> Filter {
    let follows = Binding::Derived(Box::new(Derived {
        inner: Filter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        project: Selector::Tag("p".to_string()),
    }));
    let mutes = Binding::Derived(Box::new(Derived {
        inner: Filter {
            kinds: Some(BTreeSet::from([10_000u16])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        project: Selector::Tag("p".to_string()),
    }));
    Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::SetOp(Box::new(SetOp {
            op: SetAlgebra::Diff,
            operands: vec![follows, mutes],
        }))),
        ..Filter::default()
    }
}

/// `kinds:[1], authors := SetOp(Diff, [Derived(follows), Derived(mutes)])`
/// (M1 contract test 9's exact shape) driven end-to-end over a real relay:
/// `a` follows both `b` and `c`, then mutes `c`. `b`'s pre-seeded post must
/// surface; `c`'s pre-seeded post must NOT -- proving the two-level
/// Derived+SetOp cascade actually resolves through live kind:3/kind:10000
/// deliveries and the full runtime stack, not just scripted `EngineMsg`s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follows_minus_mutes_resolves_over_a_real_relay() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port = free_port();

    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();
    let b_relay_keys = mirror_keys(&b);
    let c_relay_keys = mirror_keys(&c);

    let relay = spawn_relay(port);
    relay.run().await.expect("run relay");
    let url = RelayUrl::parse(&relay.url().await.to_string()).expect("parse relay url");

    let b_post: RelayEvent = RelayEventBuilder::text_note("b's post -- should surface")
        .finalize(&b_relay_keys)
        .expect("sign b's post");
    let c_post: RelayEvent = RelayEventBuilder::text_note("c's post -- must NOT surface (muted)")
        .finalize(&c_relay_keys)
        .expect("sign c's post");
    relay
        .add_event(b_post.clone())
        .await
        .expect("seed b's post");
    relay
        .add_event(c_post.clone())
        .await
        .expect("seed c's post");

    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [url.clone()])
        .with_write(b.public_key().to_hex(), [url.clone()])
        .with_write(c.public_key().to_hex(), [url.clone()]);

    let (engine_thread, handle) = EngineThread::spawn(
        nmp_store::MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
    );
    handle.add_signer(LocalKeySigner::new(a.clone()));

    handle.set_active_account(Some(a.public_key()));
    let (_qh, rows_rx) = handle.subscribe(LiveQuery(follows_minus_mutes_filter()));

    // Publish a's contact list naming BOTH b and c.
    let contact_list = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::ContactList,
        vec![
            Tag::public_key(b.public_key()),
            Tag::public_key(c.public_key()),
        ],
        "",
    );
    let contact_receipt_rx = handle.publish(WriteIntent {
        payload: WritePayload::Unsigned(contact_list),
        durability: Durability::Durable,
        routing: WriteRouting::AuthorOutbox,
    });
    assert!(
        wait_for_status(&contact_receipt_rx, Duration::from_secs(10), |s| matches!(
            s,
            WriteStatus::Acked(r) if r == &url
        )),
        "a's contact list (naming b, c) must reach Acked"
    );

    // Publish a's mute list naming c.
    let mute_list = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::from(10_000u16),
        vec![Tag::public_key(c.public_key())],
        "",
    );
    let mute_receipt_rx = handle.publish(WriteIntent {
        payload: WritePayload::Unsigned(mute_list),
        durability: Durability::Durable,
        routing: WriteRouting::AuthorOutbox,
    });
    assert!(
        wait_for_status(&mute_receipt_rx, Duration::from_secs(10), |s| matches!(
            s,
            WriteStatus::Acked(r) if r == &url
        )),
        "a's mute list (naming c) must reach Acked"
    );

    // The settled state must show b's post and never c's -- SetOp(Diff)
    // resolved end-to-end over the real relay.
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(15), |rows, _coverage| {
            let ids: BTreeSet<String> = rows.iter().map(|r| r.id.to_hex()).collect();
            ids.contains(&b_post.id.to_hex()) && !ids.contains(&c_post.id.to_hex())
        }),
        "follows-minus-mutes must surface b's post and exclude c's (muted) once both the \
         contact list and the mute list have resolved"
    );

    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}
