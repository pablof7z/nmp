//! Shared scenario vocabulary. `mod support;` from each test file.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::{
    Binding, Demand, Engine, EngineConfig, Filter, LiveQuery, ReadRouting, Row, RowDelta,
    Subscription,
};
use nmp_relay_lab::RelayLab;
use nostr::{Event, EventBuilder, Keys, RelayUrl, Timestamp};

/// How long a scenario waits for the engine to do something it should do
/// promptly. Generous, because the assertion is never "it was fast".
pub const SETTLE: Duration = Duration::from_secs(20);
/// How long the wire must stay silent before a count off it is settled.
pub const QUIET: Duration = Duration::from_millis(250);

/// A signed kind:1 note at a stated instant.
pub fn note(author: &Keys, content: &str, created_at: u64) -> Event {
    EventBuilder::text_note(content)
        .custom_created_at(Timestamp::from_secs(created_at))
        .sign_with_keys(author)
        .expect("a fixture note signs cleanly")
}

/// `n` notes by one author, one second apart, oldest first.
pub fn notes(author: &Keys, n: usize, from: u64) -> Vec<Event> {
    (0..n)
        .map(|i| note(author, &format!("note {i}"), from + i as u64))
        .collect()
}

/// An engine whose only relay is this one.
pub fn engine_against(relay: &RelayLab) -> Engine {
    Engine::new(EngineConfig {
        app_relays: vec![relay.url().to_string()],
        ..EngineConfig::default()
    })
    .expect("an engine with one app relay builds")
}

/// A literal `kinds:[1], authors:[author]` live query, PINNED to one relay.
///
/// Explicit rather than `ReadRouting::Auto`, and that is not incidental: an
/// engine built from an `EngineConfig` naming this relay in `app_relays`
/// sends it writes but issues no READ against it for a query like this one --
/// the socket is never even opened. Every read scenario here therefore pins
/// its relay, the same way `crates/nmp/tests/finished_stored_events.rs` and
/// `integration_capstone.rs` do. See this crate's report: it is a finding
/// about the routing surface, not a property of the harness.
pub fn kind1_by_on(author: &Keys, relay: &RelayUrl) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([1u16])),
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

/// A literal one-kind, one-author query pinned to `relay`.
pub fn kind_by_on(author: &Keys, relay: &RelayLab, kind: u16) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([kind])),
                authors: Some(Binding::Literal(BTreeSet::from([author
                    .public_key()
                    .to_hex()]))),
                ..Filter::default()
            },
            ReadRouting::Explicit(vec![relay.url().clone()]),
        )
        .expect("a one-relay pinned set is nonempty"),
    )
}

/// [`kind_by_on`], pinned to a read session that authenticates as `reader`.
///
/// `Demand::authenticate_as` is what makes a PROTECTED read session exist at
/// all. Left `None` -- the default -- the reads ride the connection bound to
/// nobody, which is documented as never authenticating, so a relay that gates
/// reads simply never gets an answer. A scenario about NIP-42 on the read
/// path has to say who is asking.
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

/// [`kind1_by_on`] against the relay the scenario is driving.
pub fn kind1_by(author: &Keys, relay: &RelayLab) -> LiveQuery {
    kind1_by_on(author, relay.url())
}

/// Drain the subscription until `budget` elapses, returning every row the app
/// was ever shown, newest state per event id, in first-seen order.
///
/// Deliberately drains the WHOLE budget rather than stopping at a count: a
/// scenario about silent truncation is asserting the app was shown no more,
/// and stopping as soon as the expected number arrived would make that
/// assertion unfalsifiable.
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

/// Drain until at least `want` distinct rows have arrived, or `budget`
/// elapses. For the scenarios where the count is a floor, not a ceiling.
pub fn rows_until(subscription: &Subscription, want: usize, budget: Duration) -> Vec<Row> {
    let deadline = Instant::now() + budget;
    let mut rows: Vec<Row> = Vec::new();
    while rows.len() < want {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return rows;
        }
        let Ok(frame) = subscription.recv_timeout(remaining) else {
            return rows;
        };
        for delta in frame.deltas {
            if let RowDelta::Added(row) = delta {
                rows.push(row);
            }
        }
    }
    rows
}

/// An engine with a local account, ready to publish.
pub fn publishing_engine(relay: &RelayLab, keys: &Keys) -> Engine {
    let engine = engine_against(relay);
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), true)
        .expect("the account and its local provider register");
    engine
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

/// Drain a receipt until one relay fact for this relay is terminal, or the
/// budget runs out. Returns every fact seen, so a failure reports the whole
/// history rather than "timed out".
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
/// pass without the client binding to anything, and NMP cannot be used to
/// demonstrate that because it always binds correctly.
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

    /// Every complete server frame that arrives within `budget`, decoded.
    /// Server frames are unmasked, so this decoder is the short half of the
    /// one in `nmp_relay_lab::ws`.
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

/// An [`nmp::AuthPolicy`] that consents to every challenge.
///
/// NMP will not authenticate to a relay without one. That is deliberate and
/// it is the right default -- proving an identity to a stranger is the app's
/// decision, not the engine's -- but it means an AUTH scenario that registers
/// an account and stops there gets silence, and the silence looks exactly
/// like a client that ignores challenges.
pub struct AllowAnyChallenge;

impl nmp::AuthPolicy for AllowAnyChallenge {
    fn evaluate(&self, _request: nmp::AuthPolicyRequest) -> nmp::AuthPolicyOp {
        nmp::AuthPolicyOp::allow()
    }
}

/// An engine that holds `reader`'s key AND consents to authenticate with it.
pub fn authenticating_engine(relay: &RelayLab, reader: &Keys) -> Engine {
    let engine = engine_against(relay);
    engine
        .add_private_key_account(&reader.secret_key().to_secret_bytes(), true)
        .expect("the account and its local provider register");
    // The registration handle is deliberately leaked rather than dropped:
    // dropping it would withdraw the policy, and every scenario here wants it
    // to outlive the call that installed it.
    std::mem::forget(
        engine
            .add_auth_policy(reader.public_key(), AllowAnyChallenge)
            .expect("the auth policy registers"),
    );
    engine
}
