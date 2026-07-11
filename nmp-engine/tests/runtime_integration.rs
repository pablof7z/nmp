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

use std::collections::BTreeSet;
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
use nmp_store::MemoryStore;
use nmp_transport::PoolConfig;
use nostr::{Keys, Kind, RelayUrl, Tag, Timestamp, UnsignedEvent};

use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    Event as RelayEvent, EventBuilder as RelayEventBuilder, FinalizeEvent, Keys as RelayKeys,
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

/// Block (on the calling OS thread -- this crate's `Receiver`s are plain
/// `std::sync::mpsc`, never tokio) until a rows batch matching `pred`
/// arrives, or return `false` after `timeout`. Drains and discards
/// non-matching batches (e.g. the initial empty/`Unknown` batch every fresh
/// `subscribe` delivers).
fn wait_for_rows(
    rx: &Receiver<RowsMsg>,
    timeout: Duration,
    pred: impl Fn(&[RowDelta]) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok((rows, _coverage)) if pred(&rows) => return true,
            Ok(_) => {} // non-matching batch -- keep waiting for the next one
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
        LocalKeySigner::new(a.clone()),
    );

    handle.set_active_pubkey(Some(a.public_key()));

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
            .any(|r| r.event.id.to_hex() == b_post.id.to_hex())),
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
            .any(|r| r.event.id.to_hex() == b_post.id.to_hex())),
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
            .any(|r| r.event.id.to_hex() == second_post.id.to_hex())),
        "reconnect must replay the current subs with no gap -- b's post-reconnect note must surface without the app resubscribing"
    );

    handle.shutdown();
    engine_thread.join();
    relay_b.shutdown();
}

/// Structural grep-guard (M3 plan §5 test 14): `Handle`'s public surface is
/// exactly the four verbs (`subscribe`/`unsubscribe`/`set_active_pubkey`/
/// `publish`) plus `shutdown` -- no `relays:` parameter, no open-REQ method
/// anywhere on it (ledger #2/#3 preserved at the top edge). Asserted by
/// reading this crate's own source rather than by reflection (Rust has
/// none) -- the same "grep-guard" idiom the plan itself names.
#[test]
fn handle_surface_is_exactly_four_verbs_plus_shutdown() {
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
        "publish",
        "set_active_pubkey",
        "shutdown",
        "subscribe",
        "unsubscribe",
    ];
    expected.sort_unstable();
    assert_eq!(
        methods, expected,
        "Handle must expose exactly the four verbs + shutdown -- no relays:/open-REQ method"
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
