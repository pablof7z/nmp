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

use nmp_grammar::RelaySessionKey;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use nmp_engine::core::{ObservationFact, RowDelta};
use nmp_engine::publish_queue::{RelayState, SigningState, WriteFact};
use nmp_grammar::LiveQuery;
use nmp_grammar::{
    Binding, ConcreteFilter, ContextualAtom, Demand, Derived, Filter, Freshness,
    IdentityField, IndexedTagName, ReadRouting, Selector,
};
use nmp_grammar::{Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_local_signer::LocalKeySigner;
use nmp_router_testkit::FixtureRoutingFacts;
use nmp_runtime::{
    EngineThread, FifoReceiver, FifoTryRecvError, ReceiptReattachment, RowsReceiver,
};
use nmp_store::{
    sentinel_signature, AcceptWrite, CoverageInterval, IntentSigState, RedbStore,
    RedbStoreResetError, RelayObserved,
};
use nmp_test_support::{
    relays::{AdvertisedLimits, RelayConfig, ScriptedRelay},
    ConnectionOwner,
};
use nmp_transport::PoolConfig;
use nostr::{
    EventId, Keys, Kind, RelayUrl, Tag, Timestamp,
    UnsignedEvent,
};

use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    Event as RelayEvent, EventBuilder as RelayEventBuilder, FinalizeEvent, Keys as RelayKeys,
    Tag as RelayTag, Timestamp as RelayTimestamp,
};

fn expect_attached(result: ReceiptReattachment) -> FifoReceiver<WriteFact> {
    match result {
        ReceiptReattachment::Attached { statuses, .. } => statuses,
        ReceiptReattachment::NotFound => panic!("known receipt was not found"),
        ReceiptReattachment::RetainedButUnreadable => {
            panic!("known receipt evidence was unreadable")
        }
    }
}


/// #765: `LocalKeySigner` now owns its scalar in one canonical zeroizing
/// allocation and no longer accepts a `nostr::Keys`. These fixtures still
/// build identities as `Keys`, so hand the raw scalar across exactly here.
fn local_signer(keys: &Keys) -> LocalKeySigner {
    LocalKeySigner::from_secret_bytes(keys.secret_key().as_secret_bytes())
        .expect("fixture keys are valid secp256k1 scalars")
}

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
    LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
            ..Filter::default()
        },
        ..Demand::default()
    })
}

fn pinned_tag_value(relay: &RelayUrl, value: &str) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([1u16])),
                tags: BTreeMap::from([(
                    IndexedTagName::new('p').expect("'p' is an indexed tag name"),
                    Binding::Literal(BTreeSet::from([value.to_string()])),
                )]),
                ..Filter::default()
            },
            ReadRouting::Explicit(vec![relay.clone()])
        )
        .expect("a pinned demand over one relay is constructible"),
    )
}

/// #565/#1344 product-path falsifier: `Freshness::MaxAge` is decided once when
/// the runtime processes `Cmd::Subscribe`, using that command's current wall
/// time directly. The stored coverage below is fresh relative to the core's
/// zero initial clock but stale relative to reality. Reusing reducer clock
/// state would suppress all wire work; the real TCP accept proves the stale
/// handle became `Live` instead, without turning open into a Tick.
#[test]
fn subscribe_uses_current_wall_clock_for_the_one_time_max_age_decision() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind capture relay");
    listener
        .set_nonblocking(true)
        .expect("make capture relay nonblocking");
    let relay = RelayUrl::parse(&format!(
        "ws://{}",
        listener.local_addr().expect("capture relay address")
    ))
    .expect("parse capture relay URL");
    let author_key = Keys::generate().public_key();
    let author = author_key.to_hex();
    let selection = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([author.clone()]))),
        ..Filter::default()
    };
    let atom = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([author.clone()])),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Auto,
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let now = Timestamp::now().as_secs();
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .record_coverage(&[(
            atom.clone(),
            RelaySessionKey::unauthenticated(relay.clone()),
            CoverageInterval::new(
                Timestamp::from(0u64),
                Timestamp::from(now.saturating_sub(60)),
            ),
        )])
        .expect("seed stale coverage");
    let directory = FixtureRoutingFacts::new().with_outbound_routes(author_key, [relay.clone()]);
    let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
        store,
        directory,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_secs(3600)),
            ..PoolConfig::default()
        },
    )
    .expect("spawn runtime");
    let mut demand = Demand {
        selection,
        ..Demand::default()
    };
    demand.freshness = Freshness::MaxAge { seconds: 1 };
    let (_query, _rows) = handle
        .subscribe(LiveQuery::single(demand))
        .expect("subscribe through runtime product path");

    let deadline = Instant::now() + Duration::from_secs(5);
    let connected = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                drop(stream);
                break true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break false;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("capture relay accept failed: {error}"),
        }
    };
    assert!(
        connected,
        "stale MaxAge coverage must become Live from Cmd::Subscribe's current wall time"
    );

    handle.shutdown();
    engine_thread.join();
}

/// #489: store-layer ownership must survive the lowest supported raw runtime
/// path. Moving `RedbStore` directly into `EngineThread` cannot bypass live
/// reset refusal, and joining the thread releases the exclusive lock.
#[test]
fn raw_engine_thread_owns_persistent_reset_guard_until_join() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("raw-engine-thread.redb");
    let store = RedbStore::open(&path).unwrap();
    let (engine_thread, handle) = EngineThread::spawn(store, 10, PoolConfig::default()).unwrap();

    assert!(matches!(
        RedbStore::reset(&path),
        Err(RedbStoreResetError::StoreStillOpen { path: refused })
            if refused == path.canonicalize().unwrap()
    ));
    assert!(
        path.exists(),
        "typed refusal must leave raw-engine bytes intact"
    );

    handle.shutdown();
    engine_thread.join();
    RedbStore::reset(&path).expect("joined raw engine must release store ownership");
}

/// Block (on the calling OS thread -- this crate's `Receiver`s are plain
/// `std::sync::mpsc`, never tokio) until the ACCUMULATED row set (built by
/// replaying every `Added`/`Removed` delta this channel has delivered so
/// far, exactly as a real app must -- `Handle::subscribe`'s wire is deltas,
/// not snapshots, per `nmp_engine::core::RowDelta`'s doc) matches `pred`, or
/// return `false` after `timeout`.
fn wait_for_rows(
    rx: &RowsReceiver,
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
            Ok((deltas, _coverage, _execution)) => {
                for delta in deltas {
                    match delta {
                        RowDelta::Added(row) => {
                            current.insert(
                                row.id(),
                                row.signed_event().expect("relay rows are signed"),
                            );
                        }
                        RowDelta::Updated(row) => {
                            current.insert(
                                row.id(),
                                row.signed_event().expect("relay rows are signed"),
                            );
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

fn drain_relay_request_evidence(rx: &RowsReceiver) -> Vec<(u64, u64, bool, ConcreteFilter)> {
    let mut requests = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok((_deltas, _coverage, execution)) => {
                requests.extend(execution.into_iter().filter_map(|evidence| {
                    let ObservationFact::RelayRequest {
                        transport_generation,
                        request_revision,
                        filter,
                        replay,
                        ..
                    } = evidence.fact
                    else {
                        return None;
                    };
                    Some((
                        transport_generation,
                        request_revision,
                        replay,
                        filter.as_ref().clone(),
                    ))
                }));
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("observation disconnected while collecting request evidence")
            }
        }
    }
    requests
}

/// Same shape as [`wait_for_rows`], for the receipt-status stream.
fn wait_for_status(
    rx: &FifoReceiver<WriteFact>,
    timeout: Duration,
    pred: impl Fn(&WriteFact) -> bool,
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
            Err(nmp_runtime::FifoRecvTimeoutError::Timeout)
            | Err(nmp_runtime::FifoRecvTimeoutError::Closed) => return false,
            Err(nmp_runtime::FifoRecvTimeoutError::Lagged) => {
                panic!("fixture receipt stream must not lag")
            }
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

    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [url.clone()])
        .with_outbound_routes(b.public_key(), [url.clone()]);

    let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
        RedbStore::temporary().expect("temporary Redb store"),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            reconnect_jitter_max: Some(Duration::ZERO),
            ..PoolConfig::default()
        },
    )
    .expect("test engine thread construction");

    handle
        .add_signer(local_signer(&a))
        .expect("local signer has a public key");
    handle.set_current_account(Some(a.public_key()));

    // $myFollows shape: kind:1 authored by whoever `a`'s kind:3 contact
    // list (#p-projected) currently names -- identical shape to M1's own
    // contract-test query and `core_headless.rs`'s analog.
    let my_follows = LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: Demand {
                    selection: Filter {
                        kinds: Some(BTreeSet::from([3u16])),
                        authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                        ..Filter::default()
                    },
                    ..Demand::default()
                },
                project: Selector::Tag("p".to_string()),
            }))),
            ..Filter::default()
        },
        ..Demand::default()
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
            payload: WritePayload::Event(body_of(&contact_list)),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
        })
        .expect("receipt id allocation")
        .statuses;

    assert!(
        wait_for_status(&receipt_rx, Duration::from_secs(10), |s| matches!(
            s,
            WriteFact::Relay { relay: r, state: RelayState::Published, .. } if r == &url
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

/// #1341: the production runtime, not a caller-side sleep, owns the short
/// first-arrival-anchored admission deadline. Two compatible queries issued
/// back-to-back must reach the relay as one combined request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_admission_deadline_groups_a_rapid_query_burst() {
    let relay_config = RelayConfig {
        advertised_limits: Some(AdvertisedLimits::default()),
        ..RelayConfig::default()
    };
    let relay = ScriptedRelay::start(&relay_config).await;
    let (engine_thread, handle) = EngineThread::spawn(
        RedbStore::temporary().expect("temporary Redb store"),
        10,
        PoolConfig::default(),
    )
    .expect("spawn runtime");

    let (alice, _alice_rows) = handle
        .subscribe(pinned_tag_value(&relay.url, "alice"))
        .expect("open first pending query");
    let (bob, _bob_rows) = handle
        .subscribe(pinned_tag_value(&relay.url, "bob"))
        .expect("join second pending query to the same cohort");

    // Wait for the deadline to FIRE before counting what it produced. A quiet
    // wire does not mean "the burst was admitted as one request" -- it equally
    // means "nothing has happened yet", and `wait_wire_quiet` cannot tell those
    // apart. Under CPU contention the 100ms quiet window elapses before the
    // admission deadline fires at all, and the count below then asserts against
    // an entirely empty record: observed failing with `reqs: []`, left 0.
    //
    // Waiting for the positive fact first turns "the runtime never sent it"
    // into its own named failure, and leaves the quiet window doing the only
    // job it can actually do -- proving no SECOND request follows the first.
    assert!(
        relay
            .wait_wire_req(Duration::from_secs(10), |req| req.names_tag('p'))
            .await
            .is_some(),
        "the runtime's admission deadline never sent the burst at all"
    );
    relay
        .wait_wire_quiet(Duration::from_millis(100), Duration::from_secs(5))
        .await;
    let record = relay.wire_record();
    let requests = record.reqs_naming_tag('p');
    assert_eq!(
        requests.len(),
        1,
        "the runtime deadline must admit the rapid compatible burst once: {record:#?}"
    );
    assert_eq!(
        requests[0].tag_values('p'),
        BTreeSet::from(["alice".to_string(), "bob".to_string()]),
        "the one admitted request must cover every pending value"
    );

    handle.unsubscribe(alice);
    handle.unsubscribe(bob);
    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}

/// #832: withdrawing the last demand for a relay makes its worker obsolete,
/// but the reducer still owns one terminal NIP-01 `CLOSE`. Retirement must
/// flush that frame on the exact connected generation before tearing down the
/// socket. Closing the worker first and dispatching `CLOSE` afterward makes
/// the frame unreachable and is falsified by the relay's real wire record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withdrawing_last_demand_flushes_close_before_worker_retirement() {
    let relay = ScriptedRelay::start(&RelayConfig {
        advertised_limits: Some(AdvertisedLimits::default()),
        ..RelayConfig::default()
    })
    .await;
    let (engine_thread, handle) = EngineThread::spawn(
        RedbStore::temporary().expect("temporary Redb store"),
        10,
        PoolConfig::default(),
    )
    .expect("spawn runtime");

    let (query, _rows) = handle
        .subscribe(pinned_tag_value(&relay.url, "android-close-proof"))
        .expect("open query");
    assert!(
        relay
            .wait_query_count_for_kind(1, 1, Duration::from_secs(5))
            .await,
        "the relay must witness the owned REQ before withdrawal"
    );
    let request_id = relay.wire_record().reqs_naming_tag('p')[0].sub_id.clone();

    handle.unsubscribe(query);
    relay
        .wait_wire_quiet(Duration::from_millis(100), Duration::from_secs(5))
        .await;
    let withdrawn = relay.wire_record();
    assert_eq!(
        withdrawn.closes,
        vec![request_id],
        "last-demand withdrawal must put the exact CLOSE on the wire before the obsolete worker disconnects: {withdrawn:#?}"
    );

    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}

/// #1075/#1341: one accepted `(session, sub-id, filter, transport generation)`
/// remains immutable until close or disconnect. A later admission cohort may
/// reuse exact existing coverage or open a sibling request, but cannot rewrite
/// an accepted request. A fresh connection generation replays every current
/// request exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_requests_are_immutable_and_reconnect_replays_each_once() {
    let relay_config = RelayConfig {
        advertised_limits: Some(AdvertisedLimits::default()),
        ..RelayConfig::default()
    };
    let mut relay = ScriptedRelay::start(&relay_config).await;
    let (engine_thread, handle) = EngineThread::spawn(
        RedbStore::temporary().expect("temporary Redb store"),
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            reconnect_jitter_max: Some(Duration::ZERO),
            ..PoolConfig::default()
        },
    )
    .expect("spawn runtime");

    let (first, first_rows) = handle
        .subscribe(pinned_tag_value(&relay.url, "alice"))
        .expect("open first query");
    assert!(
        relay
            .wait_query_count_for_kind(1, 1, Duration::from_secs(5))
            .await,
        "the relay must independently witness the first kind:1 REQ before quiescence"
    );
    relay
        .wait_wire_quiet(Duration::from_millis(100), Duration::from_secs(5))
        .await;
    let initial = relay.wire_record();
    assert_eq!(
        initial.reqs_naming_tag('p').len(),
        1,
        "the first generation must receive one REQ, not a queued send plus \
         transport/core replays: {initial:#?}"
    );
    assert!(
        initial.redundant_reqs().is_empty(),
        "the initial Connected edge must not replay a request already handed \
         to the same generation: {initial:#?}"
    );
    let initial_evidence = drain_relay_request_evidence(&first_rows);
    assert_eq!(
        initial_evidence.len(),
        1,
        "one wire REQ must mint exactly one public request incarnation: \
         {initial_evidence:#?}"
    );
    assert!(
        !initial_evidence[0].2,
        "the dialing-generation handoff is the original request, not a replay"
    );

    let (second, second_rows) = handle
        .subscribe(pinned_tag_value(&relay.url, "bob"))
        .expect("open a later admission cohort");
    assert!(
        relay
            .wait_query_count_for_kind(1, 2, Duration::from_secs(5))
            .await,
        "the relay must witness the later sibling REQ before quiescence"
    );
    relay
        .wait_wire_quiet(Duration::from_millis(100), Duration::from_secs(5))
        .await;
    let widened = relay.wire_record();
    let widened_reqs = widened.reqs_naming_tag('p');
    assert_eq!(
        widened_reqs.len(),
        2,
        "later uncovered demand must produce exactly one sibling REQ: {widened:#?}"
    );
    assert_ne!(
        widened_reqs[0].sub_id, widened_reqs[1].sub_id,
        "an accepted request is immutable; a later cohort gets a sibling id"
    );
    assert!(
        !widened_reqs[1].replaces,
        "ordinary later demand must not replace an already-sent request"
    );
    assert!(
        widened.redundant_reqs().is_empty(),
        "no byte-identical request may be resent on the unchanged generation: \
         {widened:#?}"
    );
    let second_evidence = drain_relay_request_evidence(&second_rows);
    assert_eq!(
        second_evidence.len(),
        1,
        "one later cohort must mint exactly one sibling request incarnation: \
         {second_evidence:#?}"
    );
    assert!(!second_evidence[0].2);
    assert_eq!(
        second_evidence[0].0, initial_evidence[0].0,
        "the sibling opens on the same transport generation"
    );
    assert_ne!(
        second_evidence[0].1, initial_evidence[0].1,
        "independent requests own independent revisions"
    );
    assert_ne!(
        second_evidence[0].3, initial_evidence[0].3,
        "the sibling request carries only its own cohort's filter"
    );

    let relay_port = relay.port();
    relay.disconnect().await;
    let replacement = ScriptedRelay::start_on_port(relay_port, &relay_config).await;
    assert!(
        replacement
            .wait_query_count_for_kind(1, 2, Duration::from_secs(10))
            .await,
        "the fresh relay generation must independently witness both replayed kind:1 REQs"
    );
    replacement
        .wait_wire_quiet(Duration::from_millis(100), Duration::from_secs(5))
        .await;
    let replay = replacement.wire_record();
    assert_eq!(
        replay.reqs_naming_tag('p').len(),
        2,
        "the fresh generation must replay both immutable requests once: {replay:#?}"
    );
    assert!(
        replay.redundant_reqs().is_empty(),
        "reconnect replay itself must have one owner: {replay:#?}"
    );
    let mut replay_evidence = drain_relay_request_evidence(&first_rows);
    replay_evidence.extend(drain_relay_request_evidence(&second_rows));
    assert_eq!(
        replay_evidence.len(),
        2,
        "two current wire REQs must mint exactly two replay incarnations: \
         {replay_evidence:#?}"
    );
    assert!(replay_evidence.iter().all(|evidence| evidence.2));
    let original_filters =
        BTreeSet::from([initial_evidence[0].3.clone(), second_evidence[0].3.clone()]);
    assert_eq!(
        replay_evidence
            .iter()
            .map(|evidence| evidence.3.clone())
            .collect::<BTreeSet<_>>(),
        original_filters,
        "reconnect replays each unchanged current filter exactly once"
    );
    for evidence in &replay_evidence {
        assert_ne!(
            evidence.0, initial_evidence[0].0,
            "reconnect replay must name the fresh transport generation"
        );
        assert_ne!(
            evidence.1, initial_evidence[0].1,
            "reconnect replay must own a fresh request revision"
        );
        assert_ne!(
            evidence.1, second_evidence[0].1,
            "reconnect replay must own a fresh request revision"
        );
    }

    handle.unsubscribe(first);
    handle.unsubscribe(second);
    handle.shutdown();
    engine_thread.join();
    replacement.shutdown();
}

// ---- #39: the deadline-armed driver (design §3.3) ------------------------
//
// `EngineThread::spawn`'s `Handle` exposes no manual-tick verb at all (see
// `handle_surface_is_closed_and_receipt_reattachment_is_explicit` below) -- so any
// `RowDelta::Removed` this crate's own tests observe with no further
// command sent can only have come from `runtime::engine_loop`'s own
// `recv_timeout` arming itself off `core::EngineCore::next_deadline()` and
// firing `EngineMsg::Tick` on its own, exactly the property #39 asks for.

/// #39 test obligation `no_deadlines_blocks_indefinitely`: an engine thread
/// with zero subscriptions has no wire demand and hence no expiring content
/// -- `core::EngineCore::next_deadline()` is
/// `None` from the moment it is built, so `engine_loop` must be blocking on
/// a plain `cmd_rx.recv()`, never a hot `recv_timeout(0)` loop (D8). No
/// `Effect` crosses the wire in this scenario (a spurious tick with nothing
/// due produces an empty effect vec -- see `EngineCore::tick`), so nothing on
/// the wire can distinguish the two.
///
/// #1796: what does distinguish them is `EngineThread::wait_arms` -- how many
/// times THIS engine thread's loop has come back around to arm a wait. Parked
/// in `recv()` the loop never reaches the top again, so the count stands
/// perfectly still however long the window is; a `recv_timeout(0)` hot loop
/// moves it once per spin, millions of times a second. That is the property
/// itself, counted, not a proxy for it.
///
/// This replaces a `getrusage(RUSAGE_SELF)` CPU sample across the same idle
/// window. `RUSAGE_SELF` is the whole PROCESS: under `cargo test`'s default
/// parallelism every sibling test in this binary spent its CPU inside that
/// window too, so the sample could not tell "the engine thread is spinning"
/// (the only thing it was for) from "something else in this process was
/// busy". `wait_arms` belongs to one engine thread and nothing else can
/// touch it.
#[test]
fn no_deadlines_blocks_indefinitely() {
    let (engine_thread, handle) = EngineThread::spawn(
        RedbStore::temporary().expect("temporary Redb store"),
        10,
        PoolConfig::default(),
    )
    .expect("test engine thread construction");

    // Let the engine thread settle onto its idle `recv()` before sampling.
    std::thread::sleep(Duration::from_millis(100));
    let before = engine_thread.wait_arms();
    std::thread::sleep(Duration::from_millis(500));
    let after = engine_thread.wait_arms();

    // Exactly zero, not "few": with no subscription there is no core
    // deadline, no NIP-11 fallback, no wire-admission flush and no deferred
    // diagnostics, so the wait is `None` and the loop is inside a plain
    // `recv()` that nothing sends to. Any re-arm at all would mean the loop
    // woke for something this scenario says cannot exist.
    assert_eq!(
        after - before,
        0,
        "an engine thread with no deadlines must block on a plain recv() for \
         the whole idle window -- a busy-spinning recv_timeout(0) loop would \
         re-arm its wait millions of times inside the same 500ms instead: \
         re-armed {} time(s)",
        after - before
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

    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [url.clone()]);
    let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
        RedbStore::temporary().expect("temporary Redb store"),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
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

    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [url.clone()]);
    let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
        RedbStore::temporary().expect("temporary Redb store"),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
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
            nmp_resolver_testkit::expiring_kind1(&a, "expires almost immediately", 100, 101);
        let control = nmp_resolver_testkit::kind1(&a, "a plain, non-expiring note", 100);
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
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay0]);
    let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
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

#[test]
fn runtime_exposes_stable_receipt_id_and_supports_multiple_reattach_observers() {
    let keys = Keys::generate();
    let (thread, handle) = EngineThread::spawn(
        RedbStore::temporary().expect("temporary Redb store"),
        10,
        PoolConfig::default(),
    )
    .expect("test engine thread construction");
    handle.set_current_account(Some(keys.public_key()));
    let tracked = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(nmp_grammar::EventBuilder {
                kind: Kind::TextNote,
                tags: (vec![]).into_iter().collect(),
                content: ("tracked").into(),
                created_at: Some(Timestamp::now()),
            }),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
        })
        .expect("receipt id allocation");
    assert!(
        tracked.id.0 < (1u64 << 63),
        "accepted ids use store namespace"
    );
    assert_eq!(
        tracked.statuses.recv().unwrap(),
        WriteFact::Signing(SigningState::AwaitingSigner {
            pubkey: keys.public_key()
        })
    );
    assert_eq!(
        tracked.statuses.try_recv(),
        Err(FifoTryRecvError::Empty),
        "one reducer publish must deliver each emitted fact exactly once"
    );

    let first = expect_attached(handle.reattach_receipt(tracked.id));
    let second = expect_attached(handle.reattach_receipt(tracked.id));
    assert_eq!(
        first.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteFact::Signing(SigningState::AwaitingSigner {
            pubkey: keys.public_key()
        })
    );
    assert_eq!(
        second.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteFact::Signing(SigningState::AwaitingSigner {
            pubkey: keys.public_key()
        })
    );
    handle
        .add_signer(local_signer(&keys))
        .expect("local signer has a public key");
    assert!(wait_for_status(
        &first,
        Duration::from_secs(2),
        |status| matches!(
            status,
            WriteFact::Signing(SigningState::Signed { event_id: _ })
        )
    ));
    assert!(wait_for_status(
        &second,
        Duration::from_secs(2),
        |status| matches!(
            status,
            WriteFact::Signing(SigningState::Signed { event_id: _ })
        )
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
                payload: nmp_store::AcceptWritePayload::Event {
                    frozen: Box::new(nostr::Event::new(
                        id,
                        unsigned.pubkey,
                        unsigned.created_at,
                        unsigned.kind,
                        unsigned.tags,
                        unsigned.content,
                        sentinel_signature(),
                    )),
                    routing: "auto".into(),
                    sig_state: IntentSigState::AwaitingSigner,
                },
                expected_pubkey: keys.public_key(),
                signing_identity_ref: keys.public_key().to_hex(),
                accepted_at: Timestamp::now(),
            })
            .unwrap();
        nmp_engine::core::ReceiptId(outcome.journaled_receipt_id().unwrap())
    };
    let (thread, handle) =
        EngineThread::spawn(RedbStore::open(&path).unwrap(), 10, PoolConfig::default())
            .expect("test engine thread construction");
    // This is literally the first command sent to the new engine thread.
    let statuses = expect_attached(handle.reattach_receipt(receipt));
    assert_eq!(
        statuses.recv_timeout(Duration::from_secs(1)).unwrap(),
        WriteFact::Signing(SigningState::AwaitingSigner {
            pubkey: keys.public_key()
        })
    );
    handle
        .add_signer(local_signer(&keys))
        .expect("local signer has a public key");
    assert!(wait_for_status(
        &statuses,
        Duration::from_secs(2),
        |status| matches!(status, WriteFact::Signing(SigningState::Signed { event_id }) if *event_id == id)
    ));
    handle.shutdown();
    thread.join();
}

/// The same body these fixtures already build, said the way an app says it:
/// a builder states the kind, the tags, the content and (here, so the
/// assertions can name exact ids) the timestamp. The author is not part of
/// it -- the write's identity decides that at acceptance.
fn body_of(unsigned: &nostr::UnsignedEvent) -> nmp_grammar::EventBuilder {
    nmp_grammar::EventBuilder {
        kind: unsigned.kind,
        tags: unsigned.tags.iter().cloned().collect(),
        content: unsigned.content.clone(),
        created_at: Some(unsigned.created_at),
    }
}
