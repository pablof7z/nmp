//! Native signer discovery and NIP-46 connection projection.
//!
//! Rust owns catalog/protocol/lifecycle policy. Native shells only execute
//! the supplied OS probe/launch URI and render these bounded progress facts.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use crate::convert::FfiError;
use crate::facade::NmpEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiLocalSignerProtocol {
    Nip46,
    Nip55,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiLocalSignerApp {
    pub id: String,
    pub display_name: String,
    pub protocols: Vec<FfiLocalSignerProtocol>,
    pub ios_detection_uri: Option<String>,
    pub nip46_launch_scheme: Option<String>,
    pub android_detection_uri: Option<String>,
    pub android_package_id: Option<String>,
    pub android_provider_authority: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct FfiNip46ClientMetadata {
    pub name: Option<String>,
    pub url: Option<String>,
    pub image: Option<String>,
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

#[uniffi::export(callback_interface)]
pub trait Nip46ConnectionObserver: Send + Sync {
    fn on_event(&self, event: FfiNip46ConnectionEvent);
    /// The relay handshake is complete and the signer has been attached to
    /// this engine. A callback/deep-link alone never produces this fact.
    fn on_ready(&self, user_public_key: String);
    fn on_failed(&self, reason: String);
    fn on_closed(&self);
}

#[derive(uniffi::Object)]
pub struct FfiNip46Invitation {
    inner: Mutex<Option<nmp::Nip46Invitation>>,
}

struct Nip46Attachment {
    signer: Option<nmp::Nip46Signer>,
    registration: Option<nmp::SignerRegistration>,
    available: bool,
}

enum ObserverDelivery {
    Event(FfiNip46ConnectionEvent),
    Ready(String),
    Failed(String),
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
    engine: Arc<nmp::Engine>,
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
    fn new(engine: Arc<nmp::Engine>, observer: Arc<dyn Nip46ConnectionObserver>) -> Arc<Self> {
        Arc::new(Self {
            engine,
            observer,
            lifecycle: Mutex::new(()),
            deliveries: Mutex::new(ObserverDeliveryState::default()),
            attachment: Mutex::new(Nip46Attachment {
                signer: None,
                registration: None,
                available: false,
            }),
            cancellation: nmp_signer::Nip46Cancellation::default(),
            closed: AtomicBool::new(false),
        })
    }

    fn on_event(&self, event: nmp::Nip46ConnectionEvent) {
        let should_drain = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            let mut reattached_public_key = None;
            let mut failure = None;
            match &event {
                nmp::Nip46ConnectionEvent::Available => {
                    let mut attachment = self
                        .attachment
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    attachment.available = true;
                    if attachment.registration.is_none() {
                        if let Some(signer) = attachment.signer.clone() {
                            match self.engine.add_signer(signer) {
                                Ok(registration) => {
                                    reattached_public_key = Some(registration.public_key());
                                    attachment.registration = Some(registration);
                                }
                                Err(error) => failure = Some(error.to_string()),
                            }
                        }
                    }
                }
                nmp::Nip46ConnectionEvent::Unavailable => {
                    let registration = {
                        let mut attachment = self
                            .attachment
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        attachment.available = false;
                        attachment.registration.take()
                    };
                    if let Some(registration) = registration {
                        let _ = self.engine.remove_signer(registration);
                    }
                }
                _ => {}
            }
            if let Some(reason) = failure {
                self.fail_locked(reason)
            } else {
                let mut should_drain =
                    self.enqueue_delivery(ObserverDelivery::Event(event_to_ffi(event)));
                if let Some(public_key) = reattached_public_key {
                    should_drain |=
                        self.enqueue_delivery(ObserverDelivery::Ready(public_key.to_hex()));
                }
                should_drain
            }
        };
        self.drain_deliveries(should_drain);
    }

    fn attach(&self, signer: nmp::Nip46Signer) {
        let should_drain = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let pubkey = signer.user_public_key();
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
            match self.engine.add_signer(signer) {
                Ok(registration) => {
                    attachment.registration = Some(registration);
                    drop(attachment);
                    self.enqueue_delivery(ObserverDelivery::Ready(pubkey.to_hex()))
                }
                Err(error) => {
                    drop(attachment);
                    self.fail_locked(error.to_string())
                }
            }
        };
        self.drain_deliveries(should_drain);
    }

    fn fail(&self, reason: String) {
        let should_drain = {
            let _lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            self.fail_locked(reason)
        };
        self.drain_deliveries(should_drain);
    }

    fn fail_locked(&self, reason: String) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut should_drain = self.enqueue_delivery(ObserverDelivery::Failed(reason));
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
        let registration = {
            let mut attachment = self
                .attachment
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            attachment.signer = None;
            attachment.registration.take()
        };
        if let Some(registration) = registration {
            let _ = self.engine.remove_signer(registration);
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
                ObserverDelivery::Failed(reason) => self.observer.on_failed(reason),
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
}

#[uniffi::export]
impl FfiNip46Invitation {
    /// Produce the generic chooser URI or the app-specific launch URI for a
    /// catalog signer id such as `primal`.
    pub fn uri(&self, signer_id: Option<String>) -> Result<String, FfiError> {
        let guard = self.inner.lock().map_err(|_| FfiError::InvalidSigner {
            reason: "NIP-46 invitation lock poisoned".to_string(),
        })?;
        let invitation = guard.as_ref().ok_or_else(|| FfiError::InvalidSigner {
            reason: "NIP-46 invitation was already consumed".to_string(),
        })?;
        let Some(signer_id) = signer_id else {
            return Ok(invitation.uri());
        };
        let app = nmp::known_local_signers()
            .iter()
            .find(|app| app.id == signer_id)
            .ok_or_else(|| FfiError::InvalidSigner {
                reason: format!("unknown local signer id {signer_id:?}"),
            })?;
        let scheme = app
            .nip46_launch_scheme
            .ok_or_else(|| FfiError::InvalidSigner {
                reason: format!("local signer {signer_id:?} does not support NIP-46"),
            })?;
        invitation
            .uri_with_scheme(scheme)
            .map_err(|error| FfiError::InvalidSigner {
                reason: error.to_string(),
            })
    }
}

#[uniffi::export]
pub fn local_signer_catalog() -> Vec<FfiLocalSignerApp> {
    nmp::known_local_signers()
        .iter()
        .map(|app| FfiLocalSignerApp {
            id: app.id.to_string(),
            display_name: app.display_name.to_string(),
            protocols: app
                .protocols
                .iter()
                .map(|protocol| match protocol {
                    nmp::LocalSignerProtocol::Nip46 => FfiLocalSignerProtocol::Nip46,
                    nmp::LocalSignerProtocol::Nip55 => FfiLocalSignerProtocol::Nip55,
                })
                .collect(),
            ios_detection_uri: app.ios_detection_uri.map(str::to_string),
            nip46_launch_scheme: app.nip46_launch_scheme.map(str::to_string),
            android_detection_uri: app.android_detection_uri.map(str::to_string),
            android_package_id: app.android_package_id.map(str::to_string),
            android_provider_authority: app.android_provider_authority.map(str::to_string),
        })
        .collect()
}

#[uniffi::export]
impl NmpEngine {
    pub fn nip46_invitation(
        &self,
        relays: Vec<String>,
        permissions: Option<String>,
        metadata: FfiNip46ClientMetadata,
    ) -> Result<Arc<FfiNip46Invitation>, FfiError> {
        let relays = relays
            .into_iter()
            .map(|relay| {
                nmp::RelayUrl::parse(&relay).map_err(|_| FfiError::InvalidSigner {
                    reason: format!("invalid NIP-46 relay {relay:?}"),
                })
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
        .map_err(|error| FfiError::InvalidSigner {
            reason: error.to_string(),
        })?;
        Ok(Arc::new(FfiNip46Invitation {
            inner: Mutex::new(Some(invitation)),
        }))
    }

    pub fn connect_nip46_bunker(
        &self,
        bunker_uri: String,
        timeout_millis: u64,
        observer: Box<dyn Nip46ConnectionObserver>,
    ) -> Arc<Nip46Connection> {
        let engine = Arc::clone(&self.engine);
        let observer: Arc<dyn Nip46ConnectionObserver> = Arc::from(observer);
        let connection = Nip46Connection::new(engine, observer);
        spawn_bunker_connection(
            Arc::downgrade(&connection),
            connection.cancellation.clone(),
            bunker_uri,
            timeout_millis,
        );
        connection
    }

    pub fn connect_nip46_invitation(
        &self,
        invitation: Arc<FfiNip46Invitation>,
        timeout_millis: u64,
        observer: Box<dyn Nip46ConnectionObserver>,
    ) -> Result<Arc<Nip46Connection>, FfiError> {
        let invitation = invitation
            .inner
            .lock()
            .map_err(|_| FfiError::InvalidSigner {
                reason: "NIP-46 invitation lock poisoned".to_string(),
            })?
            .take()
            .ok_or_else(|| FfiError::InvalidSigner {
                reason: "NIP-46 invitation was already consumed".to_string(),
            })?;
        let engine = Arc::clone(&self.engine);
        let observer: Arc<dyn Nip46ConnectionObserver> = Arc::from(observer);
        let connection = Nip46Connection::new(engine, observer);
        spawn_invitation_connection(
            Arc::downgrade(&connection),
            connection.cancellation.clone(),
            invitation,
            timeout_millis,
        );
        Ok(connection)
    }
}

fn spawn_bunker_connection(
    connection: Weak<Nip46Connection>,
    cancellation: nmp_signer::Nip46Cancellation,
    bunker_uri: String,
    timeout_millis: u64,
) {
    thread::spawn(move || {
        let events = lifecycle_sink(connection.clone());
        let result = nmp::Nip46Signer::connect_bunker_observed_with_cancellation(
            &bunker_uri,
            None,
            nmp::Nip46ClientMetadata::default(),
            Duration::from_millis(timeout_millis),
            events,
            &cancellation,
        );
        let Some(connection) = connection.upgrade() else {
            return;
        };
        match result {
            Ok(signer) => connection.attach(signer),
            Err(error) => connection.fail(error.to_string()),
        }
    });
}

fn spawn_invitation_connection(
    connection: Weak<Nip46Connection>,
    cancellation: nmp_signer::Nip46Cancellation,
    invitation: nmp::Nip46Invitation,
    timeout_millis: u64,
) {
    thread::spawn(move || {
        let events = lifecycle_sink(connection.clone());
        let result = invitation.connect_observed_with_cancellation(
            Duration::from_millis(timeout_millis),
            events,
            &cancellation,
        );
        let Some(connection) = connection.upgrade() else {
            return;
        };
        match result {
            Ok(signer) => connection.attach(signer),
            Err(error) => connection.fail(error.to_string()),
        }
    });
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

    use nostr::Keys;

    struct CloseCountingObserver {
        closed: Arc<AtomicUsize>,
    }

    impl Nip46ConnectionObserver for CloseCountingObserver {
        fn on_event(&self, _event: FfiNip46ConnectionEvent) {}

        fn on_ready(&self, _user_public_key: String) {}

        fn on_failed(&self, _reason: String) {}

        fn on_closed(&self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ReentrantObserver {
        deliveries: Arc<Mutex<Vec<&'static str>>>,
        connection: Mutex<Weak<Nip46Connection>>,
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

        fn on_failed(&self, _reason: String) {
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

    #[test]
    fn catalog_keeps_probe_launch_package_and_provider_distinct() {
        let primal = local_signer_catalog()
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
        assert_eq!(
            primal.android_provider_authority.as_deref(),
            Some("net.primal.android")
        );
    }

    #[test]
    fn connection_close_and_drop_are_idempotent_and_stream_scoped() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let closed_a = Arc::new(AtomicUsize::new(0));
        let closed_b = Arc::new(AtomicUsize::new(0));
        let connection_a = Nip46Connection::new(
            Arc::clone(&engine),
            Arc::new(CloseCountingObserver {
                closed: Arc::clone(&closed_a),
            }),
        );
        let connection_b = Nip46Connection::new(
            Arc::clone(&engine),
            Arc::new(CloseCountingObserver {
                closed: Arc::clone(&closed_b),
            }),
        );

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
        engine.shutdown();
    }

    #[test]
    fn observer_delivery_is_reentrant_and_closed_is_terminal() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let observer = Arc::new(ReentrantObserver {
            deliveries: Arc::clone(&deliveries),
            connection: Mutex::new(Weak::new()),
        });
        let connection = Nip46Connection::new(Arc::clone(&engine), observer.clone());
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
        engine.shutdown();
    }

    #[test]
    fn unavailable_before_attach_is_retained_as_attachment_state() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let connection = Nip46Connection::new(
            Arc::clone(&engine),
            Arc::new(CloseCountingObserver {
                closed: Arc::new(AtomicUsize::new(0)),
            }),
        );

        connection.on_event(nmp::Nip46ConnectionEvent::Available);
        connection.on_event(nmp::Nip46ConnectionEvent::Unavailable);

        let attachment = connection
            .attachment
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert!(!attachment.available);
        assert!(attachment.registration.is_none());
        drop(attachment);
        connection.disconnect();
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

        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let closed = Arc::new(AtomicUsize::new(0));
        let connection = Nip46Connection::new(
            Arc::clone(&engine),
            Arc::new(CloseCountingObserver {
                closed: Arc::clone(&closed),
            }),
        );
        let weak = Arc::downgrade(&connection);
        let uri = format!(
            "bunker://{}?relay={}&secret=pending-drop",
            remote.public_key().to_hex(),
            url::form_urlencoded::byte_serialize(relay.as_bytes()).collect::<String>()
        );
        spawn_bunker_connection(weak.clone(), connection.cancellation.clone(), uri, 60_000);
        accepted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the pending handshake opens its socket");

        drop(connection);

        assert!(
            weak.upgrade().is_none(),
            "the worker owns no strong connection Arc"
        );
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        closed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("connection drop cancels the pending handshake socket");
        engine.shutdown();
    }
}
