//! The vocabulary every scenario is written in: keys, notes, queries, engines
//! and a raw socket.
//!
//! Library code rather than a test-support module, because the scenarios that
//! use it are library code too. There is no test target here to hide a
//! fixture in.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::{
    AuthPolicy, AuthPolicyOp, AuthPolicyRequest, Binding, Demand, Engine, EngineConfig, Filter,
    LiveQuery, ReadRouting, Row, RowDelta, Subscription,
};
use nostr::{Event, EventBuilder, Keys, RelayUrl, Timestamp};

use crate::RelayLab;

/// How long a scenario waits for something that should happen promptly. The
/// claim is never "it was fast", so this is generous.
pub const SETTLE: Duration = Duration::from_secs(20);
/// How long the wire must stay silent before a count taken off it is settled.
pub const QUIET: Duration = Duration::from_millis(250);

/// A signed kind:1 note at a stated instant.
#[must_use]
pub fn note(author: &Keys, content: &str, created_at: u64) -> Event {
    EventBuilder::text_note(content)
        .custom_created_at(Timestamp::from_secs(created_at))
        .sign_with_keys(author)
        .expect("a fixture note signs cleanly")
}

/// `n` notes by one author, one second apart, oldest first.
#[must_use]
pub fn notes(author: &Keys, n: usize, from: u64) -> Vec<Event> {
    (0..n)
        .map(|i| note(author, &format!("note {i}"), from + i as u64))
        .collect()
}

/// An engine whose only relay is this one.
#[must_use]
pub fn engine_against(relay: &RelayLab) -> Engine {
    Engine::new(EngineConfig {
        app_relays: vec![relay.url().to_string()],
        ..EngineConfig::default()
    })
    .expect("an engine with one app relay builds")
}

/// An engine holding this key, ready to publish.
#[must_use]
pub fn publishing_engine(relay: &RelayLab, keys: &Keys) -> Engine {
    let engine = engine_against(relay);
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), true)
        .expect("the account and its local provider register");
    engine
}

/// An [`AuthPolicy`] that consents to every challenge.
///
/// NMP will not authenticate without one, which is the right default -- proving
/// an identity to a stranger is the app's decision. It does mean a scenario
/// that registers an account and stops there gets silence, and the silence
/// looks exactly like a client that ignores challenges.
pub struct AllowAnyChallenge;

impl AuthPolicy for AllowAnyChallenge {
    fn evaluate(&self, _request: AuthPolicyRequest) -> AuthPolicyOp {
        AuthPolicyOp::allow()
    }
}

/// An engine that holds `reader`'s key AND consents to authenticate with it.
#[must_use]
pub fn authenticating_engine(relay: &RelayLab, reader: &Keys) -> Engine {
    let engine = engine_against(relay);
    engine
        .add_private_key_account(&reader.secret_key().to_secret_bytes(), true)
        .expect("the account and its local provider register");
    // Leaked deliberately: dropping the registration withdraws the policy, and
    // every scenario wants it to outlive the call that installed it.
    std::mem::forget(
        engine
            .add_auth_policy(reader.public_key(), AllowAnyChallenge)
            .expect("the auth policy registers"),
    );
    engine
}

/// A literal one-kind, one-author query PINNED to `relay`.
///
/// Explicit rather than `ReadRouting::Auto`, and not incidentally: an engine
/// whose `EngineConfig` names this relay in `app_relays` sends it writes but
/// issues no READ against it for a query like this -- the socket is never
/// opened. Every read scenario pins its relay.
#[must_use]
pub fn kind_by_on(author: &Keys, relay: &RelayUrl, kind: u16) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([kind])),
                authors: Some(Binding::Literal(BTreeSet::from([author
                    .public_key()
                    .to_hex()]))),
                ..Filter::default()
            },
            ReadRouting::Explicit(vec![relay.clone()]),
        )
        .expect("a one-relay pinned set is nonempty"),
    )
}

/// [`kind_by_on`] for kind:1 against the relay the scenario is driving.
#[must_use]
pub fn kind1_by(author: &Keys, relay: &RelayLab) -> LiveQuery {
    kind_by_on(author, relay.url(), 1)
}

/// [`kind1_by`], pinned to a read session that authenticates as `reader`.
///
/// `Demand::authenticate_as` is what makes a PROTECTED read session exist.
/// Left `None` -- the default -- the reads ride the connection bound to
/// nobody, which never authenticates, so a relay that gates reads never gets
/// an answer.
#[must_use]
pub fn kind1_by_as(author: &Keys, relay: &RelayLab, reader: &Keys) -> LiveQuery {
    let mut demand = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([author
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        },
        ReadRouting::Explicit(vec![relay.url().clone()]),
    )
    .expect("a one-relay pinned set is nonempty");
    demand.authenticate_as = Some(reader.public_key());
    LiveQuery::single(demand)
}

/// A kind:1 write intent whose destination NMP resolves itself.
pub fn publish_note(engine: &Engine, content: &str) -> nmp::ReceiptStream {
    engine
        .publish(nmp::WriteIntent {
            payload: nmp::WritePayload::Event(nmp::EventBuilder {
                kind: nostr::Kind::TextNote,
                tags: Vec::new(),
                content: content.to_string(),
                created_at: None,
            }),
            routing: nmp::WriteRouting::Auto,
            identity: nmp::Identity::Active,
        })
        .expect("the write is accepted")
}

/// Drain the whole budget and return every row the app was shown.
///
/// Deliberately drains all of it rather than stopping at a count: a scenario
/// about silent truncation asserts the app was shown NO MORE, and stopping as
/// soon as the expected number arrived makes that unfalsifiable.
#[must_use]
pub fn rows_within(subscription: &Subscription, budget: Duration) -> Vec<Row> {
    let deadline = Instant::now() + budget;
    let mut rows: Vec<Row> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return rows;
        }
        let Ok(frame) = subscription.recv_timeout(remaining) else {
            return rows;
        };
        for delta in frame.deltas {
            match delta {
                RowDelta::Added(row) => rows.push(row),
                RowDelta::Updated(row) => {
                    if let Some(slot) = rows.iter_mut().find(|held| held.id() == row.id()) {
                        *slot = row;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Drain a receipt until one relay fact is terminal, or the budget runs out.
/// Returns every fact seen, so a failure reports the history.
#[must_use]
pub fn relay_facts(
    receipts: &nmp::FifoReceiver<nmp::WriteFact>,
    budget: Duration,
) -> Vec<nmp::RelayState> {
    let deadline = Instant::now() + budget;
    let mut states = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return states;
        }
        match receipts.recv_timeout(remaining) {
            Ok(nmp::WriteFact::Relay { state, .. }) => {
                let terminal = matches!(
                    state,
                    nmp::RelayState::Published
                        | nmp::RelayState::Rejected { .. }
                        | nmp::RelayState::GaveUp
                );
                states.push(state);
                if terminal {
                    return states;
                }
            }
            Ok(_) => {}
            Err(_) => return states,
        }
    }
}

/// A raw websocket client, for saying things NMP would never say.
///
/// The relay's own validation has to be provable independently of NMP: a
/// fixture that accepts an unbound AUTH response makes every AUTH scenario
/// pass without the client binding to anything, and NMP cannot demonstrate
/// that because it always binds correctly.
pub struct RawSession {
    socket: tokio::net::TcpStream,
    buf: Vec<u8>,
}

impl RawSession {
    pub async fn connect(port: u16) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the relay accepts a plain TCP client");
        socket
            .write_all(
                b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
                  Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .await
            .expect("the upgrade is writable");
        let mut head = [0u8; 1024];
        let read = socket.read(&mut head).await.expect("the handshake reply");
        assert!(
            String::from_utf8_lossy(&head[..read]).starts_with("HTTP/1.1 101"),
            "the handshake must complete"
        );
        Self {
            socket,
            buf: Vec::new(),
        }
    }

    /// Send one masked text frame, as RFC 6455 requires of a client.
    pub async fn send(&mut self, payload: &str) {
        use tokio::io::AsyncWriteExt;
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let bytes = payload.as_bytes();
        let mut frame = vec![0x81];
        if bytes.len() < 126 {
            frame.push(0x80 | bytes.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.socket
            .write_all(&frame)
            .await
            .expect("the frame is writable");
    }

    /// Every complete server frame arriving within `budget`, decoded.
    pub async fn read_messages(&mut self, budget: Duration) -> Vec<serde_json::Value> {
        use tokio::io::AsyncReadExt;
        let deadline = Instant::now() + budget;
        let mut out = Vec::new();
        loop {
            while let Some(payload) = self.take_frame() {
                if let Ok(value) = serde_json::from_slice(&payload) {
                    out.push(value);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return out;
            }
            let mut chunk = [0u8; 8192];
            match tokio::time::timeout(remaining, self.socket.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return out,
                Ok(Ok(n)) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    fn take_frame(&mut self) -> Option<Vec<u8>> {
        if self.buf.len() < 2 {
            return None;
        }
        let short = (self.buf[1] & 0x7f) as usize;
        let (len, offset) = match short {
            126 => {
                if self.buf.len() < 4 {
                    return None;
                }
                (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4)
            }
            127 => {
                if self.buf.len() < 10 {
                    return None;
                }
                let mut be = [0u8; 8];
                be.copy_from_slice(&self.buf[2..10]);
                (u64::from_be_bytes(be) as usize, 10)
            }
            n => (n, 2),
        };
        if self.buf.len() < offset + len {
            return None;
        }
        let payload = self.buf[offset..offset + len].to_vec();
        self.buf.drain(..offset + len);
        Some(payload)
    }
}

/// A scratch path for a durable relay store, unique to this process.
#[must_use]
pub fn store_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nmp-relay-lab-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the scenario's own directory");
    dir.join("relay.jsonl")
}
