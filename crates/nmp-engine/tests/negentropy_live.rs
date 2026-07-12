//! Live negentropy round-trip (M3 plan §5 test 10, negentropy half):
//! `EngineThread`/`Handle` driven against a real in-process relay that
//! genuinely speaks NIP-77 -- `nostr-relay-builder`'s `LocalRelay` (see
//! `nostr-relay-builder-0.45.0-alpha.3/src/local/inner.rs`'s `NegOpen`/
//! `NegMsg`/`NegClose` handling: it is a full server-side negentropy
//! implementation, not a stub). This is therefore a REAL green
//! reconciliation, not a `#[ignore]`d placeholder -- see the module doc on
//! `subscribe_widens_via_negentropy_and_surfaces_the_backfilled_post` for
//! why that honesty claim is load-bearing here.
//!
//! Deliberately NOT a glob import of `nostr_relay_builder::prelude::*` and
//! keys are bridged by hex round-trip, mirroring `runtime_integration.rs`'s
//! identical `nostr`-version-shadowing precaution (this workspace pins
//! `nostr = "0.44.4"`; `nostr-relay-builder` pulls its own `0.45.0-alpha.4`).

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use nmp_engine::core::RowDelta;
use nmp_engine::runtime::{EngineThread, RowsMsg};
use nmp_grammar::{Binding, Filter};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_signer::LocalKeySigner;
use nmp_store::MemoryStore;
use nmp_transport::PoolConfig;
use nostr::{EventId, Keys, RelayUrl};

use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    Event as RelayEvent, EventBuilder as RelayEventBuilder, FinalizeEvent, Keys as RelayKeys,
};

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

fn mirror_keys(k: &Keys) -> RelayKeys {
    RelayKeys::parse(&k.secret_key().to_secret_hex())
        .expect("mirror keypair across nostr crate versions")
}

/// Accumulates the channel's `Added`/`Removed` deltas into the row set they
/// currently describe (exactly as a real app must -- the wire is deltas, not
/// snapshots, per `nmp_engine::core::RowDelta`'s doc) and blocks until that
/// accumulated set + the latest acquisition evidence satisfy `pred`, or
/// `timeout` lapses. Evidence and rows can change independently (a
/// watermark advancing carries an empty row delta) -- `pred` is checked
/// against the freshest evidence seen alongside the freshest accumulated
/// row set on every batch.
fn wait_for_rows(
    rx: &Receiver<RowsMsg>,
    timeout: Duration,
    pred: impl Fn(&[nostr::Event], &nmp_engine::core::AcquisitionEvidence) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut current: BTreeMap<EventId, nostr::Event> = BTreeMap::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok((deltas, evidence)) => {
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
                if pred(&snapshot, &evidence) {
                    return true;
                }
            }
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn literal_kind1(author_hex: &str) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(std::collections::BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(std::collections::BTreeSet::from([
            author_hex.to_string(),
        ]))),
        ..Filter::default()
    })
}

/// Test 10 (plan §5, negentropy half): a relay PROVEN to speak NIP-77
/// (`LocalRelay` -- confirmed real server-side negentropy support, not
/// assumed) reconciles a widened, broad demand instead of a plain REQ, and
/// the id it proves we are missing surfaces through the ordinary
/// REQ/EOSE/ingest backfill pipeline with coverage correctly recorded.
///
/// Bootstrap ordering note (see `core::mod`'s `recompile` doc): probing can
/// only start once SOME demand causes a connection, so the FIRST subscribe
/// to a brand-new relay is unavoidably a plain REQ. This test therefore
/// bootstraps with author `a`'s own (empty) feed first, waits for the
/// probe round-trip to resolve `Supported` (near-instant over loopback),
/// THEN widens onto `b` -- at which point the relay is already known to
/// support NIP-77 and the widened atom routes negentropy-first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_widens_via_negentropy_and_surfaces_the_backfilled_post() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let port = free_port();

    let a = Keys::generate();
    let b = Keys::generate();
    let b_relay_keys = mirror_keys(&b);

    let relay = LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build();
    relay.run().await.expect("run relay");
    let url = RelayUrl::parse(&relay.url().await.to_string()).expect("parse relay url");

    // b's post is seeded directly into the relay's database, BEFORE the
    // engine ever asks for it -- exactly the shape negentropy is for: the
    // engine's own local store has never seen it, the relay already holds
    // it, and reconciliation must discover the gap and backfill it (rather
    // than the engine ever having issued a REQ that happened to return it).
    let b_post: RelayEvent = RelayEventBuilder::text_note("hello from b, via negentropy")
        .finalize(&b_relay_keys)
        .expect("sign b's post");
    relay
        .add_event(b_post.clone())
        .await
        .expect("seed b's post into the relay");

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

    // Bootstrap: a's own (empty) kind:1 feed -- this is what actually opens
    // the connection to `url` and kicks off the capability probe.
    let (_a_handle, _a_rows_rx) = handle.subscribe(literal_kind1(&a.public_key().to_hex()));

    // Bounded wait for the probe round-trip to resolve `Supported` --
    // `LocalRelay` answers NEG-OPEN synchronously over loopback, so this is
    // comfortably (not tightly) bounded, matching `runtime_integration.rs`'s
    // own fixed-sleep precedent for out-of-band state settling.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Widen onto b's kind:1 feed -- SAME skeleton as `a`'s, so this
    // coalesces onto the SAME relay sub-id, which by now is Supported: the
    // reducer routes it negentropy-first instead of a plain REQ.
    let (_b_handle, b_rows_rx) = handle.subscribe(literal_kind1(&b.public_key().to_hex()));

    assert!(
        wait_for_rows(&b_rows_rx, Duration::from_secs(15), |rows, evidence| {
            rows.iter().any(|r| r.id.to_hex() == b_post.id.to_hex())
                && evidence
                    .sources
                    .iter()
                    .any(|s| s.reconciled_through.is_some())
        }),
        "negentropy must discover b's pre-seeded, never-REQ'd post, backfill it via the \
         ordinary REQ/EOSE/ingest pipeline, and the query's own relay source must carry a \
         proven reconciled_through once (and only once) the backfilled event actually landed"
    );

    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}
