//! Native signer discovery and NIP-46 connection projection.
//!
//! Rust owns catalog/protocol/lifecycle policy. Native shells only execute
//! the supplied OS probe/launch URI and render these bounded progress facts.

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
}

#[derive(uniffi::Object)]
pub struct FfiNip46Invitation {
    inner: Mutex<Option<nmp::Nip46Invitation>>,
}

/// Owns one attached remote signer for exactly as long as its FFI engine.
/// The session callback only retains a `Weak` reference to this coordinator,
/// avoiding a signer -> callback -> signer ownership cycle.
pub(crate) struct Nip46Connection {
    engine: Arc<nmp::Engine>,
    observer: Arc<dyn Nip46ConnectionObserver>,
    signer: Mutex<Option<nmp::Nip46Signer>>,
}

impl Nip46Connection {
    fn new(engine: Arc<nmp::Engine>, observer: Arc<dyn Nip46ConnectionObserver>) -> Arc<Self> {
        Arc::new(Self {
            engine,
            observer,
            signer: Mutex::new(None),
        })
    }

    fn on_event(&self, event: nmp::Nip46ConnectionEvent) {
        match &event {
            nmp::Nip46ConnectionEvent::Available => {
                if let Some(signer) = self.signer.lock().ok().and_then(|slot| slot.clone()) {
                    let _ = self.engine.add_signer(signer);
                }
            }
            nmp::Nip46ConnectionEvent::Unavailable => {
                if let Some(signer) = self.signer.lock().ok().and_then(|slot| slot.clone()) {
                    let _ = self.engine.remove_signer(signer.user_public_key());
                }
            }
            _ => {}
        }
        self.observer.on_event(event_to_ffi(event));
    }

    fn attach(
        self: &Arc<Self>,
        signer: nmp::Nip46Signer,
        retained: &Mutex<Vec<Arc<Nip46Connection>>>,
    ) {
        let pubkey = signer.user_public_key();
        match self.engine.add_signer(signer.clone()) {
            Ok(_) => {
                if let Ok(mut slot) = self.signer.lock() {
                    *slot = Some(signer);
                }
                if let Ok(mut connections) = retained.lock() {
                    connections.push(Arc::clone(self));
                }
                self.observer.on_ready(pubkey.to_hex());
            }
            Err(error) => self.observer.on_failed(error.to_string()),
        }
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
        Ok(invitation.uri_with_scheme(scheme))
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
    ) {
        let engine = Arc::clone(&self.engine);
        let retained = Arc::clone(&self.nip46_connections);
        let observer: Arc<dyn Nip46ConnectionObserver> = Arc::from(observer);
        spawn_bunker_connection(engine, retained, bunker_uri, timeout_millis, observer);
    }

    pub fn connect_nip46_invitation(
        &self,
        invitation: Arc<FfiNip46Invitation>,
        timeout_millis: u64,
        observer: Box<dyn Nip46ConnectionObserver>,
    ) -> Result<(), FfiError> {
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
        let retained = Arc::clone(&self.nip46_connections);
        let observer: Arc<dyn Nip46ConnectionObserver> = Arc::from(observer);
        spawn_invitation_connection(engine, retained, invitation, timeout_millis, observer);
        Ok(())
    }
}

fn spawn_bunker_connection(
    engine: Arc<nmp::Engine>,
    retained: Arc<Mutex<Vec<Arc<Nip46Connection>>>>,
    bunker_uri: String,
    timeout_millis: u64,
    observer: Arc<dyn Nip46ConnectionObserver>,
) {
    thread::spawn(move || {
        let connection = Nip46Connection::new(engine, observer);
        let events = lifecycle_sink(Arc::downgrade(&connection));
        match nmp::Nip46Signer::connect_bunker_observed(
            &bunker_uri,
            None,
            nmp::Nip46ClientMetadata::default(),
            Duration::from_millis(timeout_millis),
            events,
        ) {
            Ok(signer) => connection.attach(signer, &retained),
            Err(error) => connection.observer.on_failed(error.to_string()),
        }
    });
}

fn spawn_invitation_connection(
    engine: Arc<nmp::Engine>,
    retained: Arc<Mutex<Vec<Arc<Nip46Connection>>>>,
    invitation: nmp::Nip46Invitation,
    timeout_millis: u64,
    observer: Arc<dyn Nip46ConnectionObserver>,
) {
    thread::spawn(move || {
        let connection = Nip46Connection::new(engine, observer);
        let events = lifecycle_sink(Arc::downgrade(&connection));
        match invitation.connect_observed(Duration::from_millis(timeout_millis), events) {
            Ok(signer) => connection.attach(signer, &retained),
            Err(error) => connection.observer.on_failed(error.to_string()),
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
}
