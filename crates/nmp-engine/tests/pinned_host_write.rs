//! #115 acceptance falsifiers, live tier: `WriteRouting::PinnedHost` end to
//! end against real in-process relays, driven through the actual production
//! composer (`nmp_nip29::compose_group_send`/`group_content_demand`), not a
//! hand-rolled `WriteIntent`. Mirrors `integration_capstone.rs`'s house
//! style exactly (same `LocalRelay`/`FixtureDirectory`/`EngineThread`
//! harness, same version-shadowing precaution: never
//! `use nostr_relay_builder::prelude::*`, every cross-version value is
//! bridged by explicit hex/id round-trip).

use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use nmp_engine::core::RelayAdmissionPolicy;
use nmp_engine::core::RowDelta;
use nmp_engine::outbox::WriteStatus;
use nmp_engine::runtime::{EngineThread, RowsMsg};
use nmp_nip29::GroupTimelineEvidence;
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_signer::LocalKeySigner;
use nmp_transport::PoolConfig;
use nostr::{EventId, Keys, RelayUrl, Timestamp};

use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    BoxedFuture, Event as RelayEvent, MachineReadablePrefix, WritePolicy, WritePolicyResult,
};

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

fn spawn_relay(port: u16) -> LocalRelay {
    LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build()
}

fn wait_for_status(
    rx: &Receiver<WriteStatus>,
    timeout: Duration,
    pred: impl Fn(&WriteStatus) -> bool,
) -> Option<WriteStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(status) if pred(&status) => return Some(status),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn wait_for_rows(
    rx: &Receiver<RowsMsg>,
    timeout: Duration,
    pred: impl Fn(&[nostr::Event]) -> bool,
) -> Option<Vec<nostr::Event>> {
    let deadline = Instant::now() + timeout;
    let mut current: std::collections::BTreeMap<EventId, nostr::Event> =
        std::collections::BTreeMap::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok((deltas, _evidence)) => {
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
                    return Some(snapshot);
                }
            }
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Falsifiers 2 (host-only wire), 6 (frozen template), and 8 (read-back
/// loop) in one pass: publish a `compose_group_send`-composed kind:9 group
/// send, prove it reaches ONLY the pinned host (never the second relay,
/// which is otherwise a fully compliant relay the directory doesn't even
/// know about -- `FixtureDirectory::new()` registers no write relays at
/// all for this author, proving `PinnedHost` never consults it), then
/// subscribe against that same host with the production
/// `group_content_demand` read and confirm the identical tags/kind/content
/// come back unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_host_send_reaches_only_the_host_and_round_trips_unchanged() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port_host = free_port();
    let port_other = free_port();

    let author = Keys::generate();

    let relay_host = spawn_relay(port_host);
    relay_host.run().await.expect("run relay_host");
    let url_host =
        RelayUrl::parse(&relay_host.url().await.to_string()).expect("parse relay_host url");

    let relay_other = spawn_relay(port_other);
    relay_other.run().await.expect("run relay_other");
    let url_other =
        RelayUrl::parse(&relay_other.url().await.to_string()).expect("parse relay_other url");

    // Deliberately EMPTY: no write relay registered for `author` at all.
    // `PinnedHost` routing must never consult this directory (`resolve_
    // routes`'s doc: infallible, directory-blind, exactly like
    // `PrivateNarrow`) -- if it ever did, this publish would have nowhere
    // to route and the test would hang/fail closed instead of reaching
    // `relay_host`.
    let dir = FixtureDirectory::new();

    let (engine_thread, handle) = EngineThread::spawn(
        nmp_store::MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
        RelayAdmissionPolicy::default(),
    );
    handle.add_signer(LocalKeySigner::new(author.clone()));
    handle.set_active_account(Some(author.public_key()));

    let intent = nmp_nip29::compose_group_send(
        url_host.clone(),
        "group-a",
        author.public_key(),
        Timestamp::now(),
        9,
        "hello group".to_string(),
        vec![],
        &GroupTimelineEvidence::none(),
    )
    .expect("well-formed group send composes");
    let nmp_grammar::WritePayload::Unsigned(composed_unsigned) = &intent.payload else {
        panic!("compose_group_send always yields Unsigned")
    };
    let composed_tags = composed_unsigned.tags.clone();

    let receipt_rx = handle.publish(intent).expect("receipt id allocation");

    assert!(
        wait_for_status(
            &receipt_rx,
            Duration::from_secs(10),
            |s| matches!(s, WriteStatus::Acked(r) if r == &url_host)
        )
        .is_some(),
        "the pinned host must reach Acked"
    );

    // Drain a grace window: no status naming `url_other` may EVER appear --
    // this relay was never told about the write, `PinnedHost` must never
    // widen to it.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match receipt_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(status) => assert!(
                !matches!(&status, WriteStatus::Sent(r) | WriteStatus::Acked(r) | WriteStatus::Rejected(r, _) if r == &url_other),
                "no status may ever name the non-pinned relay (got: {status:?})"
            ),
            Err(_) => {}
        }
    }

    // Falsifier 8 (read-back loop): the production `group_content_demand`
    // read, pinned to the SAME host, must see the just-acked event.
    let demand = nmp_nip29::group_content_demand(url_host.clone(), "group-a");
    let (_qh, rows_rx) = handle.subscribe(LiveQuery(demand));

    let rows = wait_for_rows(&rows_rx, Duration::from_secs(10), |rows| !rows.is_empty())
        .expect("the group send must reappear via a live group_content_demand read");
    assert_eq!(rows.len(), 1, "exactly the one sent event, nothing else");

    // Falsifier 6 (frozen template): the delivered row is byte-for-byte
    // what was composed, tags included -- the engine never injected,
    // rewrote, or dropped anything between compose and acceptance.
    assert_eq!(rows[0].tags, composed_tags);
    assert_eq!(rows[0].content, "hello group");
    assert_eq!(rows[0].kind.as_u16(), 9);
    assert_eq!(rows[0].pubkey, author.public_key());

    handle.shutdown();
    engine_thread.join();
    relay_host.shutdown();
    relay_other.shutdown();
}

/// A `WritePolicy` that rejects every event with a NIP-29-flavored reason,
/// simulating a group host that has genuinely vetoed the write (e.g. "not
/// a member") -- mirrors `integration_capstone.rs`'s `RejectAllWrites`.
#[derive(Debug)]
struct RejectNotAMember;

impl WritePolicy for RejectNotAMember {
    fn admit_event<'a>(
        &'a self,
        _event: &'a RelayEvent,
        _addr: &'a SocketAddr,
    ) -> BoxedFuture<'a, WritePolicyResult> {
        Box::pin(async move {
            WritePolicyResult::reject(MachineReadablePrefix::Blocked, "not a member")
        })
    }
}

/// Falsifiers 4 and 7: a group host's admission veto surfaces as a typed
/// `WriteStatus::Rejected(host, reason)` on the receipt stream -- never
/// silence, and never swallowed by the pinned-host routing path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_host_rejection_surfaces_as_a_typed_status_never_silence() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port_host = free_port();
    let author = Keys::generate();

    let relay_host = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port_host)
        .write_policy(RejectNotAMember)
        .build();
    relay_host.run().await.expect("run relay_host");
    let url_host =
        RelayUrl::parse(&relay_host.url().await.to_string()).expect("parse relay_host url");

    let (engine_thread, handle) = EngineThread::spawn(
        nmp_store::MemoryStore::new(),
        FixtureDirectory::new(),
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
        RelayAdmissionPolicy::default(),
    );
    handle.add_signer(LocalKeySigner::new(author.clone()));
    handle.set_active_account(Some(author.public_key()));

    let intent = nmp_nip29::compose_group_send(
        url_host.clone(),
        "group-a",
        author.public_key(),
        Timestamp::now(),
        9,
        "hello group".to_string(),
        vec![],
        &GroupTimelineEvidence::none(),
    )
    .expect("well-formed group send composes");

    let receipt_rx = handle.publish(intent).expect("receipt id allocation");

    match wait_for_status(
        &receipt_rx,
        Duration::from_secs(10),
        |s| matches!(s, WriteStatus::Rejected(r, _) if r == &url_host),
    ) {
        Some(WriteStatus::Rejected(relay, reason)) => {
            assert_eq!(relay, url_host);
            assert!(
                reason.contains("not a member"),
                "the host's own veto reason must surface verbatim (got: {reason:?})"
            );
        }
        other => panic!("expected WriteStatus::Rejected(url_host, _), got {other:?}"),
    }

    handle.shutdown();
    engine_thread.join();
    relay_host.shutdown();
}
