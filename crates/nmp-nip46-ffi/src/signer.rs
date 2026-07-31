//! Native NIP-46 discovery and connection projection.
//!
//! Rust owns catalog/protocol/lifecycle policy. Native shells only execute
//! the supplied OS probe/launch URI and render these bounded progress facts.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use nmp_component_interface::{
    new_signer_adapter, ComponentInterfaceError, FfiSignerAdapter, ProviderAdapterTask,
    SignerAdapterControl, SignerAdapterRuntime, COMPONENT_INTERFACE_IDENTITY,
};
use nmp_nip46 as nmp_signer;

// Keep the provider projection readable without importing the core engine.
// This private namespace contains only protocol primitives and provider
// values; it never crosses UniFFI.
mod provider_surface {
    pub use nmp_nip46::{
        known_nip46_signers, Nip46ClientMetadata, Nip46ConnectionEvent, Nip46Invitation,
        Nip46Signer,
    };
    pub use nostr::{PublicKey, RelayUrl};
}
use provider_surface as nmp;

struct ComponentNip46Runtime {
    runtime: SignerAdapterRuntime,
    connection: Weak<Nip46Connection>,
}

struct ComponentNip46TaskHandle {
    task: tokio::task::AbortHandle,
    supervisor: tokio::task::AbortHandle,
}

impl nmp_signer::Nip46RuntimeTaskHandle for ComponentNip46TaskHandle {
    fn abort(&self) {
        self.supervisor.abort();
        self.task.abort();
    }

    fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl nmp_signer::Nip46TaskRuntime for ComponentNip46Runtime {
    fn spawn(
        &self,
        task: nmp_signer::Nip46RuntimeTask,
    ) -> Box<dyn nmp_signer::Nip46RuntimeTaskHandle> {
        let task = self.runtime.spawn(task);
        let task_abort = task.abort_handle();
        let connection = self.connection.clone();
        let supervisor = self.runtime.spawn(async move {
            if let Err(error) = task.await {
                if let Some(connection) = connection.upgrade() {
                    let reason = if error.is_panic() {
                        "NIP-46 child runtime task panicked"
                    } else {
                        "NIP-46 child runtime task was cancelled unexpectedly"
                    };
                    connection.fail(FfiNip46Failure::CoreRefused {
                        reason: reason.to_string(),
                    });
                }
            }
        });
        Box::new(ComponentNip46TaskHandle {
            task: task_abort,
            supervisor: supervisor.abort_handle(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiNip46SignerApp {
    pub id: String,
    pub display_name: String,
    pub ios_detection_uri: Option<String>,
    pub nip46_launch_scheme: Option<String>,
    pub android_detection_uri: Option<String>,
    pub android_package_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct FfiNip46ClientMetadata {
    pub name: Option<String>,
    pub url: Option<String>,
    pub image: Option<String>,
}

/// Synchronous provider-boundary refusal. Connection/session failures remain
/// in [`FfiNip46Failure`]; this type covers malformed caller values and a
/// closed provider adapter before a session worker can start.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiNip46ProviderError {
    InvalidSecretKey,
    InvalidPublicKey { field: String },
    InvalidRelay { relay: String },
    InvalidSigner { reason: String },
    ProviderBindingMismatch { expected: String, actual: String },
    ProviderNativeMismatch { expected: String, actual: String },
    PackageInterfaceMismatch { expected: String, actual: String },
    CoreIdentityMismatch { expected: String, actual: String },
}

impl std::fmt::Display for FfiNip46ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSecretKey => f.write_str("invalid NIP-46 client secret key"),
            Self::InvalidPublicKey { field } => {
                write!(f, "invalid NIP-46 public key field: {field}")
            }
            Self::InvalidRelay { relay } => write!(f, "invalid NIP-46 relay: {relay:?}"),
            Self::InvalidSigner { reason } => write!(f, "invalid NIP-46 signer: {reason}"),
            Self::ProviderBindingMismatch { expected, actual } => write!(
                f,
                "NIP-46 provider native identity is {expected}, packaged binding is {actual}"
            ),
            Self::ProviderNativeMismatch { expected, actual } => write!(
                f,
                "NIP-46 provider was built as {expected}, loaded native identity is {actual}"
            ),
            Self::PackageInterfaceMismatch { expected, actual } => write!(
                f,
                "NIP-46 provider interface is {expected}, packaged interface is {actual}"
            ),
            Self::CoreIdentityMismatch { expected, actual } => write!(
                f,
                "NIP-46 provider requires core component {expected}, loaded {actual}"
            ),
        }
    }
}

impl std::error::Error for FfiNip46ProviderError {}

fn parse_pubkey(value: &str, field: &'static str) -> Result<nmp::PublicKey, FfiNip46ProviderError> {
    nmp::PublicKey::from_hex(value).map_err(|_| FfiNip46ProviderError::InvalidPublicKey {
        field: field.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiNip46ConnectionEvent {
    Connecting,
    Available,
    Unavailable,
    RelayAuthentication { relay: String },
    AuthorizationRequired { url: String },
    Connected { user_public_key: String },
}

/// `nmp_signer::Nip46Origin` mirror (#571): distinguishes a session paired
/// via `nostrconnect://` from one dialed via `bunker://`. Restore mechanics
/// are identical either way -- kept because "absence of a reusable client
/// checkpoint is observable rather than guessed from partial metadata" is a
/// hard requirement, not because restore branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiNip46Origin {
    ClientInitiated,
    Bunker,
}

/// `nmp_signer::Nip46SessionCheckpoint` mirror (#571) -- the minimum secrets
/// and descriptor needed to reconnect an already-authorized NIP-46 client
/// session without another pairing handshake. `client_secret_key` crosses
/// this boundary once, matching `add_account`'s existing precedent; native
/// callers must never log, print, serialize to diagnostics, or otherwise
/// surface it outside their own secure checkpoint store.
#[derive(Clone, uniffi::Record)]
pub struct FfiNip46SessionCheckpoint {
    pub client_secret_key: String,
    pub user_public_key: String,
    pub remote_signer_public_key: String,
    pub relays: Vec<String>,
    pub origin: FfiNip46Origin,
}

/// Redacted like `Nip46SessionCheckpoint`'s own `Debug` -- never prints
/// `client_secret_key`.
impl std::fmt::Debug for FfiNip46SessionCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiNip46SessionCheckpoint")
            .field("client_secret_key", &"[redacted]")
            .field("user_public_key", &self.user_public_key)
            .field("remote_signer_public_key", &self.remote_signer_public_key)
            .field("relays", &self.relays)
            .field("origin", &self.origin)
            .finish()
    }
}

fn nip46_origin_to_ffi(origin: nmp_signer::Nip46Origin) -> FfiNip46Origin {
    match origin {
        nmp_signer::Nip46Origin::ClientInitiated => FfiNip46Origin::ClientInitiated,
        nmp_signer::Nip46Origin::Bunker => FfiNip46Origin::Bunker,
    }
}

fn nip46_origin_from_ffi(origin: FfiNip46Origin) -> nmp_signer::Nip46Origin {
    match origin {
        FfiNip46Origin::ClientInitiated => nmp_signer::Nip46Origin::ClientInitiated,
        FfiNip46Origin::Bunker => nmp_signer::Nip46Origin::Bunker,
    }
}

fn checkpoint_to_ffi(checkpoint: nmp_signer::Nip46SessionCheckpoint) -> FfiNip46SessionCheckpoint {
    FfiNip46SessionCheckpoint {
        client_secret_key: checkpoint.client_secret_key.to_secret_hex(),
        user_public_key: checkpoint.user_public_key.to_hex(),
        remote_signer_public_key: checkpoint.remote_signer_public_key.to_hex(),
        relays: checkpoint
            .relays
            .into_iter()
            .map(|r| r.to_string())
            .collect(),
        origin: nip46_origin_to_ffi(checkpoint.origin),
    }
}

/// Parses every field of an [`FfiNip46SessionCheckpoint`] into the typed
/// Rust shape `Nip46Signer::from_parts` needs. Corrupt/malformed input
/// (secret key, either public key, or a relay URL) fails closed with a
/// typed `FfiError` and never partially constructs a checkpoint.
fn checkpoint_from_ffi(
    checkpoint: FfiNip46SessionCheckpoint,
) -> Result<nmp_signer::Nip46SessionCheckpoint, FfiNip46ProviderError> {
    let client_secret_key = nostr::SecretKey::parse(&checkpoint.client_secret_key)
        .map_err(|_| FfiNip46ProviderError::InvalidSecretKey)?;
    let user_public_key = parse_pubkey(&checkpoint.user_public_key, "user_public_key")?;
    let remote_signer_public_key = parse_pubkey(
        &checkpoint.remote_signer_public_key,
        "remote_signer_public_key",
    )?;
    let relays = checkpoint
        .relays
        .into_iter()
        .map(|relay| {
            nmp::RelayUrl::parse(&relay).map_err(|_| FfiNip46ProviderError::InvalidRelay { relay })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(nmp_signer::Nip46SessionCheckpoint {
        client_secret_key,
        user_public_key,
        remote_signer_public_key,
        relays,
        origin: nip46_origin_from_ffi(checkpoint.origin),
    })
}

/// `nmp_signer::BunkerParseError` mirror (#494) -- strict `bunker://` token
/// parsing, carried instead of collapsing into `Nip46Error::InvalidBunkerUri`'s
/// own `.to_string()`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBunkerParseError {
    Empty,
    TooLong { len: u64 },
    WrongScheme,
    MissingRemoteSignerKey,
    InvalidRemoteSignerKey,
    MissingRelay,
    TooManyRelays { count: u64 },
    InvalidRelay { relay: String },
    Malformed { reason: String },
}

/// `nmp_signer::Nip46Error` mirror (#494) -- every live discriminant a NIP-46
/// connection attempt can fail with, so a native caller can branch on
/// "auth required" vs. "timeout" vs. "malformed" instead of parsing English.
/// `Nip46Error::InvalidRelay`/`InvalidInvitation`/`SecretMismatch` are not
/// mirrored: nothing in the workspace ever constructs them (see that type's
/// own doc).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiNip46Failure {
    InvalidBunkerUri {
        source: FfiBunkerParseError,
    },
    MissingRelay,
    TooManyRelays {
        count: u64,
    },
    InvitationTooLong {
        len: u64,
    },
    InvalidLaunchScheme {
        scheme: String,
    },
    Timeout,
    Disconnected,
    Rejected {
        reason: String,
    },
    InvalidResponse {
        reason: String,
    },
    /// The provider signer supplied no stable public key. This is not a
    /// `Nip46Error` variant; it crosses the component-interface taxonomy at
    /// the same observer seam.
    SignerMissingPublicKey,
    CapabilityRegistryFull {
        limit: u64,
    },
    CapabilityInstanceExhausted,
    AdapterClosed,
    CoreRefused {
        reason: String,
    },
    /// A restore/import's live `get_public_key` answer did not match the
    /// checkpoint's expected identity (#571). No signer was attached under
    /// the wrong pubkey.
    RestoredIdentityMismatch {
        expected: String,
        actual: String,
    },
}

#[uniffi::export(callback_interface)]
pub trait Nip46ConnectionObserver: Send + Sync {
    fn on_event(&self, event: FfiNip46ConnectionEvent);
    /// The relay handshake is complete and the signer has been attached to
    /// this engine. A callback/deep-link alone never produces this fact.
    fn on_ready(&self, user_public_key: String);
    fn on_failed(&self, failure: FfiNip46Failure);
    fn on_closed(&self);
}

fn bunker_parse_error_to_ffi(error: nmp_signer::BunkerParseError) -> FfiBunkerParseError {
    match error {
        nmp_signer::BunkerParseError::Empty => FfiBunkerParseError::Empty,
        nmp_signer::BunkerParseError::TooLong(len) => {
            FfiBunkerParseError::TooLong { len: len as u64 }
        }
        nmp_signer::BunkerParseError::WrongScheme => FfiBunkerParseError::WrongScheme,
        nmp_signer::BunkerParseError::MissingRemoteSignerKey => {
            FfiBunkerParseError::MissingRemoteSignerKey
        }
        nmp_signer::BunkerParseError::InvalidRemoteSignerKey => {
            FfiBunkerParseError::InvalidRemoteSignerKey
        }
        nmp_signer::BunkerParseError::MissingRelay => FfiBunkerParseError::MissingRelay,
        nmp_signer::BunkerParseError::TooManyRelays(count) => FfiBunkerParseError::TooManyRelays {
            count: count as u64,
        },
        nmp_signer::BunkerParseError::InvalidRelay(relay) => {
            FfiBunkerParseError::InvalidRelay { relay }
        }
        nmp_signer::BunkerParseError::Malformed(reason) => {
            FfiBunkerParseError::Malformed { reason }
        }
    }
}

fn nip46_failure_to_ffi(error: nmp_signer::Nip46Error) -> FfiNip46Failure {
    match error {
        nmp_signer::Nip46Error::InvalidBunkerUri(source) => FfiNip46Failure::InvalidBunkerUri {
            source: bunker_parse_error_to_ffi(source),
        },
        nmp_signer::Nip46Error::MissingRelay => FfiNip46Failure::MissingRelay,
        nmp_signer::Nip46Error::TooManyRelays(count) => FfiNip46Failure::TooManyRelays {
            count: count as u64,
        },
        nmp_signer::Nip46Error::InvitationTooLong(len) => {
            FfiNip46Failure::InvitationTooLong { len: len as u64 }
        }
        nmp_signer::Nip46Error::InvalidLaunchScheme(scheme) => {
            FfiNip46Failure::InvalidLaunchScheme { scheme }
        }
        nmp_signer::Nip46Error::Timeout => FfiNip46Failure::Timeout,
        nmp_signer::Nip46Error::Disconnected => FfiNip46Failure::Disconnected,
        nmp_signer::Nip46Error::Rejected(reason) => FfiNip46Failure::Rejected { reason },
        nmp_signer::Nip46Error::InvalidResponse(reason) => {
            FfiNip46Failure::InvalidResponse { reason }
        }
        nmp_signer::Nip46Error::RestoredIdentityMismatch { expected, actual } => {
            FfiNip46Failure::RestoredIdentityMismatch {
                expected: expected.to_hex(),
                actual: actual.to_hex(),
            }
        }
    }
}

/// Component-interface attachment failure -> [`FfiNip46Failure`].
/// Preserve every reachable refusal rather than collapsing registry pressure,
/// instance exhaustion, lifecycle closure, and core refusal into a
/// misleading "missing public key".
fn engine_attach_failure_to_ffi(error: ComponentInterfaceError) -> FfiNip46Failure {
    match error {
        ComponentInterfaceError::EngineClosed => FfiNip46Failure::Disconnected,
        ComponentInterfaceError::SignerMissingPublicKey => FfiNip46Failure::SignerMissingPublicKey,
        ComponentInterfaceError::CapabilityRegistryFull { limit } => {
            FfiNip46Failure::CapabilityRegistryFull {
                limit: limit as u64,
            }
        }
        ComponentInterfaceError::CapabilityInstanceExhausted => {
            FfiNip46Failure::CapabilityInstanceExhausted
        }
        ComponentInterfaceError::AdapterClosed => FfiNip46Failure::AdapterClosed,
        ComponentInterfaceError::CoreRefused { reason } => FfiNip46Failure::CoreRefused { reason },
    }
}

#[derive(uniffi::Object)]
pub struct FfiNip46Invitation {
    inner: Mutex<Option<nmp::Nip46Invitation>>,
}

struct Nip46Attachment {
    signer: Option<nmp::Nip46Signer>,
    available: bool,
    attached: bool,
}

/// Everything [`drive_desired_signer_state`] needs from an attached signer:
/// the public key it reports to the connection, and the signer's own crossing
/// onto the adapter control lane. [`nmp::Nip46Signer`] is the only production
/// implementation; the driver is generic over this so its command sequencing
/// can be falsified without a live remote-signer session.
trait DesiredSigner: Clone + Send + Sync + 'static {
    fn public_key_hex(&self) -> String;

    fn attach<'a>(
        self: Box<Self>,
        control: &'a SignerAdapterControl,
    ) -> Pin<Box<dyn Future<Output = Result<(), ComponentInterfaceError>> + Send + 'a>>;
}

impl DesiredSigner for nmp::Nip46Signer {
    fn public_key_hex(&self) -> String {
        self.user_public_key().to_hex()
    }

    fn attach<'a>(
        self: Box<Self>,
        control: &'a SignerAdapterControl,
    ) -> Pin<Box<dyn Future<Output = Result<(), ComponentInterfaceError>> + Send + 'a>> {
        Box::pin(control.attach_boxed(self))
    }
}

#[derive(Clone)]
enum DesiredSignerState<S = nmp::Nip46Signer> {
    Detached,
    Attached(Box<S>),
}

enum ObserverDelivery {
    Event(FfiNip46ConnectionEvent),
    Ready(String),
    Failed(FfiNip46Failure),
    Closed,
}

#[derive(Default)]
struct ObserverDeliveryState {
    queue: VecDeque<ObserverDelivery>,
    draining: bool,
    terminal_queued: bool,
}

/// Owns one remote-signer session. The native connection handle, not the
/// engine, owns this value: `disconnect()`/drop therefore detach
/// deterministically instead of accumulating sessions until engine shutdown.
/// Connection workers and callbacks retain only `Weak` references, avoiding
/// both an ownership cycle and a pending-handshake keepalive.
#[derive(uniffi::Object)]
pub struct Nip46Connection {
    desired: Mutex<Option<tokio::sync::watch::Sender<DesiredSignerState>>>,
    observer: Arc<dyn Nip46ConnectionObserver>,
    /// Serializes attachment transitions with observer-queue insertion. The
    /// queue itself invokes callbacks outside this lock, so a callback may
    /// safely call `disconnect()` without deadlocking.
    lifecycle: Mutex<()>,
    deliveries: Mutex<ObserverDeliveryState>,
    attachment: Mutex<Nip46Attachment>,
    cancellation: nmp_signer::Nip46Cancellation,
    closed: AtomicBool,
}

impl Nip46Connection {
    fn new(
        desired: tokio::sync::watch::Sender<DesiredSignerState>,
        observer: Arc<dyn Nip46ConnectionObserver>,
        cancellation: nmp_signer::Nip46Cancellation,
    ) -> Arc<Self> {
        Arc::new(Self {
            desired: Mutex::new(Some(desired)),
            observer,
            lifecycle: Mutex::new(()),
            deliveries: Mutex::new(ObserverDeliveryState::default()),
            attachment: Mutex::new(Nip46Attachment {
                signer: None,
                available: false,
                attached: false,
            }),
            cancellation,
            closed: AtomicBool::new(false),
        })
    }

    fn on_event(self: &Arc<Self>, event: nmp::Nip46ConnectionEvent) {
        let should_drain = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            match &event {
                nmp::Nip46ConnectionEvent::Available => {
                    let mut attachment = self
                        .attachment
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    attachment.available = true;
                    if let Some(signer) = attachment.signer.clone() {
                        self.set_desired(DesiredSignerState::Attached(Box::new(signer)));
                    }
                }
                nmp::Nip46ConnectionEvent::Unavailable => {
                    let mut attachment = self
                        .attachment
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    attachment.available = false;
                    self.set_desired(DesiredSignerState::Detached);
                }
                _ => {}
            }
            if matches!(&event, nmp::Nip46ConnectionEvent::Unavailable) {
                false
            } else {
                self.enqueue_delivery(ObserverDelivery::Event(event_to_ffi(event)))
            }
        };
        self.drain_deliveries(should_drain);
    }

    fn attach(self: &Arc<Self>, signer: nmp::Nip46Signer) {
        {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut attachment = self
                .attachment
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            attachment.signer = Some(signer.clone());
            if !attachment.available {
                return;
            }
            self.set_desired(DesiredSignerState::Attached(Box::new(signer)));
        }
    }

    fn set_desired(&self, desired: DesiredSignerState) {
        if let Some(sender) = self
            .desired
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
        {
            sender.send_replace(desired);
        }
    }

    fn adapter_attached(&self, public_key: String, result: Result<(), ComponentInterfaceError>) {
        let should_drain = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            match result {
                Ok(()) => {
                    let mut attachment = self
                        .attachment
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    let current = attachment.available
                        && attachment
                            .signer
                            .as_ref()
                            .is_some_and(|signer| signer.user_public_key().to_hex() == public_key);
                    attachment.attached = current;
                    drop(attachment);
                    current && self.enqueue_delivery(ObserverDelivery::Ready(public_key))
                }
                Err(error) => self.fail_locked(engine_attach_failure_to_ffi(error)),
            }
        };
        self.drain_deliveries(should_drain);
    }

    fn adapter_detached(&self, result: Result<(), ComponentInterfaceError>) {
        let should_drain = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            match result {
                Ok(()) => {
                    self.attachment
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .attached = false;
                    self.enqueue_delivery(ObserverDelivery::Event(
                        FfiNip46ConnectionEvent::Unavailable,
                    ))
                }
                Err(error) => self.fail_locked(engine_attach_failure_to_ffi(error)),
            }
        };
        self.drain_deliveries(should_drain);
    }

    fn fail(&self, failure: FfiNip46Failure) {
        let should_drain = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            self.fail_locked(failure)
        };
        self.drain_deliveries(should_drain);
    }

    fn fail_locked(&self, failure: FfiNip46Failure) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut should_drain = self.enqueue_delivery(ObserverDelivery::Failed(failure));
        self.detach_locked();
        should_drain |= self.enqueue_delivery(ObserverDelivery::Closed);
        should_drain
    }

    fn close_inner(&self) {
        let should_drain = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.closed.swap(true, Ordering::AcqRel) {
                return;
            }
            self.detach_locked();
            self.enqueue_delivery(ObserverDelivery::Closed)
        };
        self.drain_deliveries(should_drain);
    }

    fn detach_locked(&self) {
        self.cancellation.cancel();
        {
            let mut attachment = self
                .attachment
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            attachment.signer = None;
            attachment.available = false;
            attachment.attached = false;
        }
        if let Some(desired) = self
            .desired
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            desired.send_replace(DesiredSignerState::Detached);
        }
    }

    /// Queue one observer fact. Returns true only to the caller elected to
    /// drain the queue; all other producers leave their facts for that same
    /// drainer. `Closed` seals the queue before its callback runs, so no later
    /// producer can append a post-terminal fact.
    fn enqueue_delivery(&self, delivery: ObserverDelivery) -> bool {
        let mut state = self
            .deliveries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.terminal_queued {
            return false;
        }
        if matches!(&delivery, ObserverDelivery::Closed) {
            state.terminal_queued = true;
        }
        state.queue.push_back(delivery);
        if state.draining {
            false
        } else {
            state.draining = true;
            true
        }
    }

    fn drain_deliveries(&self, should_drain: bool) {
        if !should_drain {
            return;
        }
        loop {
            let delivery = {
                let mut state = self
                    .deliveries
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                match state.queue.pop_front() {
                    Some(delivery) => delivery,
                    None => {
                        state.draining = false;
                        return;
                    }
                }
            };
            match delivery {
                ObserverDelivery::Event(event) => self.observer.on_event(event),
                ObserverDelivery::Ready(public_key) => self.observer.on_ready(public_key),
                ObserverDelivery::Failed(failure) => self.observer.on_failed(failure),
                ObserverDelivery::Closed => self.observer.on_closed(),
            }
        }
    }
}

impl Drop for Nip46Connection {
    fn drop(&mut self) {
        self.close_inner();
    }
}

#[uniffi::export]
impl Nip46Connection {
    /// Idempotently end this connection and detach only its exact signer
    /// registration. An older session cannot remove a newer replacement.
    pub fn disconnect(&self) {
        self.close_inner();
    }

    /// Read out this session's checkpoint (#571): the minimum secrets and
    /// descriptor needed to reconnect without another pairing handshake.
    /// Refused with a typed error before this connection has reached ready
    /// (its signer attached to this engine) -- checkpointing a session that
    /// never authenticated would persist meaningless material.
    pub fn checkpoint(&self) -> Result<FfiNip46SessionCheckpoint, FfiNip46ProviderError> {
        let attachment = self
            .attachment
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !attachment.attached {
            return Err(FfiNip46ProviderError::InvalidSigner {
                reason: "NIP-46 connection has not reached ready".to_string(),
            });
        }
        let signer =
            attachment
                .signer
                .as_ref()
                .ok_or_else(|| FfiNip46ProviderError::InvalidSigner {
                    reason: "NIP-46 connection has no attached signer".to_string(),
                })?;
        Ok(checkpoint_to_ffi(signer.checkpoint()))
    }
}

#[uniffi::export]
impl FfiNip46Invitation {
    /// Produce the generic chooser URI or the app-specific launch URI for a
    /// catalog signer id such as `primal`.
    pub fn uri(&self, signer_id: Option<String>) -> Result<String, FfiNip46ProviderError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| FfiNip46ProviderError::InvalidSigner {
                reason: "NIP-46 invitation lock poisoned".to_string(),
            })?;
        let invitation = guard
            .as_ref()
            .ok_or_else(|| FfiNip46ProviderError::InvalidSigner {
                reason: "NIP-46 invitation was already consumed".to_string(),
            })?;
        let Some(signer_id) = signer_id else {
            return Ok(invitation.uri());
        };
        let app = nmp::known_nip46_signers()
            .iter()
            .find(|app| app.id == signer_id)
            .ok_or_else(|| FfiNip46ProviderError::InvalidSigner {
                reason: format!("unknown local signer id {signer_id:?}"),
            })?;
        let scheme =
            app.nip46_launch_scheme
                .ok_or_else(|| FfiNip46ProviderError::InvalidSigner {
                    reason: format!("local signer {signer_id:?} does not support NIP-46"),
                })?;
        invitation
            .uri_with_scheme(scheme)
            .map_err(|error| FfiNip46ProviderError::InvalidSigner {
                reason: error.to_string(),
            })
    }
}

#[uniffi::export]
pub fn nip46_signer_catalog() -> Vec<FfiNip46SignerApp> {
    nmp::known_nip46_signers()
        .iter()
        .map(|app| FfiNip46SignerApp {
            id: app.id.to_string(),
            display_name: app.display_name.to_string(),
            ios_detection_uri: app.ios_detection_uri.map(str::to_string),
            nip46_launch_scheme: app.nip46_launch_scheme.map(str::to_string),
            android_detection_uri: app.android_detection_uri.map(str::to_string),
            android_package_id: app.android_package_id.map(str::to_string),
        })
        .collect()
}

/// Verified carrier minted only by the four proof-first preparation doors.
/// Foreign copies share these same native Arcs; an install failure drops only
/// its local carrier reference and cannot disconnect another live owner.
#[derive(uniffi::Object)]
pub struct FfiNip46PreparedConnection {
    connection: Arc<Nip46Connection>,
    adapter: Arc<FfiSignerAdapter>,
}

#[uniffi::export]
impl FfiNip46PreparedConnection {
    /// Return the provider contribution with branded proof at input zero.
    pub fn adapter(&self, _compatibility: Arc<FfiNip46Compatibility>) -> Arc<FfiSignerAdapter> {
        Arc::clone(&self.adapter)
    }

    pub fn connection(&self) -> Arc<Nip46Connection> {
        Arc::clone(&self.connection)
    }
}

/// Exact standalone NIP-46 provider artifact identity.
pub const NMP_NIP46_COMPONENT_IDENTITY: &str = env!("NMP_NIP46_COMPONENT_IDENTITY");
/// Exact standalone core artifact identity sealed into this provider build.
pub const NMP_NIP46_REQUIRED_CORE_IDENTITY: &str = env!("NMP_NIP46_REQUIRED_CORE_IDENTITY");

/// Return the loaded NIP-46 library's identity as plain data.
#[uniffi::export]
pub fn nmp_nip46_component_identity() -> String {
    NMP_NIP46_COMPONENT_IDENTITY.to_owned()
}

/// Opaque proof that both package axes matched this provider build.
///
/// The proof is minted from plain interface/core identity text before the
/// caller requests an adapter. Every preparation door requires this proof as
/// input zero before it can return an external adapter.
#[derive(Debug, uniffi::Object)]
pub struct FfiNip46Compatibility {
    _private: (),
}

/// Verify provider binding/native, package interface, and loaded core before
/// any object is exchanged.
#[uniffi::export]
pub fn verify_nip46_component(
    packaged_provider_identity: String,
    loaded_provider_identity: String,
    packaged_interface_identity: String,
    loaded_core_identity: String,
) -> Result<Arc<FfiNip46Compatibility>, FfiNip46ProviderError> {
    if packaged_provider_identity != NMP_NIP46_COMPONENT_IDENTITY {
        return Err(FfiNip46ProviderError::ProviderBindingMismatch {
            expected: NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            actual: packaged_provider_identity,
        });
    }
    if loaded_provider_identity != NMP_NIP46_COMPONENT_IDENTITY {
        return Err(FfiNip46ProviderError::ProviderNativeMismatch {
            expected: NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            actual: loaded_provider_identity,
        });
    }
    if packaged_interface_identity != COMPONENT_INTERFACE_IDENTITY {
        return Err(FfiNip46ProviderError::PackageInterfaceMismatch {
            expected: COMPONENT_INTERFACE_IDENTITY.to_string(),
            actual: packaged_interface_identity,
        });
    }
    if loaded_core_identity != NMP_NIP46_REQUIRED_CORE_IDENTITY {
        return Err(FfiNip46ProviderError::CoreIdentityMismatch {
            expected: NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
            actual: loaded_core_identity,
        });
    }
    Ok(Arc::new(FfiNip46Compatibility { _private: () }))
}

#[uniffi::export]
pub fn nip46_invitation(
    _compatibility: Arc<FfiNip46Compatibility>,
    relays: Vec<String>,
    permissions: Option<String>,
    metadata: FfiNip46ClientMetadata,
) -> Result<Arc<FfiNip46Invitation>, FfiNip46ProviderError> {
    let relays = relays
        .into_iter()
        .map(|relay| {
            nmp::RelayUrl::parse(&relay).map_err(|_| FfiNip46ProviderError::InvalidRelay { relay })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let invitation = nmp::Nip46Invitation::new(
        relays,
        permissions,
        nmp::Nip46ClientMetadata {
            name: metadata.name,
            url: metadata.url,
            image: metadata.image,
        },
    )
    .map_err(|error| FfiNip46ProviderError::InvalidSigner {
        reason: error.to_string(),
    })?;
    Ok(Arc::new(FfiNip46Invitation {
        inner: Mutex::new(Some(invitation)),
    }))
}

#[uniffi::export]
pub fn prepare_nip46_bunker(
    _compatibility: Arc<FfiNip46Compatibility>,
    bunker_uri: String,
    timeout_millis: u64,
    observer: Box<dyn Nip46ConnectionObserver>,
) -> Result<Arc<FfiNip46PreparedConnection>, FfiNip46ProviderError> {
    Ok(prepare_connection(
        observer,
        move |connection, cancellation, runtime| {
            bunker_connection_task(
                connection,
                cancellation,
                bunker_uri,
                timeout_millis,
                runtime,
            )
        },
    ))
}

#[uniffi::export]
pub fn prepare_nip46_invitation(
    _compatibility: Arc<FfiNip46Compatibility>,
    invitation: Arc<FfiNip46Invitation>,
    timeout_millis: u64,
    observer: Box<dyn Nip46ConnectionObserver>,
) -> Result<Arc<FfiNip46PreparedConnection>, FfiNip46ProviderError> {
    let invitation = invitation
        .inner
        .lock()
        .map_err(|_| FfiNip46ProviderError::InvalidSigner {
            reason: "NIP-46 invitation lock poisoned".to_string(),
        })?
        .take()
        .ok_or_else(|| FfiNip46ProviderError::InvalidSigner {
            reason: "NIP-46 invitation was already consumed".to_string(),
        })?;
    Ok(prepare_connection(
        observer,
        move |connection, cancellation, runtime| {
            invitation_connection_task(
                connection,
                cancellation,
                invitation,
                timeout_millis,
                runtime,
            )
        },
    ))
}

#[uniffi::export]
pub fn prepare_nip46_restore(
    _compatibility: Arc<FfiNip46Compatibility>,
    checkpoint: FfiNip46SessionCheckpoint,
    timeout_millis: u64,
    observer: Box<dyn Nip46ConnectionObserver>,
) -> Result<Arc<FfiNip46PreparedConnection>, FfiNip46ProviderError> {
    let checkpoint = checkpoint_from_ffi(checkpoint)?;
    Ok(prepare_connection(
        observer,
        move |connection, cancellation, runtime| {
            from_parts_connection_task(
                connection,
                cancellation,
                checkpoint,
                timeout_millis,
                runtime,
            )
        },
    ))
}

fn prepare_connection<F>(
    observer: Box<dyn Nip46ConnectionObserver>,
    task: F,
) -> Arc<FfiNip46PreparedConnection>
where
    F: FnOnce(
            Weak<Nip46Connection>,
            nmp_signer::Nip46Cancellation,
            Arc<dyn nmp_signer::Nip46TaskRuntime>,
        ) -> ProviderAdapterTask
        + Send
        + 'static,
{
    let observer: Arc<dyn Nip46ConnectionObserver> = Arc::from(observer);
    let cancellation = nmp_signer::Nip46Cancellation::default();
    let cancel_on_drop = cancellation.clone();
    let (desired, changes) = tokio::sync::watch::channel(DesiredSignerState::Detached);
    let connection = Nip46Connection::new(desired, observer, cancellation.clone());
    let weak = Arc::downgrade(&connection);
    let adapter = new_signer_adapter(
        move || cancel_on_drop.cancel(),
        move |control, runtime| {
            let session_runtime: Arc<dyn nmp_signer::Nip46TaskRuntime> =
                Arc::new(ComponentNip46Runtime {
                    runtime: runtime.clone(),
                    connection: weak.clone(),
                });
            let provider = task(weak.clone(), cancellation, session_runtime);
            Box::pin(runtime.contextualize(async move {
                let (_, ()) =
                    tokio::join!(provider, drive_desired_signer_state(weak, control, changes));
            }))
        },
    );
    Arc::new(FfiNip46PreparedConnection {
        connection,
        adapter,
    })
}

async fn drive_desired_signer_state<S: DesiredSigner>(
    connection: Weak<Nip46Connection>,
    control: SignerAdapterControl,
    mut desired: tokio::sync::watch::Receiver<DesiredSignerState<S>>,
) {
    // The channel keeps only the newest level, so a `Detached` that arrives
    // between two `Attached` levels is dropped before this task observes it.
    // The core door refuses a second attach while a registration is live, and
    // that refusal is terminal for the connection, so track what was last
    // applied and replay the elided detach instead of driving the door into
    // its refusal. Tracking also stops a coalesced `Attached` from reporting a
    // detach that removed nothing.
    let mut attached = false;
    while desired.changed().await.is_ok() {
        let next = { desired.borrow_and_update().clone() };
        match next {
            DesiredSignerState::Detached => {
                if !attached {
                    continue;
                }
                attached = false;
                let result = control.detach().await;
                if let Some(connection) = connection.upgrade() {
                    connection.adapter_detached(result);
                }
            }
            DesiredSignerState::Attached(signer) => {
                if attached {
                    let result = control.detach().await;
                    let detached = result.is_ok();
                    attached = false;
                    if let Some(connection) = connection.upgrade() {
                        connection.adapter_detached(result);
                    }
                    if !detached {
                        // The connection has already failed on the detach
                        // error; attaching over a live registration would only
                        // add the core's refusal on top of it.
                        break;
                    }
                }
                let public_key = signer.public_key_hex();
                let result = signer.attach(&control).await;
                attached = result.is_ok();
                if let Some(connection) = connection.upgrade() {
                    connection.adapter_attached(public_key, result);
                }
            }
        }
    }
    if attached {
        let _ = control.detach().await;
    }
}

/// #704: run one NIP-46 connect handshake as an async task on the engine's
/// shared adapter `runtime` — NOT on a dedicated OS thread. The handshake's
/// availability wait is now awaited (see `Nip46Signer::*_observed_async`), so no
/// OS thread is held while the signer comes online. The capability-owned
/// contextual scheduler admits this task before the future runs, so the
/// provider has no independent spawn error to report. On completion the
/// connection attaches the signer or reports a typed failure.
async fn run_nip46_connect<F, Fut>(connection: Weak<Nip46Connection>, connect: F)
where
    F: FnOnce(Arc<dyn Fn(nmp::Nip46ConnectionEvent) + Send + Sync>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<nmp::Nip46Signer, nmp_signer::Nip46Error>> + Send,
{
    let events = lifecycle_sink(connection.clone());
    let result = connect(events).await;
    let Some(connection) = connection.upgrade() else {
        return;
    };
    match result {
        Ok(signer) => connection.attach(signer),
        Err(error) => connection.fail(nip46_failure_to_ffi(error)),
    }
}

fn bunker_connection_task(
    connection: Weak<Nip46Connection>,
    cancellation: nmp_signer::Nip46Cancellation,
    bunker_uri: String,
    timeout_millis: u64,
    session_runtime: Arc<dyn nmp_signer::Nip46TaskRuntime>,
) -> ProviderAdapterTask {
    Box::pin(async move {
        run_nip46_connect(connection, move |events| async move {
            nmp::Nip46Signer::connect_bunker_observed_async(
                &bunker_uri,
                None,
                nmp::Nip46ClientMetadata::default(),
                Duration::from_millis(timeout_millis),
                events,
                &cancellation,
                session_runtime,
            )
            .await
        })
        .await;
    })
}

fn invitation_connection_task(
    connection: Weak<Nip46Connection>,
    cancellation: nmp_signer::Nip46Cancellation,
    invitation: nmp::Nip46Invitation,
    timeout_millis: u64,
    session_runtime: Arc<dyn nmp_signer::Nip46TaskRuntime>,
) -> ProviderAdapterTask {
    Box::pin(async move {
        run_nip46_connect(connection, move |events| async move {
            invitation
                .connect_observed_async(
                    Duration::from_millis(timeout_millis),
                    events,
                    &cancellation,
                    session_runtime,
                )
                .await
        })
        .await;
    })
}

fn from_parts_connection_task(
    connection: Weak<Nip46Connection>,
    cancellation: nmp_signer::Nip46Cancellation,
    checkpoint: nmp_signer::Nip46SessionCheckpoint,
    timeout_millis: u64,
    session_runtime: Arc<dyn nmp_signer::Nip46TaskRuntime>,
) -> ProviderAdapterTask {
    // #704: the restore path is engine-associated too — the session runs its
    // tasks on the shared adapter runtime, not a standalone runtime.
    Box::pin(async move {
        run_nip46_connect(connection, move |events| async move {
            nmp::Nip46Signer::from_parts_observed_async(
                checkpoint,
                Duration::from_millis(timeout_millis),
                events,
                &cancellation,
                session_runtime,
            )
            .await
        })
        .await;
    })
}

fn lifecycle_sink(
    connection: Weak<Nip46Connection>,
) -> Arc<dyn Fn(nmp::Nip46ConnectionEvent) + Send + Sync> {
    Arc::new(move |event| {
        if let Some(connection) = connection.upgrade() {
            connection.on_event(event);
        }
    })
}

fn event_to_ffi(event: nmp::Nip46ConnectionEvent) -> FfiNip46ConnectionEvent {
    match event {
        nmp::Nip46ConnectionEvent::Connecting => FfiNip46ConnectionEvent::Connecting,
        nmp::Nip46ConnectionEvent::Available => FfiNip46ConnectionEvent::Available,
        nmp::Nip46ConnectionEvent::Unavailable => FfiNip46ConnectionEvent::Unavailable,
        nmp::Nip46ConnectionEvent::RelayAuthentication(relay) => {
            FfiNip46ConnectionEvent::RelayAuthentication {
                relay: relay.to_string(),
            }
        }
        nmp::Nip46ConnectionEvent::AuthorizationRequired(url) => {
            FfiNip46ConnectionEvent::AuthorizationRequired { url }
        }
        nmp::Nip46ConnectionEvent::Connected { user_public_key } => {
            FfiNip46ConnectionEvent::Connected {
                user_public_key: user_public_key.to_hex(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::thread;

    use nmp_component_interface::SignerAdapterCommand;
    use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
    use nostr::Keys;
    use tokio::sync::oneshot;

    struct CloseCountingObserver {
        closed: Arc<AtomicUsize>,
    }

    #[test]
    fn exact_package_axes_mint_compatibility_and_mismatches_are_typed() {
        assert!(verify_nip46_component(
            NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            COMPONENT_INTERFACE_IDENTITY.to_string(),
            NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
        )
        .is_ok());

        assert_eq!(
            verify_nip46_component(
                "mismatched-binding".to_string(),
                NMP_NIP46_COMPONENT_IDENTITY.to_string(),
                COMPONENT_INTERFACE_IDENTITY.to_string(),
                NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
            )
            .expect_err("a different packaged provider binding must be refused"),
            FfiNip46ProviderError::ProviderBindingMismatch {
                expected: NMP_NIP46_COMPONENT_IDENTITY.to_string(),
                actual: "mismatched-binding".to_string(),
            }
        );
        assert_eq!(
            verify_nip46_component(
                NMP_NIP46_COMPONENT_IDENTITY.to_string(),
                "mismatched-native".to_string(),
                COMPONENT_INTERFACE_IDENTITY.to_string(),
                NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
            )
            .expect_err("a different loaded provider native must be refused"),
            FfiNip46ProviderError::ProviderNativeMismatch {
                expected: NMP_NIP46_COMPONENT_IDENTITY.to_string(),
                actual: "mismatched-native".to_string(),
            }
        );
        assert_eq!(
            verify_nip46_component(
                NMP_NIP46_COMPONENT_IDENTITY.to_string(),
                NMP_NIP46_COMPONENT_IDENTITY.to_string(),
                "mismatched-interface".to_string(),
                NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
            )
            .expect_err("a different packaged interface must be refused"),
            FfiNip46ProviderError::PackageInterfaceMismatch {
                expected: COMPONENT_INTERFACE_IDENTITY.to_string(),
                actual: "mismatched-interface".to_string(),
            }
        );
        assert_eq!(
            verify_nip46_component(
                NMP_NIP46_COMPONENT_IDENTITY.to_string(),
                NMP_NIP46_COMPONENT_IDENTITY.to_string(),
                COMPONENT_INTERFACE_IDENTITY.to_string(),
                "mismatched-core".to_string(),
            )
            .expect_err("a different loaded core identity must be refused"),
            FfiNip46ProviderError::CoreIdentityMismatch {
                expected: NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
                actual: "mismatched-core".to_string(),
            }
        );
    }

    #[test]
    fn every_component_attach_failure_keeps_its_typed_discriminant() {
        assert_eq!(
            engine_attach_failure_to_ffi(ComponentInterfaceError::EngineClosed),
            FfiNip46Failure::Disconnected
        );
        assert_eq!(
            engine_attach_failure_to_ffi(ComponentInterfaceError::SignerMissingPublicKey),
            FfiNip46Failure::SignerMissingPublicKey
        );
        assert_eq!(
            engine_attach_failure_to_ffi(ComponentInterfaceError::CapabilityRegistryFull {
                limit: 19,
            }),
            FfiNip46Failure::CapabilityRegistryFull { limit: 19 }
        );
        assert_eq!(
            engine_attach_failure_to_ffi(ComponentInterfaceError::CapabilityInstanceExhausted),
            FfiNip46Failure::CapabilityInstanceExhausted
        );
        assert_eq!(
            engine_attach_failure_to_ffi(ComponentInterfaceError::AdapterClosed),
            FfiNip46Failure::AdapterClosed
        );
        assert_eq!(
            engine_attach_failure_to_ffi(ComponentInterfaceError::CoreRefused {
                reason: "exact refusal".to_string(),
            }),
            FfiNip46Failure::CoreRefused {
                reason: "exact refusal".to_string(),
            }
        );
    }

    impl Nip46ConnectionObserver for CloseCountingObserver {
        fn on_event(&self, _event: FfiNip46ConnectionEvent) {}

        fn on_ready(&self, _user_public_key: String) {}

        fn on_failed(&self, _failure: FfiNip46Failure) {}

        fn on_closed(&self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ReentrantObserver {
        deliveries: Arc<Mutex<Vec<&'static str>>>,
        connection: Mutex<Weak<Nip46Connection>>,
    }

    struct TerminalObserver {
        failed: Mutex<Option<mpsc::Sender<FfiNip46Failure>>>,
        closed: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl Nip46ConnectionObserver for TerminalObserver {
        fn on_event(&self, _event: FfiNip46ConnectionEvent) {}

        fn on_ready(&self, _user_public_key: String) {}

        fn on_failed(&self, failure: FfiNip46Failure) {
            if let Some(sender) = self
                .failed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
            {
                let _ = sender.send(failure);
            }
        }

        fn on_closed(&self) {
            if let Some(sender) = self
                .closed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
            {
                let _ = sender.send(());
            }
        }
    }

    impl Nip46ConnectionObserver for ReentrantObserver {
        fn on_event(&self, _event: FfiNip46ConnectionEvent) {
            self.deliveries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push("event");
        }

        fn on_ready(&self, _user_public_key: String) {
            self.deliveries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push("ready");
            if let Some(connection) = self
                .connection
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .upgrade()
            {
                connection.disconnect();
            }
        }

        fn on_failed(&self, _failure: FfiNip46Failure) {
            self.deliveries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push("failed");
        }

        fn on_closed(&self) {
            self.deliveries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push("closed");
        }
    }

    fn test_connection(observer: Arc<dyn Nip46ConnectionObserver>) -> Arc<Nip46Connection> {
        let (desired, _changes) = tokio::sync::watch::channel(DesiredSignerState::Detached);
        Nip46Connection::new(desired, observer, nmp_signer::Nip46Cancellation::default())
    }

    #[test]
    fn catalog_keeps_probe_launch_package_and_provider_distinct() {
        let primal = nip46_signer_catalog()
            .into_iter()
            .find(|app| app.id == "primal")
            .unwrap();
        assert_eq!(
            primal.ios_detection_uri.as_deref(),
            Some("primalconnect://probe")
        );
        assert_eq!(primal.nip46_launch_scheme.as_deref(), Some("primalconnect"));
        assert_eq!(
            primal.android_detection_uri.as_deref(),
            Some("primal://signer")
        );
        assert_eq!(
            primal.android_package_id.as_deref(),
            Some("net.primal.android")
        );
    }

    #[test]
    fn connection_close_and_drop_are_idempotent_and_stream_scoped() {
        let closed_a = Arc::new(AtomicUsize::new(0));
        let closed_b = Arc::new(AtomicUsize::new(0));
        let connection_a = test_connection(Arc::new(CloseCountingObserver {
            closed: Arc::clone(&closed_a),
        }));
        let connection_b = test_connection(Arc::new(CloseCountingObserver {
            closed: Arc::clone(&closed_b),
        }));

        connection_a.disconnect();
        connection_a.disconnect();
        assert_eq!(closed_a.load(Ordering::SeqCst), 1);
        assert_eq!(closed_b.load(Ordering::SeqCst), 0);
        drop(connection_a);
        assert_eq!(closed_a.load(Ordering::SeqCst), 1);

        connection_b.disconnect();
        assert_eq!(closed_b.load(Ordering::SeqCst), 1);
        drop(connection_b);
        assert_eq!(closed_b.load(Ordering::SeqCst), 1);
    }

    /// #571: a real `Nip46Connection` that has never attached a signer
    /// (never reached ready) refuses `checkpoint()` with a typed error --
    /// distinct from the Swift/Kotlin wrapper's own nil-underlying-
    /// connection guard, this exercises the actual FFI-level
    /// `attachment.attached == false` refusal this method's doc
    /// documents.
    #[test]
    fn checkpoint_before_ready_is_refused_at_the_ffi_boundary() {
        let closed = Arc::new(AtomicUsize::new(0));
        let connection = test_connection(Arc::new(CloseCountingObserver { closed }));

        assert!(matches!(
            connection.checkpoint(),
            Err(FfiNip46ProviderError::InvalidSigner { .. })
        ));

        connection.disconnect();
    }

    #[test]
    fn observer_delivery_is_reentrant_and_closed_is_terminal() {
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let observer = Arc::new(ReentrantObserver {
            deliveries: Arc::clone(&deliveries),
            connection: Mutex::new(Weak::new()),
        });
        let connection = test_connection(observer.clone());
        *observer
            .connection
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Arc::downgrade(&connection);

        let should_drain =
            connection.enqueue_delivery(ObserverDelivery::Ready("user-key".to_string()));
        connection.drain_deliveries(should_drain);
        let after_closed = connection
            .enqueue_delivery(ObserverDelivery::Event(FfiNip46ConnectionEvent::Connecting));
        connection.drain_deliveries(after_closed);

        assert_eq!(
            *deliveries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
            vec!["ready", "closed"],
            "a reentrant close is ordered after the active callback and seals the stream"
        );
        connection.disconnect();
    }

    #[test]
    fn unavailable_before_attach_is_retained_as_attachment_state() {
        let connection = test_connection(Arc::new(CloseCountingObserver {
            closed: Arc::new(AtomicUsize::new(0)),
        }));

        connection.on_event(nmp::Nip46ConnectionEvent::Available);
        connection.on_event(nmp::Nip46ConnectionEvent::Unavailable);

        let attachment = connection
            .attachment
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert!(!attachment.available);
        assert!(!attachment.attached);
        drop(attachment);
        connection.disconnect();
    }

    #[test]
    fn provider_task_uses_explicit_core_runtime_and_completes_without_panic() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the provider task is deliberately constructed outside a Tokio context"
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = format!("ws://{}", listener.local_addr().unwrap());
        let remote = Keys::generate();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            accepted_tx.send(()).unwrap();
            while socket.read().is_ok() {}
            closed_tx.send(()).unwrap();
        });
        let engine = NmpEngine::new(NmpEngineConfig::default()).unwrap();
        let (failed_tx, failed_rx) = mpsc::channel();
        let (terminal_closed_tx, terminal_closed_rx) = mpsc::channel();
        let uri = format!(
            "bunker://{}?relay={}&secret=explicit-runtime",
            remote.public_key().to_hex(),
            url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
        );
        let prepared = prepare_connection(
            Box::new(TerminalObserver {
                failed: Mutex::new(Some(failed_tx)),
                closed: Mutex::new(Some(terminal_closed_tx)),
            }),
            move |connection, cancellation, runtime| {
                bunker_connection_task(connection, cancellation, uri, 100, runtime)
            },
        );
        let compatibility = verify_nip46_component(
            NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            COMPONENT_INTERFACE_IDENTITY.to_string(),
            NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
        )
        .unwrap();
        let installation = engine
            .install_signer_adapter(prepared.adapter(compatibility))
            .expect("the core installs the provider task");
        accepted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the explicitly scheduled task opens its socket");
        assert_eq!(
            failed_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("the provider task reports its terminal result"),
            FfiNip46Failure::Timeout
        );
        terminal_closed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the provider lifecycle completes after the typed failure");
        closed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("provider task completion closes its socket");
        assert!(installation.uninstall());
        engine.shutdown();
    }

    #[test]
    fn pending_handshake_worker_does_not_retain_dropped_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = format!("ws://{}", listener.local_addr().unwrap());
        let remote = Keys::generate();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            accepted_tx.send(()).unwrap();
            while socket.read().is_ok() {}
            closed_tx.send(()).unwrap();
        });

        let engine = NmpEngine::new(NmpEngineConfig::default()).unwrap();
        let closed = Arc::new(AtomicUsize::new(0));
        let uri = format!(
            "bunker://{}?relay={}&secret=pending-drop",
            remote.public_key().to_hex(),
            url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
        );
        let prepared = prepare_connection(
            Box::new(CloseCountingObserver {
                closed: Arc::clone(&closed),
            }),
            move |connection, cancellation, runtime| {
                bunker_connection_task(connection, cancellation, uri, 60_000, runtime)
            },
        );
        let compatibility = verify_nip46_component(
            NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            COMPONENT_INTERFACE_IDENTITY.to_string(),
            NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
        )
        .unwrap();
        let adapter = prepared.adapter(compatibility);
        let installation = engine
            .install_signer_adapter(adapter)
            .expect("the core installs the prepared provider task");
        let connection = prepared.connection();
        let weak = Arc::downgrade(&connection);
        accepted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the pending handshake opens its socket");

        drop(connection);
        drop(prepared);

        assert!(
            weak.upgrade().is_none(),
            "the worker owns no strong connection Arc"
        );
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        closed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("connection drop cancels the pending handshake socket");
        installation.uninstall();
        engine.shutdown();
    }

    #[test]
    fn prepared_alias_replay_cannot_start_twice_or_disconnect_first_owner() {
        let engine = NmpEngine::new(NmpEngineConfig::default()).unwrap();
        let closed = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(AtomicUsize::new(0));
        let counted_starts = Arc::clone(&starts);
        let prepared = prepare_connection(
            Box::new(CloseCountingObserver {
                closed: Arc::clone(&closed),
            }),
            move |_connection, _cancellation, _runtime| {
                counted_starts.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::pending())
            },
        );
        let failed_alias = Arc::clone(&prepared);
        let compatibility = verify_nip46_component(
            NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            NMP_NIP46_COMPONENT_IDENTITY.to_string(),
            COMPONENT_INTERFACE_IDENTITY.to_string(),
            NMP_NIP46_REQUIRED_CORE_IDENTITY.to_string(),
        )
        .unwrap();

        assert_eq!(
            starts.load(Ordering::SeqCst),
            0,
            "preparation alone cannot invoke the provider task factory"
        );
        let installation = engine
            .install_signer_adapter(prepared.adapter(Arc::clone(&compatibility)))
            .expect("first prepared carrier installs");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            engine.install_signer_adapter(failed_alias.adapter(compatibility)),
            Err(nmp_ffi::signer::FfiSignerAdapterInstallError::AdapterAlreadyTaken)
        ));
        assert_eq!(starts.load(Ordering::SeqCst), 1);

        drop(failed_alias);
        assert!(
            !prepared.connection.closed.load(Ordering::Acquire),
            "dropping the failed alias cannot close the first retained carrier"
        );
        assert_eq!(closed.load(Ordering::SeqCst), 0);

        assert!(installation.uninstall());
        drop(prepared);
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    /// How long a command is waited for before the driver is declared hung.
    /// The driver does no I/O here, so this is a liveness backstop, never a
    /// performance assertion.
    const OBSERVATION: Duration = Duration::from_secs(10);
    /// How long the driver is watched for a command it must NOT emit.
    const QUIET: Duration = Duration::from_millis(250);
    /// Backstop for the whole driver/script pair, so a driver that never ends
    /// fails the test instead of hanging it.
    const COMPLETION: Duration = Duration::from_secs(60);

    /// Desired-state payload standing in for a connected `Nip46Signer`. A real
    /// one only exists after a full relay handshake, which is not reachable
    /// from a unit test; nothing below depends on any signing behaviour, only
    /// on the level sequence the driver is handed.
    #[derive(Clone)]
    struct TestSigner {
        public_key: &'static str,
    }

    impl ::nmp::SigningCapability for TestSigner {
        fn public_key(&self) -> Option<::nmp::SignerPublicKey> {
            None
        }

        fn sign(
            &self,
            _unsigned: ::nmp::SignerUnsignedEvent,
        ) -> ::nmp::SignerOp<::nmp::SignerSignedEvent> {
            unimplemented!("the desired-state driver never signs")
        }
    }

    impl DesiredSigner for TestSigner {
        fn public_key_hex(&self) -> String {
            self.public_key.to_string()
        }

        fn attach<'a>(
            self: Box<Self>,
            control: &'a SignerAdapterControl,
        ) -> Pin<Box<dyn Future<Output = Result<(), ComponentInterfaceError>> + Send + 'a>>
        {
            Box::pin(control.attach_boxed(self))
        }
    }

    fn command_name(command: &SignerAdapterCommand) -> &'static str {
        match command {
            SignerAdapterCommand::Attach { .. } => "Attach",
            SignerAdapterCommand::Detach { .. } => "Detach",
        }
    }

    type Acknowledgement = oneshot::Sender<Result<(), ComponentInterfaceError>>;

    /// `drive_desired_signer_state` running against a command receiver this
    /// test owns, so both the exact command sequence and the acknowledgement
    /// timing are observable. The control is the real one -- it is minted by
    /// `new_signer_adapter` and handed to the task factory exactly as the core
    /// installation path does -- so no test-only constructor is needed.
    struct DriverHarness {
        commands: tokio::sync::mpsc::Receiver<SignerAdapterCommand>,
        desired: Option<tokio::sync::watch::Sender<DesiredSignerState<TestSigner>>>,
        _connection: Arc<Nip46Connection>,
        closed: Arc<AtomicUsize>,
    }

    /// Build the driver task and the levers that drive it. The runtime is the
    /// one the `#[tokio::test]` attribute already built and entered: this crate
    /// is the separately linked provider, so it mints no runtime of its own and
    /// spawns nothing -- the driver is joined into the test's own task below.
    fn start_driver() -> (DriverHarness, ProviderAdapterTask) {
        let closed = Arc::new(AtomicUsize::new(0));
        let connection = test_connection(Arc::new(CloseCountingObserver {
            closed: Arc::clone(&closed),
        }));
        let weak = Arc::downgrade(&connection);
        let (desired, changes) =
            tokio::sync::watch::channel(DesiredSignerState::<TestSigner>::Detached);
        let adapter = new_signer_adapter(
            || {},
            move |control, _runtime| Box::pin(drive_desired_signer_state(weak, control, changes)),
        );
        let started = adapter
            .take_for_install()
            .expect("a fresh adapter still owns its parts")
            .start(
                tokio::runtime::Handle::try_current()
                    .expect("the test attribute has already entered its runtime"),
            );
        (
            DriverHarness {
                commands: started.commands,
                desired: Some(desired),
                _connection: connection,
                closed,
            },
            started.task,
        )
    }

    impl DriverHarness {
        fn set(&self, state: DesiredSignerState<TestSigner>) {
            self.desired
                .as_ref()
                .expect("the desired-state sender is still live")
                .send_replace(state);
        }

        fn close_desired(&mut self) {
            self.desired.take();
        }

        async fn recv_within(
            &mut self,
            budget: Duration,
        ) -> Result<Option<SignerAdapterCommand>, ()> {
            tokio::time::timeout(budget, self.commands.recv())
                .await
                .map_err(|_| ())
        }

        async fn next_command(&mut self, expected: &str) -> SignerAdapterCommand {
            match self.recv_within(OBSERVATION).await {
                Ok(Some(command)) => command,
                Ok(None) => panic!("expected {expected}; the driver ended instead"),
                Err(_) => {
                    panic!("expected {expected}; the driver emitted nothing in {OBSERVATION:?}")
                }
            }
        }

        async fn expect_attach(&mut self, expected: &str) -> Acknowledgement {
            match self.next_command(expected).await {
                SignerAdapterCommand::Attach { reply, .. } => reply,
                SignerAdapterCommand::Detach { .. } => {
                    panic!("expected {expected}; the driver emitted Detach")
                }
            }
        }

        async fn expect_detach(&mut self, expected: &str) -> Acknowledgement {
            match self.next_command(expected).await {
                SignerAdapterCommand::Detach { reply } => reply,
                SignerAdapterCommand::Attach { .. } => {
                    panic!("expected {expected}; the driver emitted Attach")
                }
            }
        }

        /// Give the driver a real chance to run and assert it commanded
        /// nothing. Also the yield point that lets it observe a level.
        async fn assert_quiet(&mut self, why: &str) {
            match self.recv_within(QUIET).await {
                Err(_) => {}
                Ok(Some(command)) => panic!(
                    "the driver emitted {} when it must be quiet: {why}",
                    command_name(&command)
                ),
                Ok(None) => panic!("the driver ended early when it must be quiet: {why}"),
            }
        }
    }

    /// Run the driver concurrently with the level script inside this one test
    /// task and require both to finish. Joining rather than spawning keeps the
    /// provider source free of any runtime authority of its own; the outer
    /// budget turns a driver that never ends into a failure rather than a hang.
    async fn drive_to_completion(driver: ProviderAdapterTask, script: impl Future<Output = ()>) {
        tokio::time::timeout(COMPLETION, async move {
            tokio::join!(driver, script);
        })
        .await
        .expect("the driver and its level script both run to completion");
    }

    /// #952 falsifier. `tokio::sync::watch` keeps only the newest level, so a
    /// `Detached` arriving between two `Attached` levels -- exactly what a
    /// relay flap produces while an Attach is still in flight -- is coalesced
    /// away and never observed. The core attach door hard-refuses a second
    /// attach while a registration is live, and that refusal is terminal for
    /// the connection, so a level-triggered driver would kill a live session
    /// on a mere flap. The observed sequence must be Attach, Detach, Attach --
    /// never Attach, Attach.
    #[tokio::test(flavor = "current_thread")]
    async fn coalesced_detach_is_replayed_before_attaching_over_a_live_registration() {
        let (mut harness, driver) = start_driver();
        let script = async {
            harness.set(DesiredSignerState::Attached(Box::new(TestSigner {
                public_key: "signer-a",
            })));
            let first = harness.expect_attach("the first Attach").await;

            // The first Attach is still in flight: in production this window is
            // an mpsc hop plus a blocking engine round trip. Both levels
            // therefore land in the watch cell before the driver can observe
            // either, and the `Detached` is coalesced away.
            harness.set(DesiredSignerState::Detached);
            harness.set(DesiredSignerState::Attached(Box::new(TestSigner {
                public_key: "signer-b",
            })));
            harness
                .assert_quiet(
                    "no level may be applied while the driver's own Attach is unacknowledged",
                )
                .await;

            first
                .send(Ok(()))
                .expect("the driver is awaiting the first acknowledgement");

            let replayed = match harness.next_command("the replayed Detach").await {
                SignerAdapterCommand::Detach { reply } => reply,
                SignerAdapterCommand::Attach { .. } => panic!(
                    "the driver issued a second Attach while its own registration was still \
                     live: the coalesced Detached level was never replayed, so the core attach \
                     door refuses with CoreRefused and terminally fails a session that only saw \
                     a relay flap"
                ),
            };
            replayed
                .send(Ok(()))
                .expect("the driver is awaiting the replayed detach acknowledgement");

            harness
                .expect_attach("the second Attach, after the replayed Detach")
                .await
                .send(Ok(()))
                .expect("the driver is awaiting the second attach acknowledgement");

            harness
                .assert_quiet("every coalesced level has been applied")
                .await;
            assert_eq!(
                harness.closed.load(Ordering::SeqCst),
                0,
                "a coalesced flap must not terminate the connection"
            );

            // The registration is genuinely live, so ending the loop must
            // remove it.
            harness.close_desired();
            harness
                .expect_detach("the cleanup Detach for the live registration")
                .await
                .send(Ok(()))
                .expect("the driver is awaiting the cleanup acknowledgement");
        };
        drive_to_completion(driver, script).await;
    }

    /// #952 falsifier, spurious-detach half. A `Detached` level with nothing
    /// attached removes nothing: it must issue no command inline, and ending
    /// the loop with nothing attached must issue no cleanup command either. A
    /// detach that removes nothing still reports `Unavailable` to the observer
    /// and burns the single-slot adapter lane.
    #[tokio::test(flavor = "current_thread")]
    async fn detached_level_with_nothing_attached_never_commands_the_core() {
        let (mut harness, driver) = start_driver();
        let script = async {
            harness.set(DesiredSignerState::Detached);
            harness
                .assert_quiet("nothing is attached, so there is no registration to remove")
                .await;

            harness.close_desired();
            match harness.recv_within(OBSERVATION).await {
                // The driver dropped its control, which is the only way this
                // channel closes: it ran to completion having commanded
                // nothing.
                Ok(None) => {}
                Ok(Some(command)) => panic!(
                    "the driver emitted {} with nothing attached",
                    command_name(&command)
                ),
                Err(_) => panic!("the driver never finished after its sender was dropped"),
            }
            assert_eq!(
                harness.closed.load(Ordering::SeqCst),
                0,
                "no command means no reported failure"
            );
        };
        drive_to_completion(driver, script).await;
    }
}
