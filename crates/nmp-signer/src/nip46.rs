//! Current NIP-46 client adapter.
//!
//! The adapter owns an independent relay pool and exactly-correlated remote
//! RPCs. It deliberately does not own NMP's durable write retry/publication.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nmp_transport::{Pool, PoolConfig, PoolEvent, PoolEventSink, RelayFrame, WireFrame};
use nostr::nips::nip44;
use nostr::{
    ClientMessage, Event, EventBuilder, Filter, JsonUtil, Keys, Kind, PublicKey, RelayMessage,
    RelayUrl, SubscriptionId, Tag, UnsignedEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::{
    parse_bunker_uri, BunkerParseError, CryptoCapability, SignerError, SignerOp, SigningCapability,
    MAX_BUNKER_URI_LEN, MAX_NIP46_RELAYS,
};

const DEFAULT_PERMISSIONS: &str = "sign_event,nip44_encrypt,nip44_decrypt";
const MAX_PENDING_REQUESTS: usize = 64;
const SWITCH_RELAYS_TIMEOUT: Duration = Duration::from_secs(10);

struct Nip46CancellationInner {
    cancelled: AtomicBool,
    commands: Mutex<Option<Sender<WorkerMsg>>>,
}

/// Explicit cancellation for a connection attempt that has not produced a
/// [`Nip46Signer`] yet. Native wrappers own one handle and cancel it from
/// close/drop, so a pending handshake cannot outlive its connection object.
#[derive(Clone)]
pub struct Nip46Cancellation {
    inner: Arc<Nip46CancellationInner>,
}

impl Default for Nip46Cancellation {
    fn default() -> Self {
        Self {
            inner: Arc::new(Nip46CancellationInner {
                cancelled: AtomicBool::new(false),
                commands: Mutex::new(None),
            }),
        }
    }
}

impl Nip46Cancellation {
    fn bind(&self, commands: Sender<WorkerMsg>) {
        let mut current = self
            .inner
            .commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.inner.cancelled.load(Ordering::Acquire) {
            let _ = commands.send(WorkerMsg::Shutdown);
        } else {
            *current = Some(commands);
        }
    }

    /// Idempotently terminate the currently-bound handshake/session worker.
    pub fn cancel(&self) {
        if self.inner.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let commands = self
            .inner
            .commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(commands) = commands {
            let _ = commands.send(WorkerMsg::Shutdown);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nip46ClientMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nip46ConnectionEvent {
    Connecting,
    Available,
    Unavailable,
    RelayAuthentication(RelayUrl),
    AuthorizationRequired(String),
    Connected { user_public_key: PublicKey },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nip46Error {
    InvalidBunkerUri(BunkerParseError),
    MissingRelay,
    TooManyRelays(usize),
    InvitationTooLong(usize),
    InvalidRelay(String),
    InvalidLaunchScheme(String),
    InvalidInvitation,
    SecretMismatch,
    Timeout,
    Disconnected,
    Rejected(String),
    InvalidResponse(String),
}

impl fmt::Display for Nip46Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBunkerUri(error) => error.fmt(f),
            Self::MissingRelay => f.write_str("NIP-46 requires at least one relay"),
            Self::TooManyRelays(count) => {
                write!(f, "NIP-46 exceeds {MAX_NIP46_RELAYS} relays: {count}")
            }
            Self::InvitationTooLong(len) => {
                write!(
                    f,
                    "NIP-46 invitation exceeds {MAX_BUNKER_URI_LEN} bytes: {len}"
                )
            }
            Self::InvalidRelay(relay) => write!(f, "invalid NIP-46 relay: {relay}"),
            Self::InvalidLaunchScheme(scheme) => {
                write!(f, "invalid NIP-46 launch scheme: {scheme}")
            }
            Self::InvalidInvitation => f.write_str("invalid NIP-46 invitation"),
            Self::SecretMismatch => f.write_str("NIP-46 connect secret mismatch"),
            Self::Timeout => f.write_str("NIP-46 connection timed out"),
            Self::Disconnected => f.write_str("NIP-46 connection ended"),
            Self::Rejected(reason) => write!(f, "NIP-46 signer rejected request: {reason}"),
            Self::InvalidResponse(reason) => write!(f, "invalid NIP-46 response: {reason}"),
        }
    }
}

impl std::error::Error for Nip46Error {}

impl From<SignerError> for Nip46Error {
    fn from(value: SignerError) -> Self {
        match value {
            SignerError::Rejected(reason) => Self::Rejected(reason),
            SignerError::InvalidResponse(reason) => Self::InvalidResponse(reason),
            SignerError::Timeout => Self::Timeout,
            SignerError::Unavailable | SignerError::Disconnected => Self::Disconnected,
        }
    }
}

/// Client-initiated `nostrconnect://` session. It retains the disposable
/// client keypair and expected secret until the remote signer answers.
pub struct Nip46Invitation {
    client_keys: Keys,
    relays: Vec<RelayUrl>,
    secret: Zeroizing<String>,
    permissions: String,
    metadata: Nip46ClientMetadata,
}

impl fmt::Debug for Nip46Invitation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Nip46Invitation")
            .field("client_public_key", &self.client_keys.public_key())
            .field("relays", &self.relays)
            .field("secret", &"[redacted]")
            .field("permissions", &self.permissions)
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl Nip46Invitation {
    pub fn new(
        relays: Vec<RelayUrl>,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
    ) -> Result<Self, Nip46Error> {
        if relays.is_empty() {
            return Err(Nip46Error::MissingRelay);
        }
        if relays.len() > MAX_NIP46_RELAYS {
            return Err(Nip46Error::TooManyRelays(relays.len()));
        }
        let client_keys = Keys::generate();
        let secret = Keys::generate().public_key().to_hex();
        let invitation = Self {
            client_keys,
            relays,
            secret: Zeroizing::new(secret),
            permissions: permissions.unwrap_or_else(|| DEFAULT_PERMISSIONS.to_string()),
            metadata,
        };
        let len = invitation.uri().len();
        if len > MAX_BUNKER_URI_LEN {
            return Err(Nip46Error::InvitationTooLong(len));
        }
        Ok(invitation)
    }

    /// Generic system-chooser URI.
    #[must_use]
    pub fn uri(&self) -> String {
        self.uri_with_scheme("nostrconnect")
            .expect("the standard NIP-46 launch scheme is valid")
    }

    /// App-specific handoff URI (for example `primalconnect`). The scheme is
    /// only a launch affordance; the embedded NIP-46 origin and parameters are
    /// unchanged.
    pub fn uri_with_scheme(&self, scheme: &str) -> Result<String, Nip46Error> {
        if scheme.len() > 32 {
            return Err(Nip46Error::InvalidLaunchScheme(scheme.to_string()));
        }
        let mut url = url::Url::parse(&format!(
            "{}://{}",
            scheme,
            self.client_keys.public_key().to_hex()
        ))
        .map_err(|_| Nip46Error::InvalidLaunchScheme(scheme.to_string()))?;
        if url.scheme() != scheme {
            return Err(Nip46Error::InvalidLaunchScheme(scheme.to_string()));
        }
        {
            let mut query = url.query_pairs_mut();
            for relay in &self.relays {
                query.append_pair("relay", relay.as_str());
            }
            query.append_pair("secret", self.secret.as_str());
            if !self.permissions.is_empty() {
                query.append_pair("perms", &self.permissions);
            }
            if let Some(name) = &self.metadata.name {
                query.append_pair("name", name);
            }
            if let Some(value) = &self.metadata.url {
                query.append_pair("url", value);
            }
            if let Some(image) = &self.metadata.image {
                query.append_pair("image", image);
            }
        }
        Ok(url.into())
    }

    /// Wait for a signer-launched connect response and finish the handshake.
    pub fn connect(self, timeout: Duration) -> Result<Nip46Signer, Nip46Error> {
        self.connect_observed(timeout, Arc::new(|_| {}))
    }

    pub fn connect_observed(
        self,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
    ) -> Result<Nip46Signer, Nip46Error> {
        self.connect_observed_inner(timeout, event_sink, None)
    }

    pub fn connect_observed_with_cancellation(
        self,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
    ) -> Result<Nip46Signer, Nip46Error> {
        self.connect_observed_inner(timeout, event_sink, Some(cancellation))
    }

    fn connect_observed_inner(
        self,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: Option<&Nip46Cancellation>,
    ) -> Result<Nip46Signer, Nip46Error> {
        let session = Session::spawn(self.relays, self.client_keys, None, cancellation);
        forward_events(&session, event_sink);
        session.wait_available(timeout)?;
        let remote_signer_public_key = session
            .accept_invitation(self.secret.as_str())
            .recv_timeout(timeout)
            .map_err(map_connect_recv)??;
        let user_public_key = request_string(&session, "get_public_key", Vec::new())
            .wait(timeout)
            .map_err(Nip46Error::from)
            .and_then(|value| {
                PublicKey::from_hex(&value).map_err(|error| {
                    Nip46Error::InvalidResponse(format!("get_public_key: {error}"))
                })
            })?;
        session.emit(Nip46ConnectionEvent::Connected { user_public_key });
        session.request_switch_relays();
        Ok(Nip46Signer {
            user_public_key,
            remote_signer_public_key,
            session,
        })
    }
}

/// Fully connected remote signer. Clones share the one independent NIP-46
/// session and can be reattached to the engine after availability changes.
#[derive(Clone)]
pub struct Nip46Signer {
    user_public_key: PublicKey,
    remote_signer_public_key: PublicKey,
    session: Arc<Session>,
}

impl fmt::Debug for Nip46Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Nip46Signer")
            .field("user_public_key", &self.user_public_key)
            .field("remote_signer_public_key", &self.remote_signer_public_key)
            .field("available", &self.session.is_available())
            .finish_non_exhaustive()
    }
}

impl Nip46Signer {
    /// Remote-signer initiated connection (`bunker://`).
    pub fn connect_bunker(uri: &str, timeout: Duration) -> Result<Self, Nip46Error> {
        Self::connect_bunker_observed(
            uri,
            Some(DEFAULT_PERMISSIONS.to_string()),
            Nip46ClientMetadata::default(),
            timeout,
            Arc::new(|_| {}),
        )
    }

    pub fn connect_bunker_with(
        uri: &str,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
        timeout: Duration,
    ) -> Result<Self, Nip46Error> {
        Self::connect_bunker_observed(uri, permissions, metadata, timeout, Arc::new(|_| {}))
    }

    pub fn connect_bunker_observed(
        uri: &str,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
    ) -> Result<Self, Nip46Error> {
        Self::connect_bunker_observed_inner(uri, permissions, metadata, timeout, event_sink, None)
    }

    pub fn connect_bunker_observed_with_cancellation(
        uri: &str,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
    ) -> Result<Self, Nip46Error> {
        Self::connect_bunker_observed_inner(
            uri,
            permissions,
            metadata,
            timeout,
            event_sink,
            Some(cancellation),
        )
    }

    fn connect_bunker_observed_inner(
        uri: &str,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: Option<&Nip46Cancellation>,
    ) -> Result<Self, Nip46Error> {
        let parsed = parse_bunker_uri(uri).map_err(Nip46Error::InvalidBunkerUri)?;
        let remote_signer_public_key = parsed.remote_signer_public_key;
        let session = Session::spawn(
            parsed.relays,
            Keys::generate(),
            Some(remote_signer_public_key),
            cancellation,
        );
        forward_events(&session, event_sink);
        session.wait_available(timeout)?;

        let metadata_json = serde_json::to_string(&metadata)
            .map_err(|error| Nip46Error::InvalidResponse(error.to_string()))?;
        let params = vec![
            remote_signer_public_key.to_hex(),
            parsed
                .secret
                .as_deref()
                .map(String::as_str)
                .unwrap_or_default()
                .to_string(),
            permissions.unwrap_or_default(),
            metadata_json,
        ];
        let connect_result = request_string(&session, "connect", params)
            .wait(timeout)
            .map_err(Nip46Error::from)?;
        if connect_result != "ack"
            && parsed.secret.as_deref().map(String::as_str) != Some(connect_result.as_str())
        {
            return Err(Nip46Error::InvalidResponse(format!(
                "connect returned {connect_result:?}"
            )));
        }

        let user_public_key = request_string(&session, "get_public_key", Vec::new())
            .wait(timeout)
            .map_err(Nip46Error::from)
            .and_then(|value| {
                PublicKey::from_hex(&value).map_err(|error| {
                    Nip46Error::InvalidResponse(format!("get_public_key: {error}"))
                })
            })?;
        session.emit(Nip46ConnectionEvent::Connected { user_public_key });
        session.request_switch_relays();
        Ok(Self {
            user_public_key,
            remote_signer_public_key,
            session,
        })
    }

    #[must_use]
    pub fn user_public_key(&self) -> PublicKey {
        self.user_public_key
    }

    #[must_use]
    pub fn remote_signer_public_key(&self) -> PublicKey {
        self.remote_signer_public_key
    }

    #[must_use]
    pub fn is_available(&self) -> bool {
        self.session.is_available()
    }

    /// Event-driven availability/auth projection. No connection credentials
    /// cross this channel.
    pub fn subscribe_connection_events(&self) -> Receiver<Nip46ConnectionEvent> {
        self.session.subscribe()
    }

    pub fn logout(&self) -> SignerOp<()> {
        map_string(
            request_string(&self.session, "logout", Vec::new()),
            |result| {
                (result == "ack").then_some(()).ok_or_else(|| {
                    SignerError::InvalidResponse(format!("logout returned {result:?}"))
                })
            },
        )
    }
}

impl SigningCapability for Nip46Signer {
    fn public_key(&self) -> Option<PublicKey> {
        Some(self.user_public_key)
    }

    fn is_available(&self) -> bool {
        self.session.is_available()
    }

    fn sign(&self, unsigned: UnsignedEvent) -> SignerOp<Event> {
        let expected = unsigned.clone();
        let body = serde_json::json!({
            "kind": unsigned.kind.as_u16(),
            "created_at": unsigned.created_at.as_secs(),
            "tags": unsigned.tags,
            "content": unsigned.content,
        })
        .to_string();
        let user_public_key = self.user_public_key;
        map_string(
            request_string(&self.session, "sign_event", vec![body]),
            move |result| {
                let event = Event::from_json(&result).map_err(|error| {
                    SignerError::InvalidResponse(format!("sign_event is not an event: {error}"))
                })?;
                event.verify().map_err(|error| {
                    SignerError::InvalidResponse(format!("sign_event verification failed: {error}"))
                })?;
                if event.pubkey != user_public_key {
                    return Err(SignerError::InvalidResponse(format!(
                        "sign_event author {} does not match {}",
                        event.pubkey, user_public_key
                    )));
                }
                // The engine repeats this check at the promotion boundary;
                // rejecting here also gives direct users a typed failure.
                if event.created_at != expected.created_at
                    || event.kind != expected.kind
                    || event.tags != expected.tags
                    || event.content != expected.content
                {
                    return Err(SignerError::InvalidResponse(
                        "sign_event mutated the frozen template".to_string(),
                    ));
                }
                Ok(event)
            },
        )
    }
}

impl CryptoCapability for Nip46Signer {
    fn nip44_encrypt(&self, peer: PublicKey, plaintext: &str) -> SignerOp<String> {
        request_string(
            &self.session,
            "nip44_encrypt",
            vec![peer.to_hex(), plaintext.to_string()],
        )
    }

    fn nip44_decrypt(&self, peer: PublicKey, ciphertext: &str) -> SignerOp<String> {
        request_string(
            &self.session,
            "nip44_decrypt",
            vec![peer.to_hex(), ciphertext.to_string()],
        )
    }
}

#[derive(Serialize)]
struct RpcRequest<'a> {
    id: &'a str,
    method: &'a str,
    params: &'a [String],
}

#[derive(Deserialize)]
struct RpcEnvelope {
    id: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Vec<String>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

struct PendingRequest {
    frame: String,
    reply: Sender<Result<String, SignerError>>,
}

enum WorkerMsg {
    Pool(PoolEvent),
    Request {
        id: String,
        method: String,
        params: Vec<String>,
        reply: Sender<Result<String, SignerError>>,
    },
    AcceptInvitation {
        expected_secret: String,
        reply: Sender<Result<PublicKey, Nip46Error>>,
    },
    ReplaceRelays(Vec<RelayUrl>),
    CancelRequest(String),
    Shutdown,
}

#[derive(Clone)]
struct SessionPoolSink(Sender<WorkerMsg>);

impl PoolEventSink for SessionPoolSink {
    fn on_event(&self, event: PoolEvent) {
        let _ = self.0.send(WorkerMsg::Pool(event));
    }
}

struct Session {
    commands: Sender<WorkerMsg>,
    connected_relays: AtomicUsize,
    subscribers: Arc<Mutex<Vec<Sender<Nip46ConnectionEvent>>>>,
}

impl Session {
    fn spawn(
        relays: Vec<RelayUrl>,
        client_keys: Keys,
        remote: Option<PublicKey>,
        cancellation: Option<&Nip46Cancellation>,
    ) -> Arc<Self> {
        let (commands, inbox) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let session = Arc::new(Self {
            commands: commands.clone(),
            connected_relays: AtomicUsize::new(0),
            subscribers: Arc::clone(&subscribers),
        });
        if let Some(cancellation) = cancellation {
            cancellation.bind(session.commands.clone());
        }
        let weak = Arc::downgrade(&session);
        thread::Builder::new()
            .name("nmp-nip46".to_string())
            .spawn(move || {
                let pool = Pool::new(PoolConfig::default(), SessionPoolSink(commands));
                let mut worker = SessionWorker::new(pool, client_keys, remote, weak, subscribers);
                worker.open_relays(relays);
                worker.emit(Nip46ConnectionEvent::Connecting);
                worker.run(inbox);
            })
            .expect("spawn NIP-46 session worker");
        session
    }

    fn is_available(&self) -> bool {
        self.connected_relays.load(Ordering::Acquire) > 0
    }

    fn subscribe(&self) -> Receiver<Nip46ConnectionEvent> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(tx);
        }
        rx
    }

    fn emit(&self, event: Nip46ConnectionEvent) {
        emit_to(&self.subscribers, event);
    }

    fn wait_available(&self, timeout: Duration) -> Result<(), Nip46Error> {
        if self.is_available() {
            return Ok(());
        }
        let events = self.subscribe();
        if self.is_available() {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Nip46Error::Timeout);
            }
            match events.recv_timeout(remaining) {
                Ok(Nip46ConnectionEvent::Available) => return Ok(()),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return Err(Nip46Error::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(Nip46Error::Disconnected),
            }
        }
    }

    fn accept_invitation(&self, expected_secret: &str) -> Receiver<Result<PublicKey, Nip46Error>> {
        let (tx, rx) = mpsc::channel();
        if self
            .commands
            .send(WorkerMsg::AcceptInvitation {
                expected_secret: expected_secret.to_string(),
                reply: tx.clone(),
            })
            .is_err()
        {
            let _ = tx.send(Err(Nip46Error::Disconnected));
        }
        rx
    }

    fn request_switch_relays(self: &Arc<Self>) {
        let op = request_string(self, "switch_relays", Vec::new());
        let session = Arc::downgrade(self);
        thread::spawn(move || {
            let Ok(result) = op.wait(SWITCH_RELAYS_TIMEOUT) else {
                return;
            };
            if result == "null" {
                return;
            }
            let Ok(relays) = serde_json::from_str::<Vec<String>>(&result) else {
                return;
            };
            let mut parsed = Vec::new();
            for relay in relays {
                let Ok(relay) = RelayUrl::parse(&relay) else {
                    return;
                };
                if !parsed.contains(&relay) {
                    parsed.push(relay);
                    if parsed.len() > MAX_NIP46_RELAYS {
                        return;
                    }
                }
            }
            if !parsed.is_empty() {
                let Some(session) = session.upgrade() else {
                    return;
                };
                let _ = session.commands.send(WorkerMsg::ReplaceRelays(parsed));
            }
        });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.commands.send(WorkerMsg::Shutdown);
    }
}

struct SessionWorker {
    pool: Pool,
    client_keys: Keys,
    remote: Option<PublicKey>,
    session: std::sync::Weak<Session>,
    subscribers: Arc<Mutex<Vec<Sender<Nip46ConnectionEvent>>>>,
    handles: HashMap<u32, (nmp_transport::RelayHandle, RelayUrl)>,
    configured: HashMap<RelayUrl, nmp_transport::RelayHandle>,
    pending: HashMap<String, PendingRequest>,
    invitation: Option<(String, Sender<Result<PublicKey, Nip46Error>>)>,
    subscription_id: SubscriptionId,
}

impl SessionWorker {
    fn new(
        pool: Pool,
        client_keys: Keys,
        remote: Option<PublicKey>,
        session: std::sync::Weak<Session>,
        subscribers: Arc<Mutex<Vec<Sender<Nip46ConnectionEvent>>>>,
    ) -> Self {
        static NEXT_SUB: AtomicU64 = AtomicU64::new(1);
        Self {
            pool,
            client_keys,
            remote,
            session,
            subscribers,
            handles: HashMap::new(),
            configured: HashMap::new(),
            pending: HashMap::new(),
            invitation: None,
            subscription_id: SubscriptionId::new(format!(
                "nmp-nip46-{}",
                NEXT_SUB.fetch_add(1, Ordering::Relaxed)
            )),
        }
    }

    fn run(&mut self, inbox: Receiver<WorkerMsg>) {
        while let Ok(message) = inbox.recv() {
            match message {
                WorkerMsg::Pool(event) => self.on_pool(event),
                WorkerMsg::Request {
                    id,
                    method,
                    params,
                    reply,
                } => self.on_request(id, method, params, reply),
                WorkerMsg::AcceptInvitation {
                    expected_secret,
                    reply,
                } => self.invitation = Some((expected_secret, reply)),
                WorkerMsg::ReplaceRelays(relays) => self.replace_relays(relays),
                WorkerMsg::CancelRequest(id) => {
                    self.pending.remove(&id);
                }
                WorkerMsg::Shutdown => break,
            }
        }
        for (_, pending) in self.pending.drain() {
            let _ = pending.reply.send(Err(SignerError::Disconnected));
        }
        if let Some((_, reply)) = self.invitation.take() {
            let _ = reply.send(Err(Nip46Error::Disconnected));
        }
        self.subscribers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        self.pool.shutdown();
    }

    fn emit(&self, event: Nip46ConnectionEvent) {
        emit_to(&self.subscribers, event);
    }

    fn open_relays(&mut self, relays: Vec<RelayUrl>) {
        for relay in relays {
            if self.configured.contains_key(&relay) {
                continue;
            }
            let Ok(handle) = self.pool.ensure_open(&relay) else {
                continue;
            };
            self.set_preamble(handle);
            self.configured.insert(relay, handle);
        }
    }

    fn replace_relays(&mut self, relays: Vec<RelayUrl>) {
        let old = std::mem::take(&mut self.configured);
        self.open_relays(relays);
        for (url, handle) in old {
            if !self.configured.contains_key(&url) {
                let _ = self.pool.close(handle);
            }
        }
    }

    fn filter(&self) -> Filter {
        let filter = Filter::new()
            .kind(Kind::NostrConnect)
            .pubkey(self.client_keys.public_key());
        match self.remote {
            Some(remote) => filter.author(remote),
            None => filter,
        }
    }

    fn preamble(&self) -> String {
        ClientMessage::req(self.subscription_id.clone(), vec![self.filter()]).as_json()
    }

    fn set_preamble(&self, handle: nmp_transport::RelayHandle) {
        let _ = self
            .pool
            .set_reconnect_preamble(handle, vec![self.preamble()]);
    }

    fn refresh_preambles(&self) {
        for (handle, _) in self.handles.values() {
            self.set_preamble(*handle);
        }
    }

    fn on_pool(&mut self, event: PoolEvent) {
        match event {
            PoolEvent::Connected { handle, url } => {
                if self
                    .handles
                    .get(&handle.slot)
                    .is_some_and(|(current, _)| current.generation > handle.generation)
                {
                    return;
                }
                let was_empty = self.handles.is_empty();
                self.handles.insert(handle.slot, (handle, url));
                if let Some(session) = self.session.upgrade() {
                    session
                        .connected_relays
                        .store(self.handles.len(), Ordering::Release);
                }
                self.set_preamble(handle);
                let _ = self.pool.send(handle, WireFrame::Text(self.preamble()));
                for pending in self.pending.values() {
                    let _ = self
                        .pool
                        .send(handle, WireFrame::Text(pending.frame.clone()));
                }
                if was_empty {
                    self.emit(Nip46ConnectionEvent::Available);
                }
            }
            PoolEvent::Disconnected { handle, .. } => {
                if !self
                    .handles
                    .get(&handle.slot)
                    .is_some_and(|(current, _)| *current == handle)
                {
                    return;
                }
                self.handles.remove(&handle.slot);
                if let Some(session) = self.session.upgrade() {
                    session
                        .connected_relays
                        .store(self.handles.len(), Ordering::Release);
                }
                if self.handles.is_empty() {
                    for (_, pending) in self.pending.drain() {
                        let _ = pending.reply.send(Err(SignerError::Disconnected));
                    }
                    self.emit(Nip46ConnectionEvent::Unavailable);
                }
            }
            PoolEvent::Frame { handle, frame } => {
                if self
                    .handles
                    .get(&handle.slot)
                    .is_some_and(|(current, _)| *current == handle)
                {
                    self.on_frame(handle, frame);
                }
            }
            PoolEvent::Health { .. } | PoolEvent::EventHandoff { .. } => {}
        }
    }

    fn on_frame(&mut self, handle: nmp_transport::RelayHandle, frame: RelayFrame) {
        match frame {
            RelayFrame::Event { event, .. } => self.on_event(event.as_ref()),
            RelayFrame::Message(message) => {
                if let RelayMessage::Auth { challenge } = message.as_ref() {
                    let relay = self.handles.get(&handle.slot).map(|(_, url)| url.clone());
                    if let Some(relay) = relay {
                        if let Ok(event) = EventBuilder::auth(challenge.as_ref(), relay.clone())
                            .sign_with_keys(&self.client_keys)
                        {
                            let _ = self.pool.send(
                                handle,
                                WireFrame::Text(ClientMessage::auth(event).as_json()),
                            );
                            self.emit(Nip46ConnectionEvent::RelayAuthentication(relay));
                        }
                    }
                }
            }
        }
    }

    fn on_event(&mut self, event: &Event) {
        if event.kind != Kind::NostrConnect
            || !event
                .tags
                .public_keys()
                .any(|pk| *pk == self.client_keys.public_key())
            || self.remote.is_some_and(|remote| event.pubkey != remote)
        {
            return;
        }
        let Ok(plaintext) = nip44::decrypt(
            self.client_keys.secret_key(),
            &event.pubkey,
            event.content.as_bytes(),
        ) else {
            return;
        };
        let Ok(envelope) = serde_json::from_str::<RpcEnvelope>(&plaintext) else {
            return;
        };

        if self.remote.is_none() {
            let Some((expected_secret, _)) = self.invitation.as_ref() else {
                return;
            };
            let current_result = envelope
                .result
                .as_ref()
                .and_then(Value::as_str)
                .map(str::to_string);
            // Current NIP-46 sends a connect response whose result is the
            // invitation secret. Accept the older request-shaped form too so
            // existing signers can migrate without weakening the secret gate.
            let legacy_secret = (envelope.method.as_deref() == Some("connect"))
                .then(|| envelope.params.get(1).cloned())
                .flatten();
            if current_result.as_deref() != Some(expected_secret.as_str())
                && legacy_secret.as_deref() != Some(expected_secret.as_str())
            {
                // A forged p-tagged response must not consume the one-shot
                // invitation and turn the anti-spoofing secret into a DoS.
                return;
            }
            let (_, reply) = self
                .invitation
                .take()
                .expect("invitation was just validated");
            self.remote = Some(event.pubkey);
            self.refresh_preambles();
            let _ = reply.send(Ok(event.pubkey));
            return;
        }

        if !self.pending.contains_key(&envelope.id) {
            return;
        }
        if envelope.result.as_ref().and_then(Value::as_str) == Some("auth_url") {
            if let Some(url) = envelope.error {
                self.emit(Nip46ConnectionEvent::AuthorizationRequired(url));
            }
            return;
        }
        let pending = self
            .pending
            .remove(&envelope.id)
            .expect("pending entry was just observed");
        if let Some(error) = envelope.error {
            let _ = pending.reply.send(Err(SignerError::Rejected(error)));
            return;
        }
        let result = match envelope.result {
            Some(Value::String(value)) => Ok(value),
            Some(_) => Err(SignerError::InvalidResponse(
                "response result is not a string".to_string(),
            )),
            None => Err(SignerError::InvalidResponse(
                "response contains neither result nor error".to_string(),
            )),
        };
        let _ = pending.reply.send(result);
    }

    fn on_request(
        &mut self,
        id: String,
        method: String,
        params: Vec<String>,
        reply: Sender<Result<String, SignerError>>,
    ) {
        if self.handles.is_empty() {
            let _ = reply.send(Err(SignerError::Unavailable));
            return;
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            let _ = reply.send(Err(SignerError::Unavailable));
            return;
        }
        let Some(remote) = self.remote else {
            let _ = reply.send(Err(SignerError::Unavailable));
            return;
        };
        let request = RpcRequest {
            id: &id,
            method: &method,
            params: &params,
        };
        let Ok(plaintext) = serde_json::to_string(&request) else {
            let _ = reply.send(Err(SignerError::InvalidResponse(
                "could not encode request".to_string(),
            )));
            return;
        };
        let Ok(ciphertext) = nip44::encrypt(
            self.client_keys.secret_key(),
            &remote,
            plaintext,
            nip44::Version::default(),
        ) else {
            let _ = reply.send(Err(SignerError::Unavailable));
            return;
        };
        let Ok(event) = EventBuilder::new(Kind::NostrConnect, ciphertext)
            .tag(Tag::public_key(remote))
            .sign_with_keys(&self.client_keys)
        else {
            let _ = reply.send(Err(SignerError::Unavailable));
            return;
        };
        let frame = ClientMessage::event(event).as_json();
        self.pending.insert(
            id.clone(),
            PendingRequest {
                frame: frame.clone(),
                reply,
            },
        );
        for (handle, _) in self.handles.values() {
            let _ = self.pool.send(*handle, WireFrame::Text(frame.clone()));
        }
    }
}

fn request_string(session: &Arc<Session>, method: &str, params: Vec<String>) -> SignerOp<String> {
    let id = Keys::generate().public_key().to_hex();
    let (tx, rx) = mpsc::channel();
    if session
        .commands
        .send(WorkerMsg::Request {
            id: id.clone(),
            method: method.to_string(),
            params,
            reply: tx,
        })
        .is_err()
    {
        return SignerOp::err(SignerError::Disconnected);
    }
    let commands = session.commands.clone();
    SignerOp::pending_with_cancel(rx, move || {
        let _ = commands.send(WorkerMsg::CancelRequest(id));
    })
}

fn map_string<T, F>(op: SignerOp<String>, map: F) -> SignerOp<T>
where
    T: Send + 'static,
    F: FnOnce(String) -> Result<T, SignerError> + Send + 'static,
{
    match op {
        SignerOp::Ready(Ok(value)) => SignerOp::Ready(map(value)),
        SignerOp::Ready(Err(error)) => SignerOp::Ready(Err(error)),
        SignerOp::Pending(pending) => {
            let (rx, cancel) = pending.into_parts();
            let (tx, mapped_rx) = mpsc::channel();
            thread::spawn(move || {
                let result = match rx.recv() {
                    Ok(Ok(value)) => map(value),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(SignerError::Disconnected),
                };
                let _ = tx.send(result);
            });
            SignerOp::pending_from_parts(mapped_rx, cancel)
        }
    }
}

fn map_connect_recv(error: RecvTimeoutError) -> Nip46Error {
    match error {
        RecvTimeoutError::Timeout => Nip46Error::Timeout,
        RecvTimeoutError::Disconnected => Nip46Error::Disconnected,
    }
}

fn emit_to(
    subscribers: &Arc<Mutex<Vec<Sender<Nip46ConnectionEvent>>>>,
    event: Nip46ConnectionEvent,
) {
    if let Ok(mut subscribers) = subscribers.lock() {
        subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }
}

fn forward_events(
    session: &Arc<Session>,
    event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
) {
    let events = session.subscribe();
    thread::spawn(move || {
        while let Ok(event) = events.recv() {
            event_sink(event);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_auth_frame_cannot_mutate_reopened_nip46_generation() {
        let (pool_tx, _pool_rx) = mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx);
        let (event_tx, event_rx) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(vec![event_tx]));
        let mut worker = SessionWorker::new(
            pool,
            Keys::generate(),
            None,
            std::sync::Weak::new(),
            Arc::clone(&subscribers),
        );
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let old = nmp_transport::RelayHandle {
            slot: 0,
            generation: 1,
        };
        let reopened = nmp_transport::RelayHandle {
            slot: 0,
            generation: 2,
        };
        worker.handles.insert(0, (reopened, relay.clone()));

        let auth_frame = || {
            RelayFrame::from_message(RelayMessage::Auth {
                challenge: "generation-bound-auth".into(),
            })
        };
        worker.on_pool(PoolEvent::Frame {
            handle: old,
            frame: auth_frame(),
        });
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_millis(20)),
            Err(RecvTimeoutError::Timeout)
        ));

        worker.on_pool(PoolEvent::Frame {
            handle: reopened,
            frame: auth_frame(),
        });
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Nip46ConnectionEvent::RelayAuthentication(relay)
        );
        worker.pool.shutdown();
    }

    #[test]
    fn invitation_encodes_every_relay_secret_permissions_and_metadata() {
        let invitation = Nip46Invitation::new(
            vec![
                RelayUrl::parse("wss://one.example").unwrap(),
                RelayUrl::parse("wss://two.example").unwrap(),
            ],
            Some("sign_event:1,nip44_decrypt".to_string()),
            Nip46ClientMetadata {
                name: Some("NMP App".to_string()),
                url: Some("https://example.com".to_string()),
                image: None,
            },
        )
        .unwrap();
        let uri = invitation.uri();
        let parsed = url::Url::parse(&uri).unwrap();
        assert_eq!(parsed.scheme(), "nostrconnect");
        assert_eq!(parsed.host_str().unwrap().len(), 64);
        let pairs = parsed.query_pairs().collect::<Vec<_>>();
        assert_eq!(pairs.iter().filter(|(key, _)| key == "relay").count(), 2);
        assert!(pairs
            .iter()
            .any(|(key, value)| key == "secret" && !value.is_empty()));
        assert!(pairs
            .iter()
            .any(|(key, value)| { key == "perms" && value == "sign_event:1,nip44_decrypt" }));
        assert!(pairs
            .iter()
            .any(|(key, value)| key == "name" && value == "NMP App"));
        assert!(format!("{invitation:?}").contains("[redacted]"));
    }

    #[test]
    fn app_specific_scheme_changes_only_the_handoff_scheme() {
        let invitation = Nip46Invitation::new(
            vec![RelayUrl::parse("wss://relay.example").unwrap()],
            None,
            Nip46ClientMetadata::default(),
        )
        .unwrap();
        let generic = invitation.uri();
        let primal = invitation.uri_with_scheme("primalconnect").unwrap();
        assert_eq!(
            generic.strip_prefix("nostrconnect"),
            primal.strip_prefix("primalconnect")
        );
    }

    #[test]
    fn invalid_app_scheme_is_a_typed_error_not_a_panic() {
        let invitation = Nip46Invitation::new(
            vec![RelayUrl::parse("wss://relay.example").unwrap()],
            None,
            Nip46ClientMetadata::default(),
        )
        .unwrap();
        assert!(matches!(
            invitation.uri_with_scheme("not a scheme"),
            Err(Nip46Error::InvalidLaunchScheme(_))
        ));
    }

    #[test]
    fn invitation_relay_fanout_is_bounded() {
        let relays = (0..=MAX_NIP46_RELAYS)
            .map(|i| RelayUrl::parse(&format!("wss://relay-{i}.example")).unwrap())
            .collect();
        assert!(matches!(
            Nip46Invitation::new(relays, None, Nip46ClientMetadata::default()),
            Err(Nip46Error::TooManyRelays(_))
        ));
    }
}
