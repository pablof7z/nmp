//! M3 Step F — the integration capstone: the full falsifier suite driven
//! against a live in-process relay, headlined by [`watermark_cold_start_offline`]
//! (plan `docs/plans/M3-store-transport-outbox-plan.md` §5 test 9, THE M3
//! pass criterion — ledger #7, "cache-miss treated as empty"). The other
//! three tests here round out the LIVE tier of the remaining ledger
//! falsifiers that weren't already exercised end-to-end: #5 (provenance/
//! dedup across two relays), #9 (enqueue != converged, per-relay ack split),
//! and the depth-2 grammar (`SetOp(Diff, …)`, M1 contract test 9's shape)
//! actually resolving over a real relay rather than scripted `EngineMsg`s.
//! Issue #8 extends those two write-bearing capstones through a strict local
//! WebSocket relay that requires a fresh, exactly validated NIP-42 handshake
//! before either protected reads or writes may cross the wire.
//!
//! Same version-shadowing precaution as `runtime_integration.rs`/
//! `negentropy_live.rs`: never `use nostr_relay_builder::prelude::*` (it
//! re-exports a DIFFERENT `nostr` than this workspace's pinned `0.44.4`);
//! every cross-version value is bridged by explicit hex/id round-trip.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nmp_engine::core::RelayAdmissionPolicy;
use nmp_engine::core::{AcquisitionEvidence, RowDelta, SourceStatus};
use nmp_engine::outbox::WriteStatus;
use nmp_engine::runtime::{EngineThread, FifoReceiver, RowsReceiver};
use nmp_engine::{AuthPolicy, AuthPolicyOp, AuthPolicyRequest};
use nmp_grammar::{
    AccessContext, Binding, Demand, Derived, Filter, IdentityField, Selector, SetAlgebra, SetOp,
    SourceAuthority,
};
use nmp_grammar::{Durability, WriteIntent, WritePayload, WriteRouting};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_signer::LocalKeySigner;
use nmp_store::RedbStore;
use nmp_transport::PoolConfig;
use nostr::filter::MatchEventOptions;
use nostr::{
    ClientMessage, EventId, Filter as NostrFilter, JsonUtil, Keys, Kind, RelayMessage, RelayUrl,
    SubscriptionId, Tag, Timestamp, UnsignedEvent,
};
use tungstenite::{Error as WebSocketError, Message, WebSocket};

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

fn literal_kind1(author_hex: &str) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        ..Filter::default()
    })
}

/// Accumulates the channel's `Added`/`Removed` deltas into the row set they
/// currently describe (exactly as a real app must -- `Handle::subscribe`'s
/// wire is deltas, not snapshots, per `nmp_engine::core::RowDelta`'s doc) and
/// blocks until that accumulated set + the latest acquisition evidence
/// satisfy `pred`, or `timeout` lapses. Replaying `Removed` deltas (not just
/// tracking "ever added") is load-bearing for `follows_minus_mutes_resolves_
/// over_a_real_relay` below, whose predicate needs the settled CURRENT
/// membership, not a monotonic history.
fn wait_for_rows(
    rx: &RowsReceiver,
    timeout: Duration,
    pred: impl Fn(&[nostr::Event], &AcquisitionEvidence) -> bool,
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

fn wait_for_status(
    rx: &FifoReceiver<WriteStatus>,
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

/// Find `relay`'s [`nmp_engine::core::SourceEvidence`] entry, if any, inside
/// `evidence` -- test-fixture convenience mirroring `core_headless.rs`'s
/// identically-named helper.
fn source_for<'a>(
    evidence: &'a AcquisitionEvidence,
    relay: &RelayUrl,
) -> Option<&'a nmp_engine::core::SourceEvidence> {
    evidence.sources.iter().find(|s| &s.relay == relay)
}

fn spawn_relay(port: u16) -> LocalRelay {
    LocalRelay::builder()
        .addr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        .port(port)
        .build()
}

// ===========================================================================
// NIP-42 real-relay capstone fixture (#8)
// ===========================================================================

/// Ordinary EVENT behavior after this relay has authenticated the connection.
#[derive(Debug, Clone, Copy)]
enum AuthRelayWriteOutcome {
    Ack,
    Reject,
}

#[derive(Debug, Clone, Default)]
struct AuthRelayObservation {
    challenges: Vec<String>,
    auth_events: Vec<nostr::Event>,
    invalid_auth: Vec<String>,
    ordinary_events: Vec<nostr::Event>,
    accepted_ordinary_events: Vec<EventId>,
    pre_auth_reqs: usize,
    pre_auth_events: usize,
    connections: usize,
}

#[derive(Debug, Default)]
struct AuthRelayState {
    events: BTreeMap<EventId, nostr::Event>,
    observation: AuthRelayObservation,
}

/// A deliberately strict, real WebSocket relay used only by #8's runtime
/// capstone. Unlike `nostr-relay-builder`, it sends AUTH immediately after
/// the WebSocket handshake, before the client has a chance to send a REQ or
/// EVENT. Every connection receives a unique challenge. Until a matching
/// kind:22242 event is verified and acknowledged, protected REQ/EVENT frames
/// are refused and counted as test failures.
///
/// The fixture also keeps AUTH accounting structurally separate from ordinary
/// EVENT accounting. That is the live counterpart to the reducer proof that
/// an AUTH OK cannot advance a durable write receipt.
struct AuthRequiredRelay {
    url: RelayUrl,
    state: Arc<Mutex<AuthRelayState>>,
    shutdown: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl AuthRequiredRelay {
    fn spawn(
        expected_pubkey: nostr::PublicKey,
        seed: impl IntoIterator<Item = nostr::Event>,
        write_outcome: AuthRelayWriteOutcome,
        disconnect_after_auth_connections: usize,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind AUTH relay");
        listener
            .set_nonblocking(true)
            .expect("make AUTH relay listener cancellable");
        let port = listener.local_addr().expect("AUTH relay address").port();
        let url = RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("parse AUTH relay URL");
        let state = Arc::new(Mutex::new(AuthRelayState {
            events: seed.into_iter().map(|event| (event.id, event)).collect(),
            observation: AuthRelayObservation::default(),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_url = url.clone();
        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);
        let join = thread::spawn(move || {
            run_auth_required_relay(
                listener,
                thread_url,
                expected_pubkey,
                write_outcome,
                disconnect_after_auth_connections,
                thread_state,
                thread_shutdown,
            );
        });

        Self {
            url,
            state,
            shutdown,
            join: Mutex::new(Some(join)),
        }
    }

    fn url(&self) -> RelayUrl {
        self.url.clone()
    }

    fn observation(&self) -> AuthRelayObservation {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .observation
            .clone()
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self
            .join
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            if let Err(panic) = join.join() {
                // Preserve the relay failure as the test failure, but never
                // double-panic and abort the whole test process when Drop is
                // already running because a foreground assertion failed.
                if !thread::panicking() {
                    std::panic::resume_unwind(panic);
                }
            }
        }
    }
}

impl Drop for AuthRequiredRelay {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_auth_required_relay(
    listener: TcpListener,
    relay: RelayUrl,
    expected_pubkey: nostr::PublicKey,
    write_outcome: AuthRelayWriteOutcome,
    disconnect_after_auth_connections: usize,
    state: Arc<Mutex<AuthRelayState>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !shutdown.load(Ordering::Acquire) {
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let worker = workers.swap_remove(index);
                worker.join().expect("join completed AUTH connection");
            } else {
                index += 1;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let relay = relay.clone();
                let state = Arc::clone(&state);
                let shutdown = Arc::clone(&shutdown);
                workers.push(thread::spawn(move || {
                    serve_auth_required_socket(
                        stream,
                        relay,
                        expected_pubkey,
                        write_outcome,
                        disconnect_after_auth_connections,
                        state,
                        shutdown,
                    );
                }));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_error) if shutdown.load(Ordering::Acquire) => break,
            Err(error) => panic!("accept AUTH relay connection: {error}"),
        }
    }
    for worker in workers {
        worker.join().expect("join AUTH connection at shutdown");
    }
}

fn serve_auth_required_socket(
    stream: std::net::TcpStream,
    relay: RelayUrl,
    expected_pubkey: nostr::PublicKey,
    write_outcome: AuthRelayWriteOutcome,
    disconnect_after_auth_connections: usize,
    state: Arc<Mutex<AuthRelayState>>,
    shutdown: Arc<AtomicBool>,
) {
    stream
        .set_nonblocking(false)
        .expect("make accepted AUTH relay socket blocking");
    let _ = stream.set_nodelay(true);
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(_) => {
            // The engine's separate one-shot NIP-11 probe converts this
            // ws:// URL to HTTP on the same port. This fixture deliberately
            // has no relay-information document: let tungstenite reject that
            // non-upgrade request without weakening or blocking WS AUTH.
            return;
        }
    };
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set AUTH relay read timeout");

    let connection = {
        let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
        state.observation.connections += 1;
        state.observation.connections
    };
    let challenge = format!("nmp-capstone-{relay}-{connection}");
    {
        let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
        assert!(
            !state.observation.challenges.contains(&challenge),
            "every transport generation must receive a unique challenge"
        );
        state.observation.challenges.push(challenge.clone());
    }
    send_relay_message(&mut ws, RelayMessage::auth(challenge.clone()));

    run_auth_required_connection(
        &mut ws,
        &relay,
        expected_pubkey,
        &challenge,
        write_outcome,
        connection <= disconnect_after_auth_connections,
        &state,
        &shutdown,
    );
}

#[allow(clippy::too_many_arguments)]
fn run_auth_required_connection(
    ws: &mut WebSocket<std::net::TcpStream>,
    relay: &RelayUrl,
    expected_pubkey: nostr::PublicKey,
    challenge: &str,
    write_outcome: AuthRelayWriteOutcome,
    disconnect_after_auth: bool,
    state: &Arc<Mutex<AuthRelayState>>,
    shutdown: &Arc<AtomicBool>,
) {
    let mut authenticated = false;
    let mut subscriptions: BTreeMap<String, (SubscriptionId, Vec<NostrFilter>)> = BTreeMap::new();

    while !shutdown.load(Ordering::Acquire) {
        let message = match ws.read() {
            Ok(Message::Text(text)) => text.as_str().to_string(),
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(WebSocketError::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed) => break,
            Err(_) if shutdown.load(Ordering::Acquire) => break,
            Err(error) => panic!("read AUTH relay frame: {error}"),
        };
        let message = ClientMessage::from_json(message).expect("parse client wire frame");

        if !authenticated {
            match message {
                ClientMessage::Auth(event) => {
                    let event = event.into_owned();
                    match validate_auth_event(&event, expected_pubkey, relay, challenge) {
                        Ok(()) => {
                            state
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .observation
                                .auth_events
                                .push(event.clone());
                            send_relay_message(
                                ws,
                                RelayMessage::ok(event.id, true, "authenticated"),
                            );
                            authenticated = true;
                            if disconnect_after_auth {
                                let _ = ws.close(None);
                                break;
                            }
                        }
                        Err(reason) => {
                            state
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .observation
                                .invalid_auth
                                .push(reason.clone());
                            send_relay_message(
                                ws,
                                RelayMessage::ok(
                                    event.id,
                                    false,
                                    format!("auth-required: {reason}"),
                                ),
                            );
                        }
                    }
                }
                ClientMessage::Req {
                    subscription_id, ..
                } => {
                    state
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .observation
                        .pre_auth_reqs += 1;
                    send_relay_message(
                        ws,
                        RelayMessage::closed(
                            subscription_id.into_owned(),
                            "auth-required: authenticate before REQ",
                        ),
                    );
                }
                ClientMessage::Event(event) => {
                    let event = event.into_owned();
                    state
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .observation
                        .pre_auth_events += 1;
                    send_relay_message(
                        ws,
                        RelayMessage::ok(
                            event.id,
                            false,
                            "auth-required: authenticate before EVENT",
                        ),
                    );
                }
                ClientMessage::Close(_) => {}
                ClientMessage::NegOpen { .. } | ClientMessage::Count { .. } => {
                    state
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .observation
                        .pre_auth_reqs += 1;
                }
                ClientMessage::NegMsg { .. } | ClientMessage::NegClose { .. } => {}
            }
            continue;
        }

        match message {
            ClientMessage::Req {
                subscription_id,
                filters,
            } => {
                let subscription_id = subscription_id.into_owned();
                let filters: Vec<NostrFilter> = filters
                    .into_iter()
                    .map(|filter| filter.into_owned())
                    .collect();
                let matching: Vec<nostr::Event> = state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .events
                    .values()
                    .filter(|event| {
                        filters
                            .iter()
                            .any(|filter| filter.match_event(event, MatchEventOptions::default()))
                    })
                    .cloned()
                    .collect();
                for event in matching {
                    send_relay_message(ws, RelayMessage::event(subscription_id.clone(), event));
                }
                send_relay_message(ws, RelayMessage::eose(subscription_id.clone()));
                subscriptions.insert(subscription_id.to_string(), (subscription_id, filters));
            }
            ClientMessage::Close(subscription_id) => {
                subscriptions.remove(&subscription_id.to_string());
            }
            ClientMessage::Event(event) => {
                let event = event.into_owned();
                assert_ne!(
                    event.kind,
                    Kind::Authentication,
                    "AUTH must use the dedicated client frame, never ordinary EVENT"
                );
                event.verify().expect("ordinary relay EVENT must verify");
                state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .observation
                    .ordinary_events
                    .push(event.clone());

                match write_outcome {
                    AuthRelayWriteOutcome::Reject => send_relay_message(
                        ws,
                        RelayMessage::ok(event.id, false, "blocked: test rejects ordinary writes"),
                    ),
                    AuthRelayWriteOutcome::Ack => {
                        {
                            let mut state =
                                state.lock().unwrap_or_else(|poison| poison.into_inner());
                            state.events.insert(event.id, event.clone());
                            state.observation.accepted_ordinary_events.push(event.id);
                        }
                        send_relay_message(ws, RelayMessage::ok(event.id, true, "saved"));
                        for (subscription_id, filters) in subscriptions.values() {
                            if filters.iter().any(|filter| {
                                filter.match_event(&event, MatchEventOptions::default())
                            }) {
                                send_relay_message(
                                    ws,
                                    RelayMessage::event(subscription_id.clone(), event.clone()),
                                );
                            }
                        }
                    }
                }
            }
            ClientMessage::Auth(event) => {
                let event = event.into_owned();
                send_relay_message(
                    ws,
                    RelayMessage::ok(event.id, false, "auth-required: already authenticated"),
                );
            }
            ClientMessage::Count {
                subscription_id, ..
            } => {
                send_relay_message(
                    ws,
                    RelayMessage::closed(
                        subscription_id.into_owned(),
                        "restricted: COUNT not implemented by capstone fixture",
                    ),
                );
            }
            ClientMessage::NegOpen { .. }
            | ClientMessage::NegMsg { .. }
            | ClientMessage::NegClose { .. } => {}
        }
    }
}

fn send_relay_message(ws: &mut WebSocket<std::net::TcpStream>, message: RelayMessage<'_>) {
    ws.send(Message::text(message.as_json()))
        .expect("send AUTH relay frame");
}

/// Validate the complete reducer-frozen kind:22242 template, not merely the
/// two NIP-42 tags. Tag order and arity are exact; id and signature are
/// verified; and the event must have been minted at current runtime time.
fn validate_auth_event(
    event: &nostr::Event,
    expected_pubkey: nostr::PublicKey,
    relay: &RelayUrl,
    challenge: &str,
) -> Result<(), String> {
    if event.pubkey != expected_pubkey {
        return Err("wrong frozen pubkey".to_string());
    }
    if event.kind != Kind::Authentication {
        return Err("wrong kind; expected 22242".to_string());
    }
    if !event.content.is_empty() {
        return Err("AUTH content must be empty".to_string());
    }
    let actual_tags: Vec<Vec<String>> = event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect();
    let expected_tags = vec![
        vec!["challenge".to_string(), challenge.to_string()],
        vec!["relay".to_string(), relay.to_string()],
    ];
    if actual_tags != expected_tags {
        return Err(format!(
            "wrong AUTH tag order/content: expected {expected_tags:?}, got {actual_tags:?}"
        ));
    }
    event
        .verify()
        .map_err(|error| format!("invalid AUTH id/signature: {error}"))?;
    let now = Timestamp::now().as_secs();
    if now.abs_diff(event.created_at.as_secs()) > 10 {
        return Err(format!(
            "AUTH created_at is not current: now={now}, event={}",
            event.created_at.as_secs()
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct AllowAuth;

impl AuthPolicy for AllowAuth {
    fn evaluate(&self, _request: AuthPolicyRequest) -> AuthPolicyOp {
        AuthPolicyOp::allow()
    }
}

#[derive(Debug)]
struct DenyAuth;

impl AuthPolicy for DenyAuth {
    fn evaluate(&self, request: AuthPolicyRequest) -> AuthPolicyOp {
        AuthPolicyOp::deny(format!("test denies {}", request.challenge()))
    }
}

fn authenticated_demand(selection: Filter, pubkey: nostr::PublicKey) -> Demand {
    let source = if selection.authors.is_some() {
        SourceAuthority::AuthorOutboxes
    } else {
        SourceAuthority::Public
    };
    Demand::new(selection, source, AccessContext::Nip42(pubkey))
        .expect("authenticated capstone demand is valid")
}

fn authenticated_literal_kind1(pubkey: nostr::PublicKey) -> LiveQuery {
    LiveQuery(authenticated_demand(
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([pubkey.to_hex()]))),
            ..Filter::default()
        },
        pubkey,
    ))
}

/// Guard the capstone oracle itself: a correctly signed, current kind:22242
/// event for the right relay still fails when it echoes any challenge other
/// than the one owned by this exact transport generation.
#[test]
fn auth_relay_oracle_rejects_a_wrong_challenge() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://auth-oracle.example").unwrap();
    let wrong = nostr::EventBuilder::auth("stale-challenge", relay.clone())
        .custom_created_at(Timestamp::now())
        .sign_with_keys(&keys)
        .expect("sign wrong-challenge fixture");
    let error = validate_auth_event(&wrong, keys.public_key(), &relay, "current-challenge")
        .expect_err("wrong challenge must fail the strict relay oracle");
    assert!(error.contains("wrong AUTH tag order/content"));
}

// ===========================================================================
// THE FLAGSHIP: watermark_cold_start_offline (plan §5 test 9, ledger #7)
// ===========================================================================

/// Phase 1 (online): subscribe against a real relay, wait for the plain
/// REQ/EOSE round trip to land 3 seeded events AND the query's relay
/// source to reach a proven `reconciled_through` (a source-scoped
/// watermark, persisted to the `RedbStore` file). Phase 2 (offline): shut
/// the relay down, spawn a brand-new engine on the SAME redb file,
/// subscribe the SAME query again. The FIRST batch on the fresh
/// subscription is computed entirely inside `EngineCore::on_subscribe`
/// (`recompile` + `refresh_handle`) -- both pure, no I/O -- so it is
/// available with zero network round trips: this test asserts that batch
/// already shows the 3 persisted rows AND a `reconciled_through: Some(_)`
/// on the relay's own source entry, proving the watermark survived the
/// restart and makes a cold, offline read evidence-backed rather than a
/// (wrongly) empty cache-miss. This fresh process never once connects to
/// the (now-dead) relay, so the SAME source's `status` reads `Connecting`
/// throughout -- the load-bearing orthogonality proof
/// (`docs/design/scoped-evidence-49-12-plan.md` Q3): a proven watermark and
/// a not-currently-reachable link status coexist on the SAME
/// `SourceEvidence`, neither shadowing the other (the sibling falsifier
/// `source_watermark_survives_disconnect_alongside_the_disconnected_status`
/// in `core_headless.rs` proves the same fact via an explicit
/// connect-then-disconnect sequence instead of a cold restart).
///
/// A second, never-queried-before shape (kind:1 authored by `b`, whose
/// write relay is registered up front but has no coverage row) is asserted
/// to read an UNPROVEN `reconciled_through: None` in the SAME offline
/// engine -- the falsifier's other half: "no row = not covered" must still
/// hold, offline, distinguishing a genuine unknown from a proven-empty
/// watermark. If either half regresses (offline reads unproven when it
/// should be proven, or a never-reconciled shape reads proven when it should
/// be unproven), ledger #7 is not real and this assertion fails loudly
/// rather than being softened.
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
            RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
        )
        .expect("test engine thread construction");

        let (_qh, rows_rx) = handle
            .subscribe(literal_kind1(&a.public_key().to_hex()))
            .expect("test subscription construction");

        assert!(
            wait_for_rows(&rows_rx, Duration::from_secs(10), |rows, evidence| {
                let ids: BTreeSet<String> = rows.iter().map(|r| r.id.to_hex()).collect();
                ids == post_ids
                    && source_for(evidence, &url).is_some_and(|s| s.reconciled_through.is_some())
            }),
            "phase 1 (online) must fetch all 3 seeded posts and reach a proven \
             reconciled_through via a real EOSE"
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
            RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
        )
        .expect("test engine thread construction");

        // THE assertion: a's shape reads back from cache, offline, as a
        // proven `reconciled_through` -- with zero network round trips
        // (this batch is available the instant `subscribe` returns; the
        // bounded wait below is a safety margin, not evidence of a network
        // wait having occurred). The SAME source's `status` is `Connecting`
        // (this process never once connects to the dead relay) -- proving
        // the watermark and the link status are independent facts, neither
        // shadowing the other.
        let (_qh_a, rows_rx_a) = handle
            .subscribe(literal_kind1(&a.public_key().to_hex()))
            .expect("test subscription construction");
        assert!(
            wait_for_rows(&rows_rx_a, Duration::from_secs(5), |rows, evidence| {
                let ids: BTreeSet<String> = rows.iter().map(|r| r.id.to_hex()).collect();
                ids == post_ids
                    && source_for(evidence, &url).is_some_and(|s| {
                        s.reconciled_through.is_some() && s.status == SourceStatus::Connecting
                    })
            }),
            "offline cold read must retain source-scoped evidence: a proven \
             reconciled_through for this relay, serving the 3 cached rows with zero network, \
             coexisting with a Connecting link status -- if reconciled_through is None, \
             ledger #7 is not real"
        );

        // Control: b's shape has no coverage row anywhere and must read an
        // unproven `reconciled_through: None` -- "no row = not covered"
        // must survive the restart just as faithfully as the proven case
        // does.
        let (_qh_b, rows_rx_b) = handle
            .subscribe(literal_kind1(&b.public_key().to_hex()))
            .expect("test subscription construction");
        assert!(
            wait_for_rows(&rows_rx_b, Duration::from_secs(5), |rows, evidence| {
                rows.is_empty()
                    && source_for(evidence, &url).is_some_and(|s| s.reconciled_through.is_none())
            }),
            "a never-reconciled shape must read an unproven reconciled_through, never a proven one \
             -- a proven-empty watermark must not be confused with a genuine cache-miss"
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
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )
    .expect("test engine thread construction");
    handle
        .add_signer(LocalKeySigner::new(a.clone()))
        .expect("local signer has a public key");
    handle.set_active_account(Some(a.public_key()));

    let (_qh, rows_rx) = handle
        .subscribe(literal_kind1(&a.public_key().to_hex()))
        .expect("test subscription construction");

    let shared_post_id = shared_post.id.to_hex();

    // Wait until BOTH relays' own sources independently prove their window
    // (each relay's `reconciled_through` is its OWN fact -- no joint
    // unanimity verdict anywhere under the new evidence model) AND the
    // falsifier itself: exactly ONE row for this id, never two, despite two
    // independent relay deliveries of the identical event.
    assert!(
        wait_for_rows(&rows_rx, Duration::from_secs(10), |rows, evidence| {
            let matching = rows
                .iter()
                .filter(|r| r.id.to_hex() == shared_post_id)
                .count();
            matching == 1
                && source_for(evidence, &url_1).is_some_and(|s| s.reconciled_through.is_some())
                && source_for(evidence, &url_2).is_some_and(|s| s.reconciled_through.is_some())
        }),
        "the shared post must surface as EXACTLY ONE row once both relays' own sources have \
         independently proven their window -- a duplicate row here would mean the two-relay \
         delivery leaked a second, un-deduplicated copy into the read result"
    );

    handle.shutdown();
    engine_thread.join();
    relay_1.shutdown();
    relay_2.shutdown();
}

// ===========================================================================
// Ledger #9 (live) -- enqueue != converged, per-relay ack split.
// ===========================================================================

/// Plan §5 test 11, live tier: publish a `Durable` intent to TWO real
/// relays, one of which accepts and one of which is configured to reject
/// every event. Asserts the FULL shape of ledger #9: the first status is
/// never a terminal (`Accepted` only), both relays are individually `Sent`
/// to, and the two relays resolve to DIFFERENT terminals (`Acked` vs
/// `Rejected`) -- there is no way to read "is it sent" except by observing
/// this per-relay stream.
#[test]
fn write_ack_per_relay_over_real_relays() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let a = Keys::generate();

    // Both physical sessions require NIP-42 and challenge proactively. This
    // makes the original Acked/Rejected assertions impossible to reach via
    // a Public-session bypass: the relay refuses any ordinary EVENT before
    // the exact account has authenticated that exact connection.
    let relay_ok = AuthRequiredRelay::spawn(a.public_key(), [], AuthRelayWriteOutcome::Ack, 0);
    let url_ok = relay_ok.url();
    let relay_bad = AuthRequiredRelay::spawn(a.public_key(), [], AuthRelayWriteOutcome::Reject, 0);
    let url_bad = relay_bad.url();

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
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )
    .expect("test engine thread construction");
    let signer_registration = handle
        .add_signer(LocalKeySigner::new(a.clone()))
        .expect("local signer has a public key");
    let policy_registration = handle
        .add_auth_policy(a.public_key(), AllowAuth)
        .expect("install exact-account permissive AUTH policy");
    handle.set_active_account(Some(a.public_key()));

    let unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        "durable publish, two relays, one rejects",
    );
    let receipt_rx = handle
        .publish(WriteIntent {
            payload: WritePayload::Unsigned(unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("receipt id allocation");

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
            .any(|s| matches!(s, WriteStatus::Sent { relay: r, .. } if r == &url_ok)),
        "must observe Sent(relay_ok) (got: {seen:?})"
    );
    assert!(
        seen.iter()
            .any(|s| matches!(s, WriteStatus::Sent { relay: r, .. } if r == &url_bad)),
        "must observe Sent(relay_bad) (got: {seen:?})"
    );
    assert!(acked_ok, "relay_ok must reach Acked (got stream: {seen:?})");
    assert!(
        rejected_bad,
        "relay_bad must reach Rejected, DISTINCT from relay_ok's Acked \
         (got stream: {seen:?})"
    );

    let ok = relay_ok.observation();
    let bad = relay_bad.observation();
    for (name, observation) in [("accepting", &ok), ("rejecting", &bad)] {
        assert_eq!(
            observation.connections, 1,
            "{name} relay must have exactly one protected physical session and no Public bypass"
        );
        assert_eq!(
            observation.pre_auth_reqs, 0,
            "{name} relay must receive no protected REQ before AUTH"
        );
        assert_eq!(
            observation.pre_auth_events, 0,
            "{name} relay must receive no protected EVENT before AUTH"
        );
        assert!(
            observation.invalid_auth.is_empty(),
            "{name} relay must receive the exact frozen AUTH template: {:?}",
            observation.invalid_auth
        );
        assert_eq!(
            observation.auth_events.len(),
            1,
            "{name} relay must authenticate its one physical session exactly once"
        );
        assert_eq!(
            observation.ordinary_events.len(),
            1,
            "AUTH kind:22242 must never count as, consume, or duplicate the one durable write"
        );
    }
    assert_eq!(ok.accepted_ordinary_events.len(), 1);
    assert!(bad.accepted_ordinary_events.is_empty());

    assert!(
        handle.remove_auth_policy(policy_registration),
        "the exact opaque AUTH policy registration must remove its installation"
    );
    assert!(
        handle.remove_signer(signer_registration),
        "the exact signer registration must remove its installation"
    );

    handle.shutdown();
    engine_thread.join();
    relay_ok.shutdown();
    relay_bad.shutdown();
}

/// A real protected read with an app-owned denial never emits AUTH, REQ, or
/// EVENT. The query's ordinary evidence names the denial for this exact
/// session instead of falling back to Public or converting denial into a
/// generic transport failure.
#[test]
fn auth_policy_denial_keeps_real_relay_work_parked() {
    let a = Keys::generate();
    let relay = AuthRequiredRelay::spawn(a.public_key(), [], AuthRelayWriteOutcome::Ack, 0);
    let url = relay.url();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [url.clone()]);
    let (engine_thread, handle) = EngineThread::spawn(
        nmp_store::MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
        // #524/#528 resolved-IP admission: the loopback capstone fixture is
        // an explicit operator opt-in, exactly like a real local relay.
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )
    .expect("test engine thread construction");
    let policy_registration = handle
        .add_auth_policy(a.public_key(), DenyAuth)
        .expect("install exact-account denying AUTH policy");

    let (_query, rows) = handle
        .subscribe(authenticated_literal_kind1(a.public_key()))
        .expect("subscribe protected query");
    let denied = wait_for_rows(&rows, Duration::from_secs(10), |current, evidence| {
        current.is_empty()
            && source_for(evidence, &url)
                .is_some_and(|source| source.status == SourceStatus::AuthDenied)
    });
    let observation = relay.observation();
    assert!(
        denied,
        "policy denial must surface as exact-session AuthDenied evidence; relay={observation:?}"
    );
    assert_eq!(observation.connections, 1);
    assert_eq!(observation.challenges.len(), 1);
    assert!(observation.auth_events.is_empty());
    assert!(observation.invalid_auth.is_empty());
    assert_eq!(observation.pre_auth_reqs, 0);
    assert_eq!(observation.pre_auth_events, 0);
    assert!(observation.ordinary_events.is_empty());
    assert!(handle.remove_auth_policy(policy_registration));

    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}

/// The first authenticated generation is deliberately disconnected
/// immediately after its AUTH OK. The durable write can converge only after
/// the transport reconnects, receives a different challenge, authenticates
/// again, and sends the one application EVENT. No AUTH preamble or stale
/// challenge is reusable across the generation boundary.
#[test]
fn reconnect_requires_a_fresh_real_relay_challenge() {
    let a = Keys::generate();
    let relay = AuthRequiredRelay::spawn(a.public_key(), [], AuthRelayWriteOutcome::Ack, 1);
    let url = relay.url();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [url.clone()]);
    let (engine_thread, handle) = EngineThread::spawn(
        nmp_store::MemoryStore::new(),
        dir,
        10,
        PoolConfig {
            reconnect_delay_initial: Some(Duration::from_millis(20)),
            ..PoolConfig::default()
        },
        // #524/#528 resolved-IP admission: the loopback capstone fixture is
        // an explicit operator opt-in, exactly like a real local relay.
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )
    .expect("test engine thread construction");
    let signer_registration = handle
        .add_signer(LocalKeySigner::new(a.clone()))
        .expect("local signer has a public key");
    let policy_registration = handle
        .add_auth_policy(a.public_key(), AllowAuth)
        .expect("install exact-account permissive AUTH policy");
    handle.set_active_account(Some(a.public_key()));

    let receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Unsigned(UnsignedEvent::new(
                a.public_key(),
                Timestamp::now(),
                Kind::TextNote,
                vec![],
                "fresh challenge after reconnect",
            )),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("receipt id allocation");
    assert!(
        wait_for_status(&receipt, Duration::from_secs(20), |status| {
            matches!(status, WriteStatus::Acked(acked) if acked == &url)
        }),
        "durable write must converge after a fresh second-generation AUTH"
    );

    let observation = relay.observation();
    assert_eq!(observation.connections, 2);
    assert_eq!(observation.challenges.len(), 2);
    assert_ne!(observation.challenges[0], observation.challenges[1]);
    assert_eq!(
        observation.auth_events.len(),
        2,
        "each exact transport generation must authenticate independently"
    );
    assert!(observation.invalid_auth.is_empty());
    assert_eq!(observation.pre_auth_reqs, 0);
    assert_eq!(observation.pre_auth_events, 0);
    assert_eq!(
        observation.ordinary_events.len(),
        1,
        "two AUTH events must not become two write attempts"
    );
    assert_eq!(observation.accepted_ordinary_events.len(), 1);

    assert!(handle.remove_auth_policy(policy_registration));
    assert!(handle.remove_signer(signer_registration));
    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}

// ===========================================================================
// Depth-2 grammar over a real relay: SetOp(Diff, [Derived(follows),
// Derived(mutes)]) -- M1 contract test 9's shape, actually resolving live.
// ===========================================================================

fn follows_minus_mutes_demand(pubkey: nostr::PublicKey) -> Demand {
    let follows = Binding::Derived(Box::new(Derived {
        inner: authenticated_demand(
            Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            pubkey,
        ),
        project: Selector::Tag("p".to_string()),
    }));
    let mutes = Binding::Derived(Box::new(Derived {
        inner: authenticated_demand(
            Filter {
                kinds: Some(BTreeSet::from([10_000u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            pubkey,
        ),
        project: Selector::Tag("p".to_string()),
    }));
    authenticated_demand(
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::SetOp(Box::new(SetOp {
                op: SetAlgebra::Diff,
                operands: vec![follows, mutes],
            }))),
            ..Filter::default()
        },
        pubkey,
    )
}

/// `kinds:[1], authors := SetOp(Diff, [Derived(follows), Derived(mutes)])`
/// (M1 contract test 9's exact shape) driven end-to-end over a real relay:
/// `a` follows both `b` and `c`, then mutes `c`. `b`'s pre-seeded post must
/// surface; `c`'s pre-seeded post must NOT -- proving the two-level
/// Derived+SetOp cascade actually resolves through live kind:3/kind:10000
/// deliveries and the full runtime stack, not just scripted `EngineMsg`s.
#[test]
fn follows_minus_mutes_resolves_over_a_real_relay() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .try_init();
    let a = Keys::generate();
    let b = Keys::generate();
    let c = Keys::generate();
    let b_relay_keys = mirror_keys(&b);
    let c_relay_keys = mirror_keys(&c);

    let b_post: RelayEvent = RelayEventBuilder::text_note("b's post -- should surface")
        .finalize(&b_relay_keys)
        .expect("sign b's post");
    let c_post: RelayEvent = RelayEventBuilder::text_note("c's post -- must NOT surface (muted)")
        .finalize(&c_relay_keys)
        .expect("sign c's post");

    // Bridge the seed events back into the workspace's pinned nostr version
    // by JSON, then require the entire nested demand graph to use A's exact
    // authenticated context. The strict relay makes any accidentally-Public
    // inner or outer demand observable as a pre-auth REQ refusal.
    let b_seed = nostr::Event::from_json(
        serde_json::to_string(&b_post).expect("serialize b seed across nostr versions"),
    )
    .expect("bridge b seed event");
    let c_seed = nostr::Event::from_json(
        serde_json::to_string(&c_post).expect("serialize c seed across nostr versions"),
    )
    .expect("bridge c seed event");
    let relay = AuthRequiredRelay::spawn(
        a.public_key(),
        [b_seed, c_seed],
        AuthRelayWriteOutcome::Ack,
        0,
    );
    let url = relay.url();

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
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )
    .expect("test engine thread construction");
    let signer_registration = handle
        .add_signer(LocalKeySigner::new(a.clone()))
        .expect("local signer has a public key");
    let policy_registration = handle
        .add_auth_policy(a.public_key(), AllowAuth)
        .expect("install exact-account permissive AUTH policy");

    handle.set_active_account(Some(a.public_key()));
    let (_qh, rows_rx) = handle
        .subscribe(LiveQuery(follows_minus_mutes_demand(a.public_key())))
        .expect("test subscription construction");

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
    let contact_receipt_rx = handle
        .publish(WriteIntent {
            payload: WritePayload::Unsigned(contact_list),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("receipt id allocation");
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
    let mute_receipt_rx = handle
        .publish(WriteIntent {
            payload: WritePayload::Unsigned(mute_list),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("receipt id allocation");
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
        wait_for_rows(&rows_rx, Duration::from_secs(15), |rows, _evidence| {
            let ids: BTreeSet<String> = rows.iter().map(|r| r.id.to_hex()).collect();
            ids.contains(&b_post.id.to_hex()) && !ids.contains(&c_post.id.to_hex())
        }),
        "follows-minus-mutes must surface b's post and exclude c's (muted) once both the \
         contact list and the mute list have resolved"
    );

    let observation = relay.observation();
    assert_eq!(
        observation.connections, 1,
        "the full graph must not open a second Public physical session"
    );
    assert_eq!(
        observation.pre_auth_reqs, 0,
        "no inner or outer protected demand may emit REQ before exact-session AUTH"
    );
    assert_eq!(
        observation.pre_auth_events, 0,
        "neither protected write may emit EVENT before exact-session AUTH"
    );
    assert!(
        observation.invalid_auth.is_empty(),
        "relay must validate the exact frozen AUTH event: {:?}",
        observation.invalid_auth
    );
    assert_eq!(
        observation.auth_events.len(),
        1,
        "the full nested query and both writes must share one exact authenticated session"
    );
    assert_eq!(
        observation.ordinary_events.len(),
        2,
        "AUTH kind:22242 is separate from the two application writes"
    );
    assert_eq!(observation.accepted_ordinary_events.len(), 2);

    assert!(handle.remove_auth_policy(policy_registration));
    assert!(handle.remove_signer(signer_registration));

    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}
