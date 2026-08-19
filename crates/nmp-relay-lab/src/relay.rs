//! The running endpoint: one address answering both protocols a real relay
//! answers -- a websocket upgrade becomes a NIP-01 session, and a plain `GET`
//! returns the NIP-11 document -- with every downstream frame authored by the
//! script.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr::filter::MatchEventOptions;
use nostr::{Event, JsonUtil, RelayUrl};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

use crate::script::{Nip11, Reply, ReqFrame, Script, Serve, Step, Upgrade};
use crate::wire::{Direction, WireLog, WireRecord};
use crate::ws;

/// One thing the writer task puts on (or does to) the socket.
enum Outbound {
    Text(String),
    /// Octets the SCRIPT asked for. Recorded, because a scenario about
    /// injected bytes has to be able to assert they were written.
    Bytes(Vec<u8>),
    /// Octets this crate emits on its own account -- a pong answering a
    /// keepalive ping. Deliberately NOT recorded: it is not something the
    /// scenario said, and counting it would keep `wait_quiet` awake forever
    /// on any relay a client pings.
    Control(Vec<u8>),
    Stall,
    Disconnect,
    Close { code: u16, reason: String },
}

type Out = mpsc::UnboundedSender<Outbound>;

/// A scripted relay, listening.
pub struct RelayLab {
    url: RelayUrl,
    addr: SocketAddr,
    wire: WireLog,
    corpus: Arc<Mutex<Vec<Event>>>,
    connections: Arc<AtomicU64>,
    sessions: Arc<AtomicU64>,
    kills: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
    shutdown: Option<oneshot::Sender<()>>,
    accept: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for RelayLab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayLab")
            .field("url", &self.url)
            .field("connections", &self.connection_count())
            .finish()
    }
}

impl RelayLab {
    /// Bind an ephemeral loopback port and start serving `script`.
    pub async fn start(script: Script) -> Self {
        Self::start_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), script).await
    }

    /// Start on a SPECIFIC port -- what a "the relay comes back" step needs,
    /// so NMP's own pool reconnects to the `RelayUrl` it already has open.
    pub async fn start_on_port(port: u16, script: Script) -> Self {
        Self::start_on(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), script).await
    }

    async fn start_on(addr: SocketAddr, script: Script) -> Self {
        let listener = TcpListener::bind(addr)
            .await
            .expect("nmp-relay-lab: the relay address must bind");
        let addr = listener.local_addr().expect("bound address");
        let url = RelayUrl::parse(&format!("ws://{addr}"))
            .expect("nmp-relay-lab: the relay URL must parse");

        let corpus = Arc::new(Mutex::new(std::mem::take(&mut { script.corpus.clone() })));
        let wire = WireLog::default();
        let connections = Arc::new(AtomicU64::new(0));
        let sessions = Arc::new(AtomicU64::new(0));
        let kills: Arc<Mutex<Vec<oneshot::Sender<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let accept = tokio::spawn(accept_loop(
            listener,
            Arc::new(script),
            Arc::clone(&corpus),
            wire.clone(),
            Arc::clone(&connections),
            Arc::clone(&kills),
            Arc::new(Mutex::new(RelayCounters::default())),
            Arc::clone(&sessions),
            shutdown_rx,
        ));

        Self {
            url,
            addr,
            wire,
            corpus,
            connections,
            sessions,
            kills,
            shutdown: Some(shutdown_tx),
            accept: Some(accept),
        }
    }

    /// The `ws://` URL an app puts in its relay list.
    #[must_use]
    pub fn url(&self) -> &RelayUrl {
        &self.url
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The wire recorder: what each side actually sent.
    #[must_use]
    pub fn wire(&self) -> &WireLog {
        &self.wire
    }

    /// Everything both sides sent so far.
    #[must_use]
    pub fn record(&self) -> WireRecord {
        self.wire.snapshot()
    }

    /// How many TCP connections this address has accepted, ever -- websocket
    /// sessions and NIP-11 document fetches alike, because one accept is one
    /// connection either way.
    ///
    /// Almost every scenario wants [`Self::session_count`] instead: NMP
    /// fetches the document over plain HTTP on this same address, so this
    /// number moves for reasons that have nothing to do with the socket the
    /// scenario is about.
    #[must_use]
    pub fn connection_count(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }

    /// How many WEBSOCKET sessions this relay has served, ever. A second
    /// session is a reconnect -- or a second engine.
    #[must_use]
    pub fn session_count(&self) -> u64 {
        self.sessions.load(Ordering::Relaxed)
    }

    /// Stage more pre-existing state mid-scenario, after the relay is running.
    pub fn seed(&self, events: impl IntoIterator<Item = Event>) {
        self.corpus
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(events);
    }

    /// What this relay currently holds -- including writes it ingested.
    #[must_use]
    pub fn held(&self) -> Vec<Event> {
        self.corpus
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Sever every established connection and release the listener, and do
    /// not return until both have happened. A reconnect scenario can then
    /// rebind this exact port with [`Self::start_on_port`].
    pub async fn disconnect(mut self) -> u16 {
        let port = self.port();
        self.kill_all();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(accept) = self.accept.take() {
            let _ = accept.await;
        }
        port
    }

    fn kill_all(&self) {
        let kills = std::mem::take(&mut *self.kills.lock().unwrap_or_else(|p| p.into_inner()));
        for kill in kills {
            let _ = kill.send(());
        }
    }
}

impl Drop for RelayLab {
    fn drop(&mut self) {
        self.kill_all();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(accept) = self.accept.take() {
            accept.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    listener: TcpListener,
    script: Arc<Script>,
    corpus: Arc<Mutex<Vec<Event>>>,
    wire: WireLog,
    connections: Arc<AtomicU64>,
    kills: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
    counters: Arc<Mutex<RelayCounters>>,
    sessions: Arc<AtomicU64>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut live = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                connections.fetch_add(1, Ordering::Relaxed);
                let (kill_tx, kill_rx) = oneshot::channel();
                kills.lock().unwrap_or_else(|p| p.into_inner()).push(kill_tx);
                let script = Arc::clone(&script);
                let corpus = Arc::clone(&corpus);
                let wire = wire.clone();
                let counters = Arc::clone(&counters);
                let sessions = Arc::clone(&sessions);
                live.spawn(async move {
                    serve(stream, script, corpus, wire, counters, sessions, kill_rx).await;
                });
            }
            Some(_) = live.join_next(), if !live.is_empty() => {}
        }
    }
    drop(listener);
    live.abort_all();
    while live.join_next().await.is_some() {}
}

/// Read the request head, then take whichever of the two protocols it asked
/// for. Only the client speaks first in both, so there is nothing to guess.
async fn serve(
    mut stream: TcpStream,
    script: Arc<Script>,
    corpus: Arc<Mutex<Vec<Event>>>,
    wire: WireLog,
    counters: Arc<Mutex<RelayCounters>>,
    sessions: Arc<AtomicU64>,
    kill: oneshot::Receiver<()>,
) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while ws::head_end(&head).is_none() {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return,
            Ok(_) => head.push(byte[0]),
        }
        if head.len() > 16 * 1024 {
            return;
        }
    }

    if !ws::is_upgrade(&head) {
        answer_nip11(stream, &script.nip11).await;
        return;
    }

    match &script.upgrade {
        Upgrade::Hang => {
            // Hold the socket open, answer nothing. The connection ends when
            // the relay is torn down.
            let _ = kill.await;
            return;
        }
        Upgrade::Http {
            status,
            content_type,
            body,
        } => {
            // A captive portal answers the upgrade with an ordinary page.
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
            return;
        }
        Upgrade::Accept => {}
    }

    let Some(key) = ws::websocket_key(&head) else {
        return;
    };
    if stream
        .write_all(&ws::handshake_response(&key))
        .await
        .is_err()
    {
        return;
    }

    // The connection id is minted HERE, once a websocket session actually
    // begins -- never on accept. NMP fetches the NIP-11 document over plain
    // HTTP on the same address, so accept-time ids leave holes in the record
    // and `on_connection(0)` names a document fetch rather than a session.
    let connection = wire.next_connection();
    sessions.fetch_add(1, Ordering::Relaxed);

    let (mut read, write) = stream.into_split();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Outbound>();
    let (dead_tx, dead_rx) = oneshot::channel();
    let writer = tokio::spawn(write_loop(write, out_rx, wire.clone(), connection, dead_tx));

    if let Some(greeting) = &script.on_connect {
        let ctx = ProgramCtx {
            sub_id: None,
            event: None,
            filters: Vec::new(),
            corpus: Arc::clone(&corpus),
            out: out_tx.clone(),
            wire: wire.clone(),
        };
        let steps = greeting.steps.clone();
        tokio::spawn(async move { run_program(steps, ctx).await });
    }

    let session = SessionState {
        connection,
        script: Arc::clone(&script),
        corpus,
        wire: wire.clone(),
        out: out_tx,
        counters,
        live_subs: BTreeMap::new(),
    };

    let read_loop = async move {
        let mut session = session;
        let mut decoder = ws::Decoder::default();
        let mut buf = vec![0u8; 8192];
        loop {
            let read_bytes = match read.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            decoder.push(&buf[..read_bytes]);
            loop {
                match decoder.take_message() {
                    ws::Decoded::Incomplete => break,
                    ws::Decoded::Fault(fault) => {
                        session.wire.fault(fault);
                        return;
                    }
                    ws::Decoded::Message(frame) => match frame.opcode {
                        ws::Opcode::Text => {
                            session.wire.record(
                                session.connection,
                                Direction::Up,
                                frame.payload.clone(),
                            );
                            session.dispatch(&frame.payload);
                        }
                        ws::Opcode::Ping => {
                            let _ = session
                                .out
                                .send(Outbound::Control(ws::pong_frame(&frame.payload)));
                        }
                        ws::Opcode::Pong => {}
                        ws::Opcode::Close => return,
                        other => {
                            session
                                .wire
                                .fault(format!("unexpected client frame opcode {other:?}"));
                            return;
                        }
                    },
                }
            }
        }
    };

    tokio::select! {
        _ = read_loop => {}
        _ = dead_rx => {}
        _ = kill => {}
    }
    writer.abort();
}

async fn answer_nip11(mut stream: TcpStream, document: &Nip11) {
    let response = match document {
        Nip11::Document(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/nostr+json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        Nip11::None => {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        }
    };
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// The single writer: every program's frames go through one FIFO, so two
/// concurrent programs can never interleave mid-frame.
async fn write_loop(
    mut write: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<Outbound>,
    wire: WireLog,
    connection: usize,
    dead: oneshot::Sender<()>,
) {
    while let Some(item) = rx.recv().await {
        match item {
            Outbound::Text(payload) => {
                wire.record(connection, Direction::Down, payload.clone().into_bytes());
                if write.write_all(&ws::text_frame(&payload)).await.is_err() {
                    break;
                }
            }
            Outbound::Bytes(bytes) => {
                wire.record(connection, Direction::Down, bytes.clone());
                if write.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            Outbound::Control(bytes) => {
                if write.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            Outbound::Close { code, reason } => {
                let _ = write.write_all(&ws::close_frame(code, &reason)).await;
                break;
            }
            Outbound::Disconnect => {
                let _ = dead.send(());
                return;
            }
            Outbound::Stall => {
                // Stop writing, forever, WITHOUT closing: the write half stays
                // alive in this future so the peer sees an open socket that
                // never answers. Only teardown ends it.
                std::future::pending::<()>().await;
            }
        }
    }
    let _ = dead.send(());
}

/// Per-rule match counters and the REQ ordinal, shared by every connection.
///
/// RELAY-wide, not per-connection, and that is the whole of `on_nth_req`'s
/// meaning: "fail the first REQ, then behave" is a statement about this
/// relay, and NMP drops and reopens the socket whenever demand goes to zero.
/// Per-connection counters make the rule fire once per reconnect, which is
/// not what any scenario author means and is invisible until a reconnect
/// happens to land inside the scenario.
#[derive(Debug, Default)]
struct RelayCounters {
    reqs: usize,
    rule_hits: BTreeMap<usize, usize>,
}

struct SessionState {
    connection: usize,
    script: Arc<Script>,
    corpus: Arc<Mutex<Vec<Event>>>,
    wire: WireLog,
    out: Out,
    counters: Arc<Mutex<RelayCounters>>,
    live_subs: BTreeMap<String, AbortHandle>,
}

impl SessionState {
    fn dispatch(&mut self, payload: &[u8]) {
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(payload) else {
            self.wire.fault(format!(
                "client text frame is not JSON: {:?}",
                String::from_utf8_lossy(payload)
            ));
            return;
        };
        let Some(array) = message.as_array() else {
            self.wire
                .fault(format!("client message is not a NIP-01 array: {message}"));
            return;
        };
        match array.first().and_then(serde_json::Value::as_str) {
            Some("REQ") => self.on_req(array),
            Some("CLOSE") => {
                if let Some(sub_id) = array.get(1).and_then(serde_json::Value::as_str) {
                    if let Some(handle) = self.live_subs.remove(sub_id) {
                        handle.abort();
                    }
                }
            }
            Some("EVENT") => self.on_event(array),
            Some("AUTH") => self.on_auth(array),
            Some("COUNT") => {
                let _ = self.out.send(Outbound::Text(
                    serde_json::json!(["NOTICE", "relay-lab: COUNT is not implemented"]).to_string(),
                ));
            }
            _ => self
                .wire
                .fault(format!("unknown client NIP-01 verb: {message}")),
        }
    }

    fn on_req(&mut self, array: &[serde_json::Value]) {
        let Some(sub_id) = array.get(1).and_then(serde_json::Value::as_str) else {
            self.wire
                .fault("REQ without a subscription id".to_string());
            return;
        };
        let raw_filters = array[2..].to_vec();
        let filters = raw_filters
            .iter()
            .filter_map(|f| serde_json::from_value::<nostr::Filter>(f.clone()).ok())
            .collect::<Vec<_>>();
        let index = {
            let mut counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
            let index = counters.reqs;
            counters.reqs += 1;
            index
        };
        let frame = ReqFrame {
            connection: self.connection,
            sub_id: sub_id.to_string(),
            raw_filters,
            filters: filters.clone(),
            index,
        };

        // A cap the relay never advertised: CLOSE the excess. Checked before
        // any rule, because a relay at its ceiling refuses the request rather
        // than answering it badly. A REQ replacing a live subscription costs
        // no new slot, which is what NIP-01 replacement means.
        if let Some((cap, message)) = &self.script.subscription_cap {
            if !self.live_subs.contains_key(sub_id) && self.live_subs.len() >= *cap {
                let _ = self.out.send(Outbound::Text(
                    serde_json::json!(["CLOSED", sub_id, message]).to_string(),
                ));
                return;
            }
        }

        let reply = self.matching_reply(&frame);
        let ctx = ProgramCtx {
            sub_id: Some(sub_id.to_string()),
            event: None,
            filters,
            corpus: Arc::clone(&self.corpus),
            out: self.out.clone(),
            wire: self.wire.clone(),
        };
        let handle = tokio::spawn(run_program(reply.steps, ctx));
        if let Some(previous) = self.live_subs.insert(sub_id.to_string(), handle.abort_handle()) {
            // NIP-01 replacement: the previous filter set stops being served.
            previous.abort();
        }
    }

    fn matching_reply(&mut self, frame: &ReqFrame) -> Reply {
        let mut counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
        for (index, rule) in self.script.req_rules.iter().enumerate() {
            if !rule.when.matches(frame) {
                continue;
            }
            let hits = counters.rule_hits.entry(index).or_default();
            *hits += 1;
            match rule.only_nth {
                Some(n) if *hits != n => continue,
                _ => return rule.then.clone(),
            }
        }
        Reply::stored()
    }

    fn on_event(&mut self, array: &[serde_json::Value]) {
        let Some(body) = array.get(1) else {
            self.wire.fault("EVENT without a body".to_string());
            return;
        };
        let event = match serde_json::from_value::<Event>(body.clone()) {
            Ok(event) => event,
            Err(error) => {
                // A body this crate's `nostr` cannot even parse is refused
                // with the prefix NIP-01 has for it, never silently dropped.
                let id = body
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let _ = self.out.send(Outbound::Text(
                    serde_json::json!([
                        "OK",
                        id,
                        false,
                        format!("invalid: unparseable event ({error})")
                    ])
                    .to_string(),
                ));
                return;
            }
        };

        // Verified by DEFAULT (`Script::accepts_unverified_writes` opts out):
        // a relay that admits an unsigned or mis-identified event is not one
        // any client has to survive, and a scenario built on it proves
        // nothing about the real world.
        if self.script.verify_writes {
            let refusal = if !event.verify_id() {
                Some("invalid: event id does not match its content")
            } else if !event.verify_signature() {
                Some("invalid: signature verification failed")
            } else {
                None
            };
            if let Some(message) = refusal {
                let _ = self.out.send(Outbound::Text(
                    serde_json::json!(["OK", event.id.to_hex(), false, message]).to_string(),
                ));
                return;
            }
        }

        let reply = self
            .script
            .event_rules
            .iter()
            .find(|rule| rule.when.matches(&event))
            .map(|rule| rule.then.clone())
            .unwrap_or_else(Reply::ok);

        let ctx = ProgramCtx {
            sub_id: None,
            event: Some(event),
            filters: Vec::new(),
            corpus: Arc::clone(&self.corpus),
            out: self.out.clone(),
            wire: self.wire.clone(),
        };
        tokio::spawn(run_program(reply.steps, ctx));
    }

    fn on_auth(&mut self, array: &[serde_json::Value]) {
        let event = array
            .get(1)
            .and_then(|body| serde_json::from_value::<Event>(body.clone()).ok());
        let reply = self
            .script
            .auth_reply
            .clone()
            .unwrap_or_else(|| Reply::new().then_ok(""));
        let ctx = ProgramCtx {
            sub_id: None,
            event,
            filters: Vec::new(),
            corpus: Arc::clone(&self.corpus),
            out: self.out.clone(),
            wire: self.wire.clone(),
        };
        tokio::spawn(run_program(reply.steps, ctx));
    }
}

struct ProgramCtx {
    sub_id: Option<String>,
    event: Option<Event>,
    filters: Vec<nostr::Filter>,
    corpus: Arc<Mutex<Vec<Event>>>,
    out: Out,
    wire: WireLog,
}

impl ProgramCtx {
    fn send(&self, message: serde_json::Value) {
        let _ = self.out.send(Outbound::Text(message.to_string()));
    }

    fn require_sub(&self, step: &str) -> Option<&str> {
        match &self.sub_id {
            Some(sub_id) => Some(sub_id.as_str()),
            None => {
                self.wire.fault(format!(
                    "{step} names a subscription, but this reply was triggered by \
                     something that is not a REQ"
                ));
                None
            }
        }
    }
}

async fn run_program(steps: Vec<Step>, ctx: ProgramCtx) {
    for step in steps {
        match step {
            Step::Stored(serve) => {
                let Some(sub_id) = ctx.require_sub("Step::Stored") else {
                    return;
                };
                for event in select_stored(&ctx.corpus, &ctx.filters, serve) {
                    ctx.send(serde_json::json!([
                        "EVENT",
                        sub_id,
                        serde_json::from_str::<serde_json::Value>(&event.as_json())
                            .expect("an event always renders as JSON")
                    ]));
                }
            }
            Step::Events(events) => {
                let Some(sub_id) = ctx.require_sub("Step::Events") else {
                    return;
                };
                for event in events {
                    ctx.send(serde_json::json!([
                        "EVENT",
                        sub_id,
                        serde_json::from_str::<serde_json::Value>(&event.as_json())
                            .expect("an event always renders as JSON")
                    ]));
                }
            }
            Step::EventsJson(bodies) => {
                let Some(sub_id) = ctx.require_sub("Step::EventsJson") else {
                    return;
                };
                for body in bodies {
                    ctx.send(serde_json::json!(["EVENT", sub_id, body]));
                }
            }
            Step::Eose => {
                let Some(sub_id) = ctx.require_sub("Step::Eose") else {
                    return;
                };
                ctx.send(serde_json::json!(["EOSE", sub_id]));
            }
            Step::Closed(message) => {
                let Some(sub_id) = ctx.require_sub("Step::Closed") else {
                    return;
                };
                ctx.send(serde_json::json!(["CLOSED", sub_id, message]));
            }
            Step::Notice(message) => ctx.send(serde_json::json!(["NOTICE", message])),
            Step::Auth(challenge) => ctx.send(serde_json::json!(["AUTH", challenge])),
            Step::Ok { accepted, message } => {
                let id = ctx
                    .event
                    .as_ref()
                    .map(|event| event.id.to_hex())
                    .unwrap_or_default();
                if id.is_empty() {
                    ctx.wire.fault(
                        "Step::Ok names an event id, but this reply was triggered by \
                         something that carries no event"
                            .to_string(),
                    );
                    return;
                }
                ctx.send(serde_json::json!(["OK", id, accepted, message]));
            }
            Step::Ingest => {
                if let Some(event) = ctx.event.clone() {
                    ctx.corpus
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(event);
                }
            }
            Step::Raw(payload) => {
                let _ = ctx.out.send(Outbound::Text(payload));
            }
            Step::Bytes(bytes) => {
                let _ = ctx.out.send(Outbound::Bytes(bytes));
            }
            Step::PartialFrame {
                payload,
                keep_bytes,
            } => {
                let mut frame = ws::text_frame(&payload);
                frame.truncate(keep_bytes.min(frame.len()));
                let _ = ctx.out.send(Outbound::Bytes(frame));
            }
            Step::PartialEvent { event, keep_bytes } => {
                let Some(sub_id) = ctx.require_sub("Step::PartialEvent") else {
                    return;
                };
                let payload = serde_json::json!([
                    "EVENT",
                    sub_id,
                    serde_json::from_str::<serde_json::Value>(&event.as_json())
                        .expect("an event always renders as JSON")
                ])
                .to_string();
                let mut frame = ws::text_frame(&payload);
                frame.truncate(keep_bytes.min(frame.len()));
                let _ = ctx.out.send(Outbound::Bytes(frame));
            }
            Step::Delay(delay) => tokio::time::sleep(delay).await,
            Step::Stall => {
                let _ = ctx.out.send(Outbound::Stall);
                return;
            }
            Step::Disconnect => {
                let _ = ctx.out.send(Outbound::Disconnect);
                return;
            }
            Step::Close { code, reason } => {
                let _ = ctx.out.send(Outbound::Close { code, reason });
                return;
            }
        }
    }
}

/// Everything the corpus holds for this REQ, newest first.
///
/// Each filter's own NIP-01 `limit` bounds that filter's slice, as a real
/// relay does; the union is then capped by [`Serve::AtMost`] REGARDLESS of
/// what the client asked for -- which is the whole of "serve N for a filter
/// with limit M where N < M", and is invisible to the client.
fn select_stored(
    corpus: &Arc<Mutex<Vec<Event>>>,
    filters: &[nostr::Filter],
    serve: Serve,
) -> Vec<Event> {
    let held = corpus.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let mut selected: Vec<Event> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for filter in filters {
        let mut matching: Vec<&Event> = held
            .iter()
            .filter(|event| filter.match_event(event, MatchEventOptions::new()))
            .collect();
        matching.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        if let Some(limit) = filter.limit {
            matching.truncate(limit);
        }
        for event in matching {
            if seen.insert(event.id) {
                selected.push(event.clone());
            }
        }
    }

    selected.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    if let Serve::AtMost(n) = serve {
        selected.truncate(n);
    }
    selected
}

/// Wait for the relay to be reachable at all. Not usually needed -- the
/// listener is bound before `start` returns -- but a scenario that hands the
/// URL somewhere before opening anything can state it.
pub async fn wait_reachable(addr: SocketAddr, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}
