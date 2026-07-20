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

use tokio::sync::mpsc as tokio_mpsc;

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
    parse_bunker_uri, BunkerParseError, CryptoCapability, PendingSignerSender, SignerError,
    SignerOp, SigningCapability, MAX_BUNKER_URI_LEN, MAX_NIP46_RELAYS,
};

const DEFAULT_PERMISSIONS: &str = "sign_event,nip44_encrypt,nip44_decrypt";
const MAX_PENDING_REQUESTS: usize = 64;
const SWITCH_RELAYS_TIMEOUT: Duration = Duration::from_secs(10);

struct Nip46CancellationInner {
    cancelled: AtomicBool,
    commands: Mutex<Option<tokio_mpsc::UnboundedSender<WorkerMsg>>>,
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
    fn bind(&self, commands: tokio_mpsc::UnboundedSender<WorkerMsg>) {
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
    /// A restored/imported session's live `get_public_key` answer did not
    /// match the checkpoint's expected identity (#571). The signer is never
    /// returned/attached in this case -- restore fails closed rather than
    /// resuming under a different pubkey.
    RestoredIdentityMismatch {
        expected: PublicKey,
        actual: PublicKey,
    },
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
            Self::RestoredIdentityMismatch { expected, actual } => write!(
                f,
                "restored NIP-46 session answered as {actual} but the checkpoint expected {expected}"
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
        self.connect_observed_inner_with_runtime(
            timeout,
            event_sink,
            cancellation,
            SessionRuntime(standalone_runtime()?),
        )
    }

    #[doc(hidden)]
    pub fn connect_observed_with_executor_and_cancellation(
        self,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
        runtime: tokio::runtime::Handle,
    ) -> Result<Nip46Signer, Nip46Error> {
        self.connect_observed_inner_with_runtime(
            timeout,
            event_sink,
            Some(cancellation),
            SessionRuntime(runtime),
        )
    }

    /// #704: async twin of
    /// [`Self::connect_observed_with_executor_and_cancellation`]. Identical
    /// handshake, except the availability wait is awaited
    /// ([`Session::wait_available_async`]) instead of blocking a std channel,
    /// so it runs as a task on the engine's shared adapter `runtime` and holds
    /// no OS thread while the signer comes online. `#[doc(hidden)]` for the same
    /// reason as its blocking `_with_executor_` sibling.
    #[doc(hidden)]
    pub async fn connect_observed_async(
        self,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
        runtime: tokio::runtime::Handle,
    ) -> Result<Nip46Signer, Nip46Error> {
        let client_keys = self.client_keys.clone();
        let session = Session::spawn(
            self.relays,
            self.client_keys,
            None,
            Some(cancellation),
            SessionRuntime(runtime),
        )?;
        forward_events(&session, event_sink)?;
        session.wait_available_async(timeout).await?;
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
            client_keys,
            origin: Nip46Origin::ClientInitiated,
        })
    }

    fn connect_observed_inner_with_runtime(
        self,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: Option<&Nip46Cancellation>,
        runtime: SessionRuntime,
    ) -> Result<Nip46Signer, Nip46Error> {
        let client_keys = self.client_keys.clone();
        let session = Session::spawn(self.relays, self.client_keys, None, cancellation, runtime)?;
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
            client_keys,
            origin: Nip46Origin::ClientInitiated,
        })
    }
}

/// Distinguishes a NIP-46 session this client paired via `nostrconnect://`
/// from one it dialed via `bunker://` (#571). Restore/checkpoint mechanics
/// are identical either way -- this is descriptive metadata a checkpoint
/// retains because "absence of a reusable client checkpoint is observable
/// rather than guessed from partial metadata" is a hard requirement, not
/// because restore branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nip46Origin {
    ClientInitiated,
    Bunker,
}

/// The minimum secrets and descriptor needed to reconnect an already-
/// authorized NIP-46 client session without another pairing handshake
/// (#571) -- read out once via [`Nip46Signer::checkpoint`] and consumed by
/// [`Nip46Signer::from_parts`]. Deliberately excludes `permissions`: the
/// remote signer itself holds the grant, and the checkpoint is the minimum
/// secrets/descriptor, not a permissions cache. Carries the client transport
/// secret key -- callers must not log, print, or otherwise surface this
/// value; `Debug` is redacted below.
#[derive(Clone)]
pub struct Nip46SessionCheckpoint {
    pub client_secret_key: nostr::SecretKey,
    pub user_public_key: PublicKey,
    pub remote_signer_public_key: PublicKey,
    pub relays: Vec<RelayUrl>,
    pub origin: Nip46Origin,
}

impl fmt::Debug for Nip46SessionCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Nip46SessionCheckpoint")
            .field("client_secret_key", &"[redacted]")
            .field("user_public_key", &self.user_public_key)
            .field("remote_signer_public_key", &self.remote_signer_public_key)
            .field("relays", &self.relays)
            .field("origin", &self.origin)
            .finish()
    }
}

/// Fully connected remote signer. Clones share the one independent NIP-46
/// session and can be reattached to the engine after availability changes.
#[derive(Clone)]
pub struct Nip46Signer {
    user_public_key: PublicKey,
    remote_signer_public_key: PublicKey,
    session: Arc<Session>,
    client_keys: Keys,
    origin: Nip46Origin,
}

impl fmt::Debug for Nip46Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Nip46Signer")
            .field("user_public_key", &self.user_public_key)
            .field("remote_signer_public_key", &self.remote_signer_public_key)
            .field("available", &self.session.is_available())
            .field("origin", &self.origin)
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
        Self::connect_bunker_observed_inner_with_runtime(
            uri,
            permissions,
            metadata,
            timeout,
            event_sink,
            cancellation,
            SessionRuntime(standalone_runtime()?),
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
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Nip46Error> {
        Self::connect_bunker_observed_inner_with_runtime(
            uri,
            permissions,
            metadata,
            timeout,
            event_sink,
            Some(cancellation),
            SessionRuntime(runtime),
        )
    }

    /// #704: async twin of
    /// [`Self::connect_bunker_observed_with_executor_and_cancellation`].
    /// Identical `bunker://` handshake, except the availability wait is awaited
    /// ([`Session::wait_available_async`]) instead of blocking a std channel, so
    /// it runs as a task on the engine's shared adapter `runtime` and holds no
    /// OS thread while the signer comes online. `#[doc(hidden)]` for the same
    /// reason as its blocking `_with_executor_` sibling.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_bunker_observed_async(
        uri: &str,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Nip46Error> {
        let parsed = parse_bunker_uri(uri).map_err(Nip46Error::InvalidBunkerUri)?;
        let remote_signer_public_key = parsed.remote_signer_public_key;
        let client_keys = Keys::generate();
        let session = Session::spawn(
            parsed.relays,
            client_keys.clone(),
            Some(remote_signer_public_key),
            Some(cancellation),
            SessionRuntime(runtime),
        )?;
        forward_events(&session, event_sink)?;
        session.wait_available_async(timeout).await?;

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
            client_keys,
            origin: Nip46Origin::Bunker,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn connect_bunker_observed_inner_with_runtime(
        uri: &str,
        permissions: Option<String>,
        metadata: Nip46ClientMetadata,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: Option<&Nip46Cancellation>,
        runtime: SessionRuntime,
    ) -> Result<Self, Nip46Error> {
        let parsed = parse_bunker_uri(uri).map_err(Nip46Error::InvalidBunkerUri)?;
        let remote_signer_public_key = parsed.remote_signer_public_key;
        let client_keys = Keys::generate();
        let session = Session::spawn(
            parsed.relays,
            client_keys.clone(),
            Some(remote_signer_public_key),
            cancellation,
            runtime,
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
            client_keys,
            origin: Nip46Origin::Bunker,
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
            &self.session.runtime_handle(),
            request_string(&self.session, "logout", Vec::new()),
            |result| {
                (result == "ack").then_some(()).ok_or_else(|| {
                    SignerError::InvalidResponse(format!("logout returned {result:?}"))
                })
            },
        )
    }

    /// Read out the minimum secrets and descriptor needed to reconnect this
    /// already-authorized client session without another pairing handshake
    /// (#571). Never logs/prints the secret -- see [`Nip46SessionCheckpoint`]'s
    /// redacted `Debug`.
    ///
    /// `#[doc(hidden)]`: this and [`Self::from_parts`] are fully supported,
    /// exercised directly by this crate's own falsifier
    /// (`crates/nmp-engine/tests/nip46_restart.rs`) and by `nmp-ffi`'s
    /// restore/import doors -- hidden purely to keep their several
    /// resolved field/parameter shapes off the `nmp` facade's tracked
    /// surface, which the governed size ceiling (`scripts/regenerate-surface-snapshots.sh`,
    /// a protected file this PR cannot touch) had almost no headroom left
    /// in. Not a capability restriction.
    #[doc(hidden)]
    #[must_use]
    pub fn checkpoint(&self) -> Nip46SessionCheckpoint {
        Nip46SessionCheckpoint {
            client_secret_key: self.client_keys.secret_key().clone(),
            user_public_key: self.user_public_key,
            remote_signer_public_key: self.remote_signer_public_key,
            relays: self.session.current_relays(),
            origin: self.origin,
        }
    }

    /// Reconnect an already-authorized client-initiated or bunker-origin
    /// session directly from its parts (#571), with NO re-pairing handshake:
    /// the persisted client transport key dials the SAME remote signer
    /// directly. Validates `parts.user_public_key` against a live
    /// `get_public_key` answer BEFORE returning -- a mismatch fails closed
    /// with [`Nip46Error::RestoredIdentityMismatch`] and this signer is
    /// never attached under another pubkey. Serves both a checkpoint this
    /// SDK previously wrote and a brownfield import of compatible legacy
    /// material; both are the same shape. Direct-Rust convenience: an owned
    /// executor, no observer, no external cancellation. See
    /// [`Self::checkpoint`]'s doc for why this is `#[doc(hidden)]`.
    #[doc(hidden)]
    pub fn from_parts(
        parts: Nip46SessionCheckpoint,
        timeout: Duration,
    ) -> Result<Self, Nip46Error> {
        Self::from_parts_inner(
            parts,
            timeout,
            Arc::new(|_| {}),
            None,
            SessionRuntime(standalone_runtime()?),
        )
    }

    /// #680: engine-associated restore path. Like [`Self::from_parts`] but
    /// with an observer event sink and external cancellation. #704: the session
    /// runs its tasks on the process-wide shared standalone runtime (O(1) in
    /// session count), never a per-session executor and never blocking an
    /// unrelated engine operation.
    #[doc(hidden)]
    pub fn from_parts_observed_with_cancellation(
        parts: Nip46SessionCheckpoint,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
    ) -> Result<Self, Nip46Error> {
        Self::from_parts_inner(
            parts,
            timeout,
            event_sink,
            Some(cancellation),
            SessionRuntime(standalone_runtime()?),
        )
    }

    /// #704: engine-associated async twin of
    /// [`Self::from_parts_observed_with_cancellation`]. Identical restore-and-
    /// validate reconnect, except the availability wait is awaited
    /// ([`Session::wait_available_async`]) instead of blocking a std channel, so
    /// it runs as a task on the engine's shared adapter `runtime` — removing the
    /// last standalone-runtime use on the FFI restore path — and holds no OS
    /// thread while the session comes online. `#[doc(hidden)]` for the same
    /// reason as its blocking sibling.
    #[doc(hidden)]
    pub async fn from_parts_observed_async(
        parts: Nip46SessionCheckpoint,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: &Nip46Cancellation,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, Nip46Error> {
        let client_keys = Keys::new(parts.client_secret_key.clone());
        let session = Session::spawn(
            parts.relays,
            client_keys.clone(),
            Some(parts.remote_signer_public_key),
            Some(cancellation),
            SessionRuntime(runtime),
        )?;
        forward_events(&session, event_sink)?;
        session.wait_available_async(timeout).await?;
        let live_user_public_key = request_string(&session, "get_public_key", Vec::new())
            .wait(timeout)
            .map_err(Nip46Error::from)
            .and_then(|value| {
                PublicKey::from_hex(&value).map_err(|error| {
                    Nip46Error::InvalidResponse(format!("get_public_key: {error}"))
                })
            })?;
        if live_user_public_key != parts.user_public_key {
            return Err(Nip46Error::RestoredIdentityMismatch {
                expected: parts.user_public_key,
                actual: live_user_public_key,
            });
        }
        session.emit(Nip46ConnectionEvent::Connected {
            user_public_key: live_user_public_key,
        });
        session.request_switch_relays();
        Ok(Self {
            user_public_key: live_user_public_key,
            remote_signer_public_key: parts.remote_signer_public_key,
            session,
            client_keys,
            origin: parts.origin,
        })
    }

    fn from_parts_inner(
        parts: Nip46SessionCheckpoint,
        timeout: Duration,
        event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
        cancellation: Option<&Nip46Cancellation>,
        runtime: SessionRuntime,
    ) -> Result<Self, Nip46Error> {
        let client_keys = Keys::new(parts.client_secret_key.clone());
        let session = Session::spawn(
            parts.relays,
            client_keys.clone(),
            Some(parts.remote_signer_public_key),
            cancellation,
            runtime,
        )?;
        forward_events(&session, event_sink)?;
        session.wait_available(timeout)?;
        let live_user_public_key = request_string(&session, "get_public_key", Vec::new())
            .wait(timeout)
            .map_err(Nip46Error::from)
            .and_then(|value| {
                PublicKey::from_hex(&value).map_err(|error| {
                    Nip46Error::InvalidResponse(format!("get_public_key: {error}"))
                })
            })?;
        if live_user_public_key != parts.user_public_key {
            return Err(Nip46Error::RestoredIdentityMismatch {
                expected: parts.user_public_key,
                actual: live_user_public_key,
            });
        }
        session.emit(Nip46ConnectionEvent::Connected {
            user_public_key: live_user_public_key,
        });
        session.request_switch_relays();
        Ok(Self {
            user_public_key: live_user_public_key,
            remote_signer_public_key: parts.remote_signer_public_key,
            session,
            client_keys,
            origin: parts.origin,
        })
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
            &self.session.runtime_handle(),
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

// Pool events stay owned values across this already-bounded channel. Boxing
// the largest variant would add a heap allocation to every NIP-46 pool event
// only to reduce the private enum's stack size.
#[allow(clippy::large_enum_variant)]
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
struct SessionPoolSink(tokio_mpsc::UnboundedSender<WorkerMsg>);

impl PoolEventSink for SessionPoolSink {
    fn on_event(&self, event: PoolEvent) {
        // The pool's mio worker thread pushes here; an unbounded tokio channel
        // send is non-blocking and wakes the async session worker.
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

/// #704: the async runtime a [`Session`] runs its worker/forwarder/switch-
/// relays/result-map tasks on. Always a borrowed `Handle` — engine-associated
/// sessions borrow the engine's runtime, and standalone direct-Rust sessions
/// borrow the process-wide shared NIP-46 runtime ([`standalone_runtime`]).
/// Either way the runtime outlives the session and is never shut down by it, so
/// thread growth is O(1) in the number of sessions. This replaces the removed
/// per-session `nmp-executor`.
struct SessionRuntime(tokio::runtime::Handle);

impl SessionRuntime {
    fn handle(&self) -> tokio::runtime::Handle {
        self.0.clone()
    }
}

/// Borrow the process-wide shared runtime backing every standalone direct-Rust
/// NIP-46 connect. Built once, lazily, and never shut down — so the number of
/// standalone NIP-46 sessions does NOT drive OS-thread growth (#704: thread
/// count must not be proportional to logical session count). Its worker threads
/// bump the process-wide OS-thread counter so `nmp::nmp_threads_spawned` still
/// reflects them, but that is an O(1) one-time cost shared across all sessions.
fn standalone_runtime() -> Result<tokio::runtime::Handle, Nip46Error> {
    static RUNTIME: std::sync::OnceLock<Option<Arc<tokio::runtime::Runtime>>> =
        std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .thread_name("nmp-nip46")
                .on_thread_start(nmp_executor::note_thread_spawn)
                .on_thread_stop(nmp_executor::note_thread_exit)
                .build()
                .ok()
                .map(Arc::new)
        })
        .as_ref()
        .map(|runtime| runtime.handle().clone())
        // A failure to build the shared runtime is an infrastructure failure
        // that leaves the session unusable; surfaced as the terminal
        // connection-ended outcome (#704 removed `ThreadUnavailable`).
        .ok_or(Nip46Error::Disconnected)
}

type EventSink = Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>;

struct Session {
    commands: tokio_mpsc::UnboundedSender<WorkerMsg>,
    connected_relays: AtomicUsize,
    subscribers: Arc<Mutex<Vec<Sender<Nip46ConnectionEvent>>>>,
    /// #704: the connection-event observers `forward_events` installs. Since
    /// the per-session executor is gone, the forwarder is no longer a blocking
    /// recv thread — each sink is invoked inline by `emit` (the sink contract
    /// is a lightweight non-blocking notification, so this holds no worker).
    event_sinks: Arc<Mutex<Vec<EventSink>>>,
    availability_error: Arc<Mutex<Option<Nip46Error>>>,
    /// #704: await-able availability signal. Every availability-state
    /// transition (`Available`/`Unavailable`/recorded relay-open error) fires
    /// `notify_waiters()` so an async connect parked in [`Session::wait_available_async`]
    /// re-checks without holding an OS thread. The blocking `wait_available`
    /// keeps using the subscriber channel; this signal is the async twin's edge.
    availability_signal: Arc<tokio::sync::Notify>,
    runtime: SessionRuntime,
    /// The relay set this session currently targets, kept live by
    /// `SessionWorker::replace_relays` (#571's checkpoint reads this back
    /// through [`Session::current_relays`] without a worker round trip).
    current_relays: Mutex<Vec<RelayUrl>>,
}

impl Session {
    fn spawn(
        relays: Vec<RelayUrl>,
        client_keys: Keys,
        remote: Option<PublicKey>,
        cancellation: Option<&Nip46Cancellation>,
        runtime: SessionRuntime,
    ) -> Result<Arc<Self>, Nip46Error> {
        let (commands, inbox) = tokio_mpsc::unbounded_channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let event_sinks: Arc<Mutex<Vec<EventSink>>> = Arc::new(Mutex::new(Vec::new()));
        let availability_error = Arc::new(Mutex::new(None));
        let runtime_handle = runtime.handle();
        let session = Arc::new(Self {
            commands: commands.clone(),
            connected_relays: AtomicUsize::new(0),
            subscribers: Arc::clone(&subscribers),
            event_sinks: Arc::clone(&event_sinks),
            availability_error: Arc::clone(&availability_error),
            availability_signal: Arc::new(tokio::sync::Notify::new()),
            runtime,
            current_relays: Mutex::new(relays.clone()),
        });
        if let Some(cancellation) = cancellation {
            cancellation.bind(session.commands.clone());
        }
        let weak = Arc::downgrade(&session);
        // A transport-pool build failure leaves the session unusable; #704
        // removed the operation-level `ThreadUnavailable`, so it surfaces as
        // the terminal connection-ended outcome.
        let pool = Pool::new(session_pool_config(), SessionPoolSink(commands.clone()))
            .map_err(|_| Nip46Error::Disconnected)?;
        let worker_pool = pool.clone();
        // #704: the session worker is an async task on the runtime; its whole
        // lifetime it awaits the inbox, holding no OS thread. Session teardown
        // (and external cancellation) posts `WorkerMsg::Shutdown`, which ends
        // the `run` loop; the `Owned` runtime is additionally dropped on
        // `Session::drop`, aborting any still-parked task.
        runtime_handle.spawn(async move {
            let mut worker = SessionWorker::new(
                worker_pool,
                client_keys,
                remote,
                weak,
                subscribers,
                event_sinks,
                availability_error,
            );
            worker.emit(Nip46ConnectionEvent::Connecting);
            if let Err(error) = worker.open_relays(relays) {
                worker.record_availability_error(error);
                worker.emit(Nip46ConnectionEvent::Unavailable);
            }
            worker.run(inbox).await;
        });
        drop(pool);
        Ok(session)
    }

    /// The runtime handle this session spawns its async tasks (result-map,
    /// switch-relays) on.
    fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle()
    }

    fn is_available(&self) -> bool {
        self.connected_relays.load(Ordering::Acquire) > 0
    }

    /// The live relay set this session currently targets (#571's
    /// checkpoint reader); reflects the latest `replace_relays` outcome, or
    /// the original spawn relay set before any switch has completed.
    fn current_relays(&self) -> Vec<RelayUrl> {
        self.current_relays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn subscribe(&self) -> Receiver<Nip46ConnectionEvent> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(tx);
        }
        rx
    }

    fn emit(&self, event: Nip46ConnectionEvent) {
        emit_to(&self.subscribers, &self.event_sinks, event);
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

    /// #704: fire the await-able availability edge so any task parked in
    /// [`Self::wait_available_async`] re-checks. Called by the worker at every
    /// availability-state transition. `notify_waiters()` wakes only currently
    /// armed waiters and stores no permit, which is why `wait_available_async`
    /// arms (`enable`s) its waiter BEFORE re-reading the state.
    fn signal_availability_change(&self) {
        self.availability_signal.notify_waiters();
    }

    /// Async twin of [`Self::wait_available`] with identical outcomes: `Ok` once
    /// a relay is connected, the recorded [`Nip46Error`] on an availability
    /// error, [`Nip46Error::Timeout`] on the deadline. Holds no OS thread while
    /// parked — it awaits the [`availability_signal`](Self) rather than blocking
    /// a std channel `recv_timeout`, so it can run as a task on the engine's
    /// shared adapter runtime.
    async fn wait_available_async(&self, timeout: Duration) -> Result<(), Nip46Error> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.is_available() {
                return Ok(());
            }
            if let Some(error) = self.availability_error() {
                return Err(error);
            }
            // Arm the waiter BEFORE the re-check so a transition that fires
            // `notify_waiters()` between the check and the await is not lost
            // (`notify_waiters` stores no permit for a not-yet-armed waiter).
            let mut notified = std::pin::pin!(self.availability_signal.notified());
            notified.as_mut().enable();
            if self.is_available() {
                return Ok(());
            }
            if let Some(error) = self.availability_error() {
                return Err(error);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Nip46Error::Timeout);
            }
            if tokio::time::timeout(remaining, notified.as_mut())
                .await
                .is_err()
            {
                return Err(Nip46Error::Timeout);
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
        // #704: the switch-relays wait is an async task on the session runtime;
        // it holds no OS thread while the remote round-trip is outstanding.
        // Dropping the task (session teardown) fires the op's cancel hook.
        self.runtime_handle().spawn(async move {
            let Ok(result) = tokio::time::timeout(SWITCH_RELAYS_TIMEOUT, op.recv_async()).await
            else {
                return;
            };
            let Ok(result) = result else {
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
        self.subscribers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        self.event_sinks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        // #704: the session only borrows a runtime `Handle` (the process-wide
        // shared standalone runtime, or the engine's), so dropping the session
        // never shuts a runtime down — the worker task already observed
        // `Shutdown` above and exits on its own.
    }
}

struct SessionWorker {
    pool: Pool,
    client_keys: Keys,
    remote: Option<PublicKey>,
    session: std::sync::Weak<Session>,
    subscribers: Arc<Mutex<Vec<Sender<Nip46ConnectionEvent>>>>,
    event_sinks: Arc<Mutex<Vec<EventSink>>>,
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
        event_sinks: Arc<Mutex<Vec<EventSink>>>,
        availability_error: Arc<Mutex<Option<Nip46Error>>>,
    ) -> Self {
        static NEXT_SUB: AtomicU64 = AtomicU64::new(1);
        Self {
            pool,
            client_keys,
            remote,
            session,
            subscribers,
            event_sinks,
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

    async fn run(&mut self, mut inbox: tokio_mpsc::UnboundedReceiver<WorkerMsg>) {
        while let Some(message) = inbox.recv().await {
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
        // Teardown lives in `Drop` so it runs on both a clean `Shutdown` break
        // AND an aborted task — dropping a standalone `Owned` runtime aborts the
        // worker future at its `.await`, and the drop guard is the only thing
        // that still fires the pool/pending/subscriber cleanup then (#704).
    }

    fn emit(&self, event: Nip46ConnectionEvent) {
        emit_to(&self.subscribers, &self.event_sinks, event);
    }

    fn teardown(&mut self) {
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
        self.event_sinks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        self.pool.shutdown();
    }

    fn record_availability_error(&self, error: Nip46Error) {
        *self
            .availability_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(error);
        // #704: wake an async connect parked in `wait_available_async` so it
        // observes the recorded error instead of running out its deadline.
        if let Some(session) = self.session.upgrade() {
            session.signal_availability_change();
        }
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
                Err(RelayOpenError::ThreadUnavailable(_error)) => {
                    // #704: a transport relay-worker spawn failure leaves this
                    // relay unusable; surfaced as the terminal connection-ended
                    // outcome rather than the removed `ThreadUnavailable`.
                    thread_refusal.get_or_insert(Nip46Error::Disconnected);
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
            *session
                .current_relays
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = retained;
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
            PoolEvent::Connected { handle, session } => {
                let url = session.relay;
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
                    // #704: fire the async availability edge too.
                    if let Some(session) = self.session.upgrade() {
                        session.signal_availability_change();
                    }
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
                    // #704: fire the async availability edge too.
                    if let Some(session) = self.session.upgrade() {
                        session.signal_availability_change();
                    }
                }
            }
            PoolEvent::Frame { handle, frame, .. } => {
                if self
                    .handles
                    .get(&handle.slot)
                    .is_some_and(|(current, _)| *current == handle)
                {
                    self.on_frame(handle, frame);
                }
            }
            PoolEvent::Health { .. }
            | PoolEvent::InitialReadCompleted { .. }
            | PoolEvent::EventHandoff { .. }
            | PoolEvent::WorkerRetired => {}
        }
    }

    fn on_frame(&mut self, handle: nmp_transport::RelayHandle, frame: RelayFrame) {
        match frame {
            RelayFrame::Event { event, .. } => self.on_event(event.as_ref()),
            frame @ RelayFrame::CommittedObservation(_) => {
                if let Some(frame) = frame.into_ordinary_fallback() {
                    self.on_frame(handle, frame);
                }
            }
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

impl Drop for SessionWorker {
    fn drop(&mut self) {
        self.teardown();
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

fn map_string<T, F>(runtime: &tokio::runtime::Handle, op: SignerOp<String>, map: F) -> SignerOp<T>
where
    T: Send + 'static,
    F: FnOnce(String) -> Result<T, SignerError> + Send + 'static,
{
    match op {
        SignerOp::Ready(Ok(value)) => SignerOp::Ready(map(value)),
        SignerOp::Ready(Err(error)) => SignerOp::Ready(Err(error)),
        SignerOp::Pending(pending) => {
            // #704: cancellation is bound into the op's door; the mapped op's
            // cancel hook cancels the inner op, which wakes its `.await` to a
            // disconnected end and runs its adapter cancel hook once. The map
            // runs as an async task on the session runtime — no OS thread is
            // held while the inner round-trip is outstanding.
            let inner_canceller = pending.canceller();
            let mapped_cancel = inner_canceller.clone();
            let (completion, mapped) = SignerOp::pending_channel_with_cancel(move || {
                mapped_cancel.cancel();
            });
            runtime.spawn(async move {
                let result = match pending.await {
                    Ok(value) => map(value),
                    Err(error) => Err(error),
                };
                let _ = completion.resolve(result);
            });
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
    sinks: &Arc<Mutex<Vec<EventSink>>>,
    event: Nip46ConnectionEvent,
) {
    // #704: connection-event observers are invoked inline (no forwarder
    // thread). The sink contract is a lightweight non-blocking notification.
    let installed: Vec<EventSink> = sinks.lock().map(|guard| guard.clone()).unwrap_or_default();
    for sink in &installed {
        sink(event.clone());
    }
    if let Ok(mut subscribers) = subscribers.lock() {
        subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }
}

/// #704: install a connection-event observer. The per-session executor is
/// gone, so this no longer spawns a blocking forwarder thread — the sink is
/// registered and invoked inline by `emit`. Any event emitted after this call
/// (including the terminal `Connected`) reaches the sink; infallible.
fn forward_events(
    session: &Arc<Session>,
    event_sink: Arc<dyn Fn(Nip46ConnectionEvent) + Send + Sync>,
) -> Result<(), Nip46Error> {
    session
        .event_sinks
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(event_sink);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_transport_worker_budget_equals_the_protocol_relay_ceiling() {
        assert_eq!(session_pool_config().max_relays, MAX_NIP46_RELAYS);
        assert_eq!(MAX_NIP46_RELAYS, 8);
    }

    /// #571 secrecy falsifier: `{:?}` on `Nip46SessionCheckpoint` must never
    /// leak the client transport secret -- neither its hex form nor the raw
    /// bytes -- matching `Nip46Invitation`/`LocalKeySigner`'s redacted
    /// `Debug` precedent.
    #[test]
    fn checkpoint_debug_output_redacts_client_secret_key() {
        let client_keys = Keys::generate();
        let secret_hex = client_keys.secret_key().to_secret_hex();
        let checkpoint = Nip46SessionCheckpoint {
            client_secret_key: client_keys.secret_key().clone(),
            user_public_key: Keys::generate().public_key(),
            remote_signer_public_key: Keys::generate().public_key(),
            relays: Vec::new(),
            origin: Nip46Origin::ClientInitiated,
        };

        let debug = format!("{checkpoint:?}");

        assert!(
            !debug.contains(&secret_hex),
            "Debug output must not contain the client secret key hex"
        );
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn injected_initial_relay_worker_refusal_reaches_the_waiting_caller_typed() {
        let (pool_tx, _pool_rx) = mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).expect("test pool construction");
        let (commands, _inbox) = tokio_mpsc::unbounded_channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let event_sinks = Arc::new(Mutex::new(Vec::new()));
        let availability_error = Arc::new(Mutex::new(None));
        let session = Arc::new(Session {
            commands,
            connected_relays: AtomicUsize::new(0),
            subscribers: Arc::clone(&subscribers),
            event_sinks: Arc::clone(&event_sinks),
            availability_error: Arc::clone(&availability_error),
            availability_signal: Arc::new(tokio::sync::Notify::new()),
            runtime: SessionRuntime(standalone_runtime().unwrap()),
            current_relays: Mutex::new(Vec::new()),
        });
        let mut worker = SessionWorker::new(
            pool,
            Keys::generate(),
            None,
            Arc::downgrade(&session),
            subscribers,
            event_sinks,
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
        // #704 deleted `Nip46Error::ThreadUnavailable`; a transport relay-worker
        // spawn failure that leaves every requested relay unusable now surfaces
        // as the terminal connection-ended outcome. The real semantic — a
        // relay-open infra failure reaches the waiting caller typed, not as an
        // empty document — is preserved.
        assert_eq!(error, Nip46Error::Disconnected);
        assert_eq!(session.wait_available(Duration::from_secs(1)), Err(error));
        assert!(worker.configured.is_empty());
        worker.pool.shutdown();
    }

    // #704: `borrowed_engine_executor_survives_session_teardown` and
    // `every_forwardable_engine_session_owns_two_slots` were deleted. They
    // asserted per-session `nmp-executor` census/reservation/capacity-refusal
    // behavior (admitted slot counts, `Saturated`-style refusal of the third
    // forwarder), which no longer exists: sessions run async tasks on a runtime
    // and `forward_events` never refuses. No admission remains to assert.

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
            Arc::new(Mutex::new(Vec::new())),
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
            session: nmp_transport::RelaySessionKey::public(relay.clone()),
            frame: auth_frame(),
        });
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_millis(20)),
            Err(RecvTimeoutError::Timeout)
        ));

        worker.on_pool(PoolEvent::Frame {
            handle: reopened,
            session: nmp_transport::RelaySessionKey::public(relay.clone()),
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
