//! Current NIP-46 client adapter.
//!
//! The adapter owns an independent relay pool and exactly-correlated remote
//! RPCs. It deliberately does not own NMP's durable write retry/publication.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nmp_transport::{
    Pool, PoolConfig, PoolEvent, PoolEventSink, RelayFrame, RelayOpenError, WireFrame,
};
use nostr::nips::nip44;
use nostr::{
    ClientMessage, Event, EventBuilder, Filter, JsonUtil, Keys, Kind, PublicKey, RelayMessage,
    RelayUrl, SubscriptionId, Tag, UnsignedEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::{
    parse_bunker_uri, pending_signer_cancellation, BunkerParseError, CryptoCapability,
    PendingSignerSender, SignerError, SignerOp, SigningCapability, MAX_BUNKER_URI_LEN,
    MAX_NIP46_RELAYS,
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

// #494: `InvalidRelay`, `InvalidInvitation`, and `SecretMismatch` were
// removed here -- a repo-wide grep found zero construction sites for any of
// the three (only their own `Display` arms referenced them), so the FFI
// projection had no reachable variant to mirror. `InvalidLaunchScheme`
// stays: `Nip46Invitation::uri_with_scheme` constructs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nip46Error {
    InvalidBunkerUri(BunkerParseError),
    MissingRelay,
    TooManyRelays(usize),
    InvitationTooLong(usize),
    InvalidLaunchScheme(String),
    Timeout,
    Disconnected,
    Rejected(String),
    InvalidResponse(String),
    ThreadUnavailable { component: String, reason: String },
    ExecutorSaturated { component: String, capacity: usize },
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
            Self::InvalidLaunchScheme(scheme) => {
                write!(f, "invalid NIP-46 launch scheme: {scheme}")
            }
            Self::Timeout => f.write_str("NIP-46 connection timed out"),
            Self::Disconnected => f.write_str("NIP-46 connection ended"),
            Self::Rejected(reason) => write!(f, "NIP-46 signer rejected request: {reason}"),
            Self::InvalidResponse(reason) => write!(f, "invalid NIP-46 response: {reason}"),
            Self::ThreadUnavailable { component, reason } => {
                write!(f, "{component} thread unavailable: {reason}")
            }
            Self::ExecutorSaturated {
                component,
                capacity,
            } => write!(
                f,
                "{component} refused: native task executor is at capacity {capacity}"
            ),
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
        let executor =
            nmp_executor::Executor::new(nmp_executor::DEFAULT_MAX_TASKS).map_err(|error| {
                Nip46Error::ThreadUnavailable {
                    component: "NIP-46 native task executor".to_string(),
                    reason: error.to_string(),
                }
            })?;
        self.connect_observed_inner_with_executor(
            timeout,
            event_sink,
            cancellation,
            SessionExecutor::Owned(executor),
        )
    }

    #[doc(hidden)]
    pub fn connect_observed_with_executor_and_cancellation(
        self,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
        executor: nmp_executor::Executor,
    ) -> Result<Nip46Signer, Nip46Error> {
        self.connect_observed_inner_with_executor(
            timeout,
            event_sink,
            Some(cancellation),
            SessionExecutor::Shared(executor),
        )
    }

    fn connect_observed_inner_with_executor(
        self,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: Option<&Nip46Cancellation>,
        executor: SessionExecutor,
    ) -> Result<Nip46Signer, Nip46Error> {
        let session = Session::spawn(self.relays, self.client_keys, None, cancellation, executor)?;
        forward_events(&session, event_sink)?;
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
        let executor =
            nmp_executor::Executor::new(nmp_executor::DEFAULT_MAX_TASKS).map_err(|error| {
                Nip46Error::ThreadUnavailable {
                    component: "NIP-46 native task executor".to_string(),
                    reason: error.to_string(),
                }
            })?;
        Self::connect_bunker_observed_inner_with_executor(
            uri,
            permissions,
            metadata,
            timeout,
            event_sink,
            cancellation,
            SessionExecutor::Owned(executor),
        )
    }

    #[doc(hidden)]
    pub fn connect_bunker_observed_with_executor_and_cancellation(
        uri: &str,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
        executor: nmp_executor::Executor,
    ) -> Result<Self, Nip46Error> {
        Self::connect_bunker_observed_inner_with_executor(
            uri,
            permissions,
            metadata,
            timeout,
            event_sink,
            Some(cancellation),
            SessionExecutor::Shared(executor),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn connect_bunker_observed_inner_with_executor(
        uri: &str,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: Option<&Nip46Cancellation>,
        executor: SessionExecutor,
    ) -> Result<Self, Nip46Error> {
        let parsed = parse_bunker_uri(uri).map_err(Nip46Error::InvalidBunkerUri)?;
        let remote_signer_public_key = parsed.remote_signer_public_key;
        let session = Session::spawn(
            parsed.relays,
            Keys::generate(),
            Some(remote_signer_public_key),
            cancellation,
            executor,
        )?;
        forward_events(&session, event_sink)?;
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
            self.session.executor(),
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
            self.session.executor(),
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
    reply: PendingSignerSender<String>,
}

enum WorkerMsg {
    Pool(PoolEvent),
    Request {
        id: String,
        method: String,
        params: Vec<String>,
        reply: PendingSignerSender<String>,
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

fn session_pool_config() -> PoolConfig {
    // Keep the owned transport pool's worker envelope equal to the same
    // protocol/session relay ceiling enforced at every NIP-46 input door.
    PoolConfig {
        max_relays: MAX_NIP46_RELAYS,
        // Issue #519's resolved-IP admission check (`pool::connect`) refuses
        // a loopback/private/link-local dial by default — the right default
        // for a DISCOVERED relay (a network-sourced kind:10002/kind:10050
        // list an attacker could steer at an internal address). A NIP-46
        // bunker relay is never discovered that way: it is always the
        // explicit target of a `bunker://`/`nostrconnect://` URI the user
        // pasted or scanned, the same trust tier as an operator's own config
        // (see `nmp-engine::core::admission`'s doc for that provenance
        // split). Self-hosted bunker signers on loopback/LAN are a common,
        // legitimate setup, so this session pool keeps admitting the usual
        // local ranges rather than inheriting the network-discovery-only
        // refusal.
        allowed_local_hosts: Arc::new(BTreeSet::from([
            "127.0.0.1".to_string(),
            "::1".to_string(),
            "localhost".to_string(),
        ])),
        ..PoolConfig::default()
    }
}

/// Encodes whether a [`Session`] owns its executor (and must shut it down
/// when the session is torn down) or merely borrows a shared/engine-owned
/// executor (which outlives the session and must never be shut down by it).
///
/// This makes the illegal combination — "shared executor + shut it down on
/// drop" — unrepresentable: there is exactly one `Drop` behavior per variant.
enum SessionExecutor {
    /// A fresh executor created for and used exclusively by this session.
    /// Shut down when the session drops.
    Owned(nmp_executor::Executor),
    /// A caller/engine-owned executor shared across sessions. Never shut
    /// down by this session; the owner controls its lifetime.
    Shared(nmp_executor::Executor),
}

impl SessionExecutor {
    fn handle(&self) -> &nmp_executor::Executor {
        match self {
            SessionExecutor::Owned(executor) | SessionExecutor::Shared(executor) => executor,
        }
    }
}

struct Session {
    commands: Sender<WorkerMsg>,
    connected_relays: AtomicUsize,
    subscribers: Arc<Mutex<Vec<Sender<Nip46ConnectionEvent>>>>,
    availability_error: Arc<Mutex<Option<Nip46Error>>>,
    executor: SessionExecutor,
}

impl Session {
    fn spawn(
        relays: Vec<RelayUrl>,
        client_keys: Keys,
        remote: Option<PublicKey>,
        cancellation: Option<&Nip46Cancellation>,
        executor: SessionExecutor,
    ) -> Result<Arc<Self>, Nip46Error> {
        let (commands, inbox) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let availability_error = Arc::new(Mutex::new(None));
        let executor_handle = executor.handle().clone();
        let session = Arc::new(Self {
            commands: commands.clone(),
            connected_relays: AtomicUsize::new(0),
            subscribers: Arc::clone(&subscribers),
            availability_error: Arc::clone(&availability_error),
            executor,
        });
        if let Some(cancellation) = cancellation {
            cancellation.bind(session.commands.clone());
        }
        let weak = Arc::downgrade(&session);
        let pool = Pool::new(session_pool_config(), SessionPoolSink(commands.clone())).map_err(
            |error| Nip46Error::ThreadUnavailable {
                component: "NIP-46 transport".to_string(),
                reason: error.to_string(),
            },
        )?;
        let worker_pool = pool.clone();
        let cancel_commands = commands.clone();
        let spawn = executor_handle.spawn_with_cancel(
            "NIP-46 session",
            move || {
                let _ = cancel_commands.send(WorkerMsg::Shutdown);
            },
            move || {
                let mut worker = SessionWorker::new(
                    worker_pool,
                    client_keys,
                    remote,
                    weak,
                    subscribers,
                    availability_error,
                );
                worker.emit(Nip46ConnectionEvent::Connecting);
                if let Err(error) = worker.open_relays(relays) {
                    worker.record_availability_error(error);
                    worker.emit(Nip46ConnectionEvent::Unavailable);
                }
                worker.run(inbox);
            },
        );
        if let Err(error) = spawn {
            pool.shutdown();
            return Err(map_executor_error(error));
        }
        drop(pool);
        Ok(session)
    }

    /// Handle to the underlying executor, regardless of ownership. Callers
    /// that only need to spawn tasks (as opposed to deciding shutdown
    /// semantics) go through this accessor.
    fn executor(&self) -> &nmp_executor::Executor {
        self.executor.handle()
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
        if let Some(error) = self.availability_error() {
            return Err(error);
        }
        let events = self.subscribe();
        if self.is_available() {
            return Ok(());
        }
        if let Some(error) = self.availability_error() {
            return Err(error);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(error) = self.availability_error() {
                return Err(error);
            }
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

    fn availability_error(&self) -> Option<Nip46Error> {
        self.availability_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
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
        let cancel_commands = self.commands.clone();
        let _ = self.executor().spawn_with_cancel(
            "NIP-46 switch-relays",
            move || {
                let _ = cancel_commands.send(WorkerMsg::Shutdown);
            },
            move || {
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
            },
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.commands.send(WorkerMsg::Shutdown);
        self.subscribers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        if let SessionExecutor::Owned(executor) = &self.executor {
            executor.shutdown();
        }
    }
}

struct SessionWorker {
    pool: Pool,
    client_keys: Keys,
    remote: Option<PublicKey>,
    session: std::sync::Weak<Session>,
    subscribers: Arc<Mutex<Vec<Sender<Nip46ConnectionEvent>>>>,
    availability_error: Arc<Mutex<Option<Nip46Error>>>,
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
        availability_error: Arc<Mutex<Option<Nip46Error>>>,
    ) -> Self {
        static NEXT_SUB: AtomicU64 = AtomicU64::new(1);
        Self {
            pool,
            client_keys,
            remote,
            session,
            subscribers,
            availability_error,
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
            let _ = pending.reply.resolve(Err(SignerError::Disconnected));
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

    fn record_availability_error(&self, error: Nip46Error) {
        *self
            .availability_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(error);
    }

    fn open_relays(&mut self, relays: Vec<RelayUrl>) -> Result<(), Nip46Error> {
        self.open_relays_with(relays, |pool, relay| pool.ensure_open(relay))
    }

    fn open_relays_with(
        &mut self,
        relays: Vec<RelayUrl>,
        mut ensure_open: impl FnMut(
            &Pool,
            &RelayUrl,
        ) -> Result<nmp_transport::RelayHandle, RelayOpenError>,
    ) -> Result<(), Nip46Error> {
        let mut usable = 0usize;
        let mut thread_refusal = None;
        for relay in relays {
            if self.configured.contains_key(&relay) {
                usable += 1;
                continue;
            }
            let handle = match ensure_open(&self.pool, &relay) {
                Ok(handle) => handle,
                Err(RelayOpenError::ThreadUnavailable(error)) => {
                    thread_refusal.get_or_insert(Nip46Error::ThreadUnavailable {
                        component: error.role.to_string(),
                        reason: error.reason,
                    });
                    continue;
                }
                Err(_) => continue,
            };
            self.set_preamble(handle);
            self.configured.insert(relay, handle);
            usable += 1;
        }
        if usable == 0 {
            if let Some(error) = thread_refusal {
                return Err(error);
            }
        }
        Ok(())
    }

    fn replace_relays(&mut self, relays: Vec<RelayUrl>) {
        let retained = relays.clone();
        if let Err(error) = self.open_relays(relays) {
            self.record_availability_error(error);
            self.emit(Nip46ConnectionEvent::Unavailable);
            return;
        }
        self.availability_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        self.configured.retain(|url, handle| {
            if retained.contains(url) {
                true
            } else {
                let _ = self.pool.close(*handle);
                false
            }
        });
        if let Some(session) = self.session.upgrade() {
            session
                .connected_relays
                .store(self.handles.len(), Ordering::Release);
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
                self.availability_error
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take();
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
                        let _ = pending.reply.resolve(Err(SignerError::Disconnected));
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
            PoolEvent::Health { .. }
            | PoolEvent::EventHandoff { .. }
            | PoolEvent::WorkerRetired => {}
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
            let _ = pending.reply.resolve(Err(SignerError::Rejected(error)));
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
        let _ = pending.reply.resolve(result);
    }

    fn on_request(
        &mut self,
        id: String,
        method: String,
        params: Vec<String>,
        reply: PendingSignerSender<String>,
    ) {
        if self.handles.is_empty() {
            let _ = reply.resolve(Err(SignerError::Unavailable));
            return;
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            let _ = reply.resolve(Err(SignerError::Unavailable));
            return;
        }
        let Some(remote) = self.remote else {
            let _ = reply.resolve(Err(SignerError::Unavailable));
            return;
        };
        let request = RpcRequest {
            id: &id,
            method: &method,
            params: &params,
        };
        let Ok(plaintext) = serde_json::to_string(&request) else {
            let _ = reply.resolve(Err(SignerError::InvalidResponse(
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
            let _ = reply.resolve(Err(SignerError::Unavailable));
            return;
        };
        let Ok(event) = EventBuilder::new(Kind::NostrConnect, ciphertext)
            .tag(Tag::public_key(remote))
            .sign_with_keys(&self.client_keys)
        else {
            let _ = reply.resolve(Err(SignerError::Unavailable));
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
    let commands = session.commands.clone();
    let cancel_commands = commands.clone();
    let cancel_id = id.clone();
    let (reply, operation) = SignerOp::pending_channel_with_cancel(move || {
        let _ = cancel_commands.send(WorkerMsg::CancelRequest(cancel_id));
    });
    if session
        .commands
        .send(WorkerMsg::Request {
            id,
            method: method.to_string(),
            params,
            reply,
        })
        .is_err()
    {
        return SignerOp::err(SignerError::Disconnected);
    }
    operation
}

fn map_string<T, F>(executor: &nmp_executor::Executor, op: SignerOp<String>, map: F) -> SignerOp<T>
where
    T: Send + 'static,
    F: FnOnce(String) -> Result<T, SignerError> + Send + 'static,
{
    match op {
        SignerOp::Ready(Ok(value)) => SignerOp::Ready(map(value)),
        SignerOp::Ready(Err(error)) => SignerOp::Ready(Err(error)),
        SignerOp::Pending(pending) => {
            let (cancel, cancelled) = pending_signer_cancellation();
            let mapped_cancel = cancel.clone();
            let (completion, mapped) = SignerOp::pending_channel_with_cancel(move || {
                mapped_cancel.cancel();
            });
            let failure = completion.clone();
            let spawned = executor.spawn_with_cancel(
                "NIP-46 result-map",
                move || cancel.cancel(),
                move || {
                    let result = match pending.recv_or_cancel(cancelled) {
                        Some(Ok(value)) => map(value),
                        Some(Err(error)) => Err(error),
                        None => Err(SignerError::Disconnected),
                    };
                    let _ = completion.resolve(result);
                },
            );
            if spawned.is_err() {
                let _ = failure.resolve(Err(SignerError::Unavailable));
            }
            mapped
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
) -> Result<(), Nip46Error> {
    let events = session.subscribe();
    let cancel_commands = session.commands.clone();
    session
        .executor()
        .spawn_with_cancel(
            "NIP-46 event forwarder",
            move || {
                let _ = cancel_commands.send(WorkerMsg::Shutdown);
            },
            move || {
                while let Ok(event) = events.recv() {
                    event_sink(event);
                }
            },
        )
        .map_err(map_executor_error)
}

fn map_executor_error(error: nmp_executor::ExecutorError) -> Nip46Error {
    match error {
        nmp_executor::ExecutorError::Saturated(error) => Nip46Error::ExecutorSaturated {
            component: error.component,
            capacity: error.capacity,
        },
        nmp_executor::ExecutorError::Spawn(error) => Nip46Error::ThreadUnavailable {
            component: "NIP-46 native task".to_string(),
            reason: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_transport_worker_budget_equals_the_protocol_relay_ceiling() {
        assert_eq!(session_pool_config().max_relays, MAX_NIP46_RELAYS);
        assert_eq!(MAX_NIP46_RELAYS, 8);
    }

    #[test]
    fn injected_initial_relay_worker_refusal_reaches_the_waiting_caller_typed() {
        let (pool_tx, _pool_rx) = mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).expect("test pool construction");
        let (commands, _inbox) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let availability_error = Arc::new(Mutex::new(None));
        let session = Arc::new(Session {
            commands,
            connected_relays: AtomicUsize::new(0),
            subscribers: Arc::clone(&subscribers),
            availability_error: Arc::clone(&availability_error),
            executor: SessionExecutor::Owned(nmp_executor::Executor::new(4).unwrap()),
        });
        let mut worker = SessionWorker::new(
            pool,
            Keys::generate(),
            None,
            Arc::downgrade(&session),
            subscribers,
            availability_error,
        );
        let error = worker
            .open_relays_with(
                vec![RelayUrl::parse("wss://relay.example").unwrap()],
                |_, _| {
                    Err(RelayOpenError::ThreadUnavailable(
                        nmp_transport::ThreadSpawnError {
                            role: nmp_transport::ThreadRole::RelayWorker,
                            reason: "injected NIP-46 relay pressure".to_string(),
                        },
                    ))
                },
            )
            .unwrap_err();
        worker.record_availability_error(error.clone());
        assert_eq!(
            error,
            Nip46Error::ThreadUnavailable {
                component: "relay worker".to_string(),
                reason: "injected NIP-46 relay pressure".to_string(),
            }
        );
        assert_eq!(session.wait_available(Duration::from_secs(1)), Err(error));
        assert!(worker.configured.is_empty());
        worker.pool.shutdown();
    }

    #[test]
    fn borrowed_engine_executor_survives_session_teardown() {
        let executor = nmp_executor::Executor::new(2).unwrap();
        let session = Session::spawn(
            Vec::new(),
            Keys::generate(),
            None,
            None,
            SessionExecutor::Shared(executor.clone()),
        )
        .unwrap();

        drop(session);
        executor.wait_for_idle();
        assert!(executor.census().accepting);

        let reservation = executor.reserve("post-session engine work").unwrap();
        drop(reservation);
        executor.shutdown();
    }

    #[test]
    fn every_forwardable_engine_session_owns_two_slots() {
        let executor = nmp_executor::Executor::new(5).unwrap();
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let session = Session::spawn(
                Vec::new(),
                Keys::generate(),
                None,
                None,
                SessionExecutor::Shared(executor.clone()),
            )
            .unwrap();
            forward_events(&session, Arc::new(|_| {})).unwrap();
            sessions.push(session);
        }
        assert_eq!(executor.census().admitted, 4);

        let third = Session::spawn(
            Vec::new(),
            Keys::generate(),
            None,
            None,
            SessionExecutor::Shared(executor.clone()),
        )
        .unwrap();
        let refusal = forward_events(&third, Arc::new(|_| {})).unwrap_err();
        assert_eq!(
            refusal,
            Nip46Error::ExecutorSaturated {
                component: "NIP-46 event forwarder".to_string(),
                capacity: 5,
            }
        );

        drop(third);
        drop(sessions);
        executor.wait_for_idle();
        assert_eq!(executor.census().admitted, 0);
        executor.shutdown();
    }

    #[test]
    fn one_usable_initial_relay_keeps_a_later_spawn_refusal_nonterminal() {
        let (pool_tx, _pool_rx) = mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).expect("test pool construction");
        let mut worker = SessionWorker::new(
            pool,
            Keys::generate(),
            None,
            std::sync::Weak::new(),
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(None)),
        );
        let relays = vec![
            RelayUrl::parse("wss://one.example").unwrap(),
            RelayUrl::parse("wss://two.example").unwrap(),
        ];
        let mut attempt = 0usize;
        worker
            .open_relays_with(relays, |_, _| {
                attempt += 1;
                if attempt == 1 {
                    Ok(nmp_transport::RelayHandle {
                        slot: 99,
                        generation: 1,
                    })
                } else {
                    Err(RelayOpenError::ThreadUnavailable(
                        nmp_transport::ThreadSpawnError {
                            role: nmp_transport::ThreadRole::RelayWorker,
                            reason: "injected NIP-46 relay pressure".to_string(),
                        },
                    ))
                }
            })
            .expect("one usable requested relay keeps the session viable");
        assert_eq!(worker.configured.len(), 1);
        worker.pool.shutdown();
    }

    #[test]
    fn stale_auth_frame_cannot_mutate_reopened_nip46_generation() {
        let (pool_tx, _pool_rx) = mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).expect("test pool construction");
        let (event_tx, event_rx) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(vec![event_tx]));
        let mut worker = SessionWorker::new(
            pool,
            Keys::generate(),
            None,
            std::sync::Weak::new(),
            Arc::clone(&subscribers),
            Arc::new(Mutex::new(None)),
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
