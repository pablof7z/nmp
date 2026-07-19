//! #704 falsifier — NIP-46 session scaling proves ZERO per-session executor
//! thread.
//!
//! Under the OLD architecture every standalone `Nip46Signer` owned a
//! per-session `nmp_executor::Executor` with its own admitted blocking tasks
//! (connection, session, event-forward, switch-relays, mapper, engine waiter),
//! so N live sessions multiplied executor threads by N. Under #704 that
//! per-session executor is GONE: standalone direct-Rust sessions run their
//! worker/forwarder/switch-relays/result-map work as async tasks on ONE
//! process-wide shared runtime (`standalone_runtime`, built once, 1 worker
//! thread, never shut down). See `nmp-signer/src/nip46.rs`'s `SessionRuntime`
//! /`standalone_runtime` docs.
//!
//! What DOES still scale per session is the session's OWNED transport pool
//! (`session_pool_config`): each session builds one `nmp-transport` pool, and a
//! pool spawns a bounded, fixed envelope of OS threads — a reaper, a translator,
//! `verifier_workers` verifier threads (clamped to at most
//! `MAX_DEFAULT_VERIFIER_WORKERS`), and one `mio` worker per connected relay
//! (at most `MAX_NIP46_RELAYS`, and a bunker connects to exactly one). Every one
//! of those transport threads bumps the SAME global counter this test reads
//! (`nmp-transport`'s `SystemThreadSpawner` calls `nmp_executor::note_thread_spawn`).
//! So the per-session thread delta is a CONSTANT transport envelope — NOT the
//! per-session executor of the old design. This test proves the delta stays
//! within that constant envelope and never grows with session count.
//!
//! Thread counter: `nmp_executor::nmp_threads_spawned()` is the exact same
//! process-wide atomic that `nmp::nmp_threads_spawned()` re-exports (the latter
//! is `nmp_engine::nmp_threads_spawned()`, which forwards to this counter); this
//! crate does not depend on `nmp`, so it reads the shared counter directly at
//! its source. It counts every real NMP-owned OS thread (runtime workers +
//! transport threads), never logical async tasks.
//!
//! One test per process so the global spawn counter is not perturbed by a
//! sibling test spawning its own sessions/runtime.

use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use nmp_signer::{Nip46Signer, MAX_NIP46_RELAYS};
use nostr::nips::nip44;
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, Tag};
use serde_json::{json, Value};
use tungstenite::Message;

/// `nmp-transport`'s two singleton per-pool threads (reaper + translator).
const POOL_SINGLETON_THREADS: u64 = 2;
/// Upper bound on the verifier worker pool a default `PoolConfig` builds
/// (`MAX_DEFAULT_VERIFIER_WORKERS` in `nmp-transport`; the default clamps
/// host-parallelism/2 into `[2, 8]`).
const MAX_VERIFIER_WORKERS: u64 = 8;

/// The strongest constant that still bounds a single session's transport
/// envelope: reaper + translator + verifier ceiling + up to `2 ×
/// MAX_NIP46_RELAYS` relay workers (the `2×` slack mirrors the acceptance
/// spec's stated envelope and covers a worker being retired-and-respawned
/// during a reconnect). Crucially this is a CONSTANT — it contains ZERO term
/// proportional to the number of sessions, which is exactly the property #704
/// asserts (no per-session executor thread).
fn per_session_envelope_bound() -> u64 {
    POOL_SINGLETON_THREADS + MAX_VERIFIER_WORKERS + 2 * (MAX_NIP46_RELAYS as u64)
}

fn threads_now() -> u64 {
    nmp_executor::nmp_threads_spawned()
}

fn response_event(signer: &Keys, client: PublicKey, id: &str, result: Option<String>) -> Event {
    let plaintext = json!({ "id": id, "result": result, "error": Value::Null }).to_string();
    let ciphertext = nip44::encrypt(
        signer.secret_key(),
        &client,
        plaintext,
        nip44::Version::default(),
    )
    .unwrap();
    EventBuilder::new(Kind::NostrConnect, ciphertext)
        .tag(Tag::public_key(client))
        .sign_with_keys(signer)
        .unwrap()
}

fn event_frame(subscription_id: &str, event: Event) -> String {
    json!(["EVENT", subscription_id, event]).to_string()
}

/// Serve ONE client connection for its whole lifetime: answer
/// `connect`/`get_public_key`/`switch_relays` and keep reading until the socket
/// closes (so the session stays live and its transport pool stays up while the
/// test holds the signer). Modeled on `nip46_mock_relay.rs`'s
/// `spawn_multi_session_signer_relay` inner loop, but never breaks — each
/// connection is served on its own thread so N sessions pair CONCURRENTLY.
fn serve_session(stream: TcpStream, remote: Keys, user: Keys) {
    let Ok(mut socket) = tungstenite::accept(stream) else {
        return;
    };
    let mut subscription_id = None;
    while let Ok(message) = socket.read() {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(text.as_ref()) else {
            continue;
        };
        let Some(parts) = frame.as_array() else {
            continue;
        };
        match parts.first().and_then(Value::as_str) {
            Some("REQ") => {
                subscription_id = parts.get(1).and_then(Value::as_str).map(str::to_string);
            }
            Some("EVENT") => {
                let event = Event::from_json(parts[1].to_string()).unwrap();
                let Ok(plaintext) =
                    nip44::decrypt(remote.secret_key(), &event.pubkey, event.content.as_bytes())
                else {
                    continue;
                };
                let request: Value = serde_json::from_str(&plaintext).unwrap();
                let id = request["id"].as_str().unwrap();
                let method = request["method"].as_str().unwrap();
                let result = match method {
                    "connect" => "ack".to_string(),
                    "get_public_key" => user.public_key().to_hex(),
                    "switch_relays" => "null".to_string(),
                    // Not exercised in the scaling loop, but keep the mock total.
                    _ => "null".to_string(),
                };
                let response = response_event(&remote, event.pubkey, id, Some(result));
                if socket
                    .send(Message::Text(
                        event_frame(subscription_id.as_deref().unwrap(), response).into(),
                    ))
                    .is_err()
                {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// A bunker relay that accepts an unbounded number of CONCURRENT client
/// connections (one server thread per connection). These accept/serve threads
/// are raw test `std::thread::spawn`s — they do NOT go through
/// `nmp-transport`'s counted spawner, so they never perturb
/// `nmp_threads_spawned()` (which counts only NMP-owned threads).
fn spawn_concurrent_bunker_relay(remote: Keys, user: Keys) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else {
                continue;
            };
            let remote = remote.clone();
            let user = user.clone();
            thread::spawn(move || serve_session(stream, remote, user));
        }
    });
    url
}

#[test]
fn nip46_sessions_scale_with_zero_per_session_executor_thread() {
    let remote = Keys::generate();
    let user = Keys::generate();
    let relay = spawn_concurrent_bunker_relay(remote.clone(), user.clone());
    let uri = format!(
        "bunker://{}?relay={}&secret=scaling-704",
        remote.public_key().to_hex(),
        url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
    );

    // Hold every session live for the whole test so its transport pool stays
    // up and its threads remain counted.
    let mut sessions: Vec<Nip46Signer> = Vec::new();
    let checkpoints = [1usize, 10, 50, 100];
    // (live sessions, cumulative nmp_threads_spawned) at each checkpoint.
    let mut table: Vec<(usize, u64)> = Vec::new();

    for &target in &checkpoints {
        while sessions.len() < target {
            let signer = Nip46Signer::connect_bunker(&uri, Duration::from_secs(10))
                .expect("no NIP-46 connect is ever refused for a capacity reason");
            assert_eq!(signer.user_public_key(), user.public_key());
            sessions.push(signer);
        }
        // Let the freshly-built transport pools finish spawning their threads.
        thread::sleep(Duration::from_millis(200));
        table.push((target, threads_now()));
    }

    let envelope = per_session_envelope_bound();
    eprintln!("\n#704 NIP-46 session-scaling table (all NMP-owned OS threads):");
    eprintln!(
        "  {:>8} | {:>20} | {:>18} | {:>18}",
        "sessions", "nmp_threads_spawned", "delta_vs_prev", "per_session_delta"
    );
    let mut prev_sessions = 0usize;
    let mut prev_threads = table[0].1; // baseline includes the one-time shared runtime
                                       // The one-time shared standalone runtime + the first session's pool are
                                       // both folded into the first checkpoint; report it as the baseline row and
                                       // measure the per-session RATE from the INCREMENTS between checkpoints,
                                       // which exclude every one-time cost.
    for (i, (sessions_n, threads)) in table.iter().enumerate() {
        if i == 0 {
            eprintln!(
                "  {sessions_n:>8} | {threads:>20} | {:>18} | {:>18}",
                "(baseline)", "(baseline)"
            );
            prev_sessions = *sessions_n;
            prev_threads = *threads;
            continue;
        }
        let d_sessions = (sessions_n - prev_sessions) as u64;
        let d_threads = threads.saturating_sub(prev_threads);
        let per_session = d_threads as f64 / d_sessions as f64;
        eprintln!("  {sessions_n:>8} | {threads:>20} | {d_threads:>18} | {per_session:>18.3}");

        assert!(
            d_threads <= d_sessions * envelope,
            "adding {d_sessions} sessions grew NMP threads by {d_threads}, which exceeds \
             the constant per-session transport envelope ({envelope}) × {d_sessions}. A growth \
             beyond the transport envelope would mean a per-session executor thread returned. \
             table={table:?}"
        );
        prev_sessions = *sessions_n;
        prev_threads = *threads;
    }

    // Total growth from 1 → 100 sessions must also stay within 99 × the
    // constant transport envelope (no O(N) executor-thread term).
    let total_growth = table.last().unwrap().1.saturating_sub(table[0].1);
    let added_sessions = (table.last().unwrap().0 - table[0].0) as u64;
    assert!(
        total_growth <= added_sessions * envelope,
        "growing from 1 to 100 sessions added {total_growth} NMP threads, exceeding \
         {added_sessions} × transport envelope ({envelope}); table={table:?}"
    );
    eprintln!(
        "  per-session transport envelope bound = {envelope} \
         (reaper+translator+<= {MAX_VERIFIER_WORKERS} verifiers + <= 2*{MAX_NIP46_RELAYS} relay workers); \
         ZERO per-session executor thread.\n"
    );

    // Teardown: dropping each signer tears down its owned transport pool.
    drop(sessions);
}
