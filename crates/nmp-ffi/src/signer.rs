//! Core-owned installation of protocol-neutral provider contributions.

use std::sync::{Arc, Mutex};

use nmp_component_interface::{
    ComponentInterfaceError, FfiSignerAdapter, SignerAdapterCancellation, SignerAdapterCommand,
    SignerAdapterTakeError,
};

/// Exact standalone core artifact identity.
pub const CORE_COMPONENT_IDENTITY: &str = env!("NMP_CORE_COMPONENT_IDENTITY");

/// Return the loaded core library's identity as plain data.
#[uniffi::export]
pub fn nmp_core_component_identity() -> String {
    CORE_COMPONENT_IDENTITY.to_owned()
}

/// Typed refusal before a provider contribution is installed.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiSignerAdapterInstallError {
    EngineClosed,
    AdapterAlreadyTaken,
}

impl std::fmt::Display for FfiSignerAdapterInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EngineClosed => f.write_str("core engine already shut down"),
            Self::AdapterAlreadyTaken => {
                f.write_str("provider signer adapter was already consumed")
            }
        }
    }
}

impl std::error::Error for FfiSignerAdapterInstallError {}

enum DriverState {
    Running {
        registration: Option<nmp::SignerRegistration>,
    },
    Closed,
}

struct AdapterInstallationLease {
    engine: Arc<nmp::Engine>,
    state: Arc<Mutex<DriverState>>,
    cancellation: Arc<SignerAdapterCancellation>,
    provider_task_abort: tokio::task::AbortHandle,
    provider_supervisor_abort: tokio::task::AbortHandle,
    driver_abort: tokio::task::AbortHandle,
}

impl AdapterInstallationLease {
    fn close(self) {
        let registration = close_state(&self.state);
        self.cancellation.cancel();
        self.provider_task_abort.abort();
        self.driver_abort.abort();
        self.provider_supervisor_abort.abort();
        remove_registration(&self.engine, registration);
    }
}

/// Exact core installation lease. Close/drop consumes its private lease once.
#[derive(uniffi::Object)]
pub struct FfiSignerAdapterInstallation {
    lease: Mutex<Option<AdapterInstallationLease>>,
}

impl FfiSignerAdapterInstallation {
    fn take_lease(&self) -> Option<AdapterInstallationLease> {
        self.lease
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }
}

impl Drop for FfiSignerAdapterInstallation {
    fn drop(&mut self) {
        if let Some(lease) = self
            .lease
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            lease.close();
        }
    }
}

#[uniffi::export]
impl FfiSignerAdapterInstallation {
    /// Consume this exact installation. A repeated or stale uninstall is
    /// inert. The distinct name avoids colliding with UniFFI Kotlin's
    /// generated `AutoCloseable.close()` object-release method.
    pub fn uninstall(&self) -> bool {
        let Some(lease) = self.take_lease() else {
            return false;
        };
        lease.close();
        true
    }
}

async fn drive_signer_adapter(
    engine: Arc<nmp::Engine>,
    state: Arc<Mutex<DriverState>>,
    cancellation: Arc<SignerAdapterCancellation>,
    mut commands: tokio::sync::mpsc::Receiver<SignerAdapterCommand>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            SignerAdapterCommand::Attach { signer, reply } => {
                let result = attach(&engine, &state, signer);
                let _ = reply.send(result);
            }
            SignerAdapterCommand::Detach { reply } => {
                let result = detach(&engine, &state);
                let _ = reply.send(result);
            }
        }
    }
    let registration = close_state(&state);
    cancellation.cancel();
    remove_registration(&engine, registration);
}

fn attach(
    engine: &Arc<nmp::Engine>,
    state: &Arc<Mutex<DriverState>>,
    signer: Box<dyn nmp::SigningCapability + Send>,
) -> Result<(), ComponentInterfaceError> {
    let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
    let DriverState::Running { registration } = &mut *state else {
        return Err(ComponentInterfaceError::AdapterClosed);
    };
    if registration.is_some() {
        return Err(ComponentInterfaceError::CoreRefused {
            reason: "signer adapter already has an active registration".to_string(),
        });
    }
    let installed = engine.add_signer_boxed(signer).map_err(component_error)?;
    *registration = Some(installed);
    Ok(())
}

fn detach(
    engine: &Arc<nmp::Engine>,
    state: &Arc<Mutex<DriverState>>,
) -> Result<(), ComponentInterfaceError> {
    let registration = {
        let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
        let DriverState::Running { registration } = &mut *state else {
            return Err(ComponentInterfaceError::AdapterClosed);
        };
        registration.take()
    };
    remove_registration(engine, registration);
    Ok(())
}

fn close_state(state: &Arc<Mutex<DriverState>>) -> Option<nmp::SignerRegistration> {
    let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
    match std::mem::replace(&mut *state, DriverState::Closed) {
        DriverState::Running { registration } => registration,
        DriverState::Closed => None,
    }
}

fn remove_registration(engine: &Arc<nmp::Engine>, registration: Option<nmp::SignerRegistration>) {
    if let Some(registration) = registration {
        let _ = engine.remove_signer(registration);
    }
}

pub(crate) fn install_signer_adapter(
    engine: Arc<nmp::Engine>,
    adapter: Arc<FfiSignerAdapter>,
) -> Result<Arc<FfiSignerAdapterInstallation>, FfiSignerAdapterInstallError> {
    // Resolve the core runtime before consuming the provider's take-once
    // contribution. No fallible step remains after `take_for_install`.
    let runtime = engine
        .adapter_runtime()
        .map_err(|_| FfiSignerAdapterInstallError::EngineClosed)?;
    let taken = adapter.take_for_install().map_err(|error| match error {
        SignerAdapterTakeError::AlreadyTaken => FfiSignerAdapterInstallError::AdapterAlreadyTaken,
    })?;
    let state = Arc::new(Mutex::new(DriverState::Running { registration: None }));
    let taken = taken.start(runtime.clone());

    let driver = runtime.spawn(drive_signer_adapter(
        Arc::clone(&engine),
        Arc::clone(&state),
        Arc::clone(&taken.cancellation),
        taken.commands,
    ));
    let provider_engine = Arc::clone(&engine);
    let provider_state = Arc::clone(&state);
    let provider_cancellation = Arc::clone(&taken.cancellation);
    let provider_task = runtime.spawn(taken.task);
    let provider_task_abort = provider_task.abort_handle();
    let provider = runtime.spawn(async move {
        let _task_outcome = provider_task.await;
        let registration = close_state(&provider_state);
        provider_cancellation.cancel();
        remove_registration(&provider_engine, registration);
    });

    Ok(Arc::new(FfiSignerAdapterInstallation {
        lease: Mutex::new(Some(AdapterInstallationLease {
            engine,
            state,
            cancellation: taken.cancellation,
            provider_task_abort,
            provider_supervisor_abort: provider.abort_handle(),
            driver_abort: driver.abort_handle(),
        })),
    }))
}

fn component_error(error: nmp::EngineError) -> ComponentInterfaceError {
    match error {
        nmp::EngineError::SignerMissingPublicKey => ComponentInterfaceError::SignerMissingPublicKey,
        nmp::EngineError::AuthCapabilityRegistryFull { limit } => {
            ComponentInterfaceError::CapabilityRegistryFull { limit }
        }
        nmp::EngineError::AuthCapabilityInstanceExhausted => {
            ComponentInterfaceError::CapabilityInstanceExhausted
        }
        nmp::EngineError::EngineClosed => ComponentInterfaceError::EngineClosed,
        other => ComponentInterfaceError::CoreRefused {
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use nmp_component_interface::{new_signer_adapter, SignerAdapterControl};

    struct TestSigner {
        public_key: Option<nmp::SignerPublicKey>,
    }

    struct ActiveTask {
        active: Arc<AtomicUsize>,
    }

    impl ActiveTask {
        fn start(active: Arc<AtomicUsize>) -> Self {
            active.fetch_add(1, Ordering::SeqCst);
            Self { active }
        }
    }

    impl Drop for ActiveTask {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl nmp::SigningCapability for TestSigner {
        fn public_key(&self) -> Option<nmp::SignerPublicKey> {
            self.public_key
        }

        fn sign(
            &self,
            _unsigned: nmp::SignerUnsignedEvent,
        ) -> nmp::SignerOp<nmp::SignerSignedEvent> {
            panic!("signing is outside the adapter lifecycle falsifier")
        }
    }

    fn pending_adapter(
        starts: Arc<AtomicUsize>,
        cancellations: Arc<AtomicUsize>,
    ) -> Arc<FfiSignerAdapter> {
        new_signer_adapter(
            move || {
                cancellations.fetch_add(1, Ordering::SeqCst);
            },
            move |control, _runtime| {
                starts.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    let _control = control;
                    std::future::pending::<()>().await;
                })
            },
        )
    }

    #[test]
    fn core_component_identity_is_v2_and_exact_length() {
        let identity = nmp_core_component_identity();
        let digest = identity
            .strip_prefix("nmp-core-component-v2-")
            .expect("identity carries its schema version");
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn closed_engine_refuses_before_take_and_live_engine_starts_factory_once() {
        let closed_engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        closed_engine.shutdown();
        let starts = Arc::new(AtomicUsize::new(0));
        let cancellations = Arc::new(AtomicUsize::new(0));
        let adapter = pending_adapter(Arc::clone(&starts), Arc::clone(&cancellations));

        assert!(matches!(
            install_signer_adapter(closed_engine, Arc::clone(&adapter)),
            Err(FfiSignerAdapterInstallError::EngineClosed)
        ));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(cancellations.load(Ordering::SeqCst), 0);

        let live_engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let installation = install_signer_adapter(Arc::clone(&live_engine), adapter)
            .expect("adapter remains live");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(installation.uninstall());
        assert!(!installation.uninstall());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        live_engine.shutdown();
    }

    #[test]
    fn duplicate_take_cannot_reinvoke_factory_or_invalidate_first_installation() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let starts = Arc::new(AtomicUsize::new(0));
        let cancellations = Arc::new(AtomicUsize::new(0));
        let adapter = pending_adapter(Arc::clone(&starts), Arc::clone(&cancellations));

        let first = install_signer_adapter(Arc::clone(&engine), Arc::clone(&adapter)).unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            install_signer_adapter(Arc::clone(&engine), adapter),
            Err(FfiSignerAdapterInstallError::AdapterAlreadyTaken)
        ));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(
            first
                .lease
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_some(),
            "a duplicate adapter alias cannot consume the first installation lease"
        );
        assert_eq!(cancellations.load(Ordering::SeqCst), 0);

        assert!(first.uninstall());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    #[test]
    fn provider_panic_is_supervised_and_removes_the_exact_registration() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let cancellations = Arc::new(AtomicUsize::new(0));
        let counted_cancellations = Arc::clone(&cancellations);
        let (attached_tx, attached_rx) = std::sync::mpsc::sync_channel(1);
        let adapter = new_signer_adapter(
            move || {
                counted_cancellations.fetch_add(1, Ordering::SeqCst);
            },
            move |control, _runtime| {
                Box::pin(async move {
                    control
                        .attach_boxed(Box::new(TestSigner {
                            public_key: Some(nmp::SignerPublicKey::new([9; 32])),
                        }))
                        .await
                        .expect("the provider attaches before the panic");
                    attached_tx.send(()).unwrap();
                    panic!("provider panic cleanup falsifier");
                })
            },
        );
        let installation =
            install_signer_adapter(Arc::clone(&engine), adapter).expect("install adapter");
        attached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the provider reaches its attached state");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let closed = {
                let lease = installation
                    .lease
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                let closed = matches!(
                    &*lease.as_ref().unwrap().state.lock().unwrap(),
                    DriverState::Closed
                );
                closed
            };
            if closed && cancellations.load(Ordering::SeqCst) == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the supervised panic must close state, cancel once, and remove registration"
            );
            std::thread::yield_now();
        }

        assert!(installation.uninstall());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        engine.shutdown();
    }

    #[test]
    fn uninstall_aborts_the_exact_provider_task_without_a_detached_survivor() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&active);
        let adapter = new_signer_adapter(
            || {},
            move |_control, _runtime| {
                let task = ActiveTask::start(counted);
                Box::pin(async move {
                    let _task = task;
                    std::future::pending::<()>().await;
                })
            },
        );
        let installation =
            install_signer_adapter(Arc::clone(&engine), adapter).expect("install adapter");
        assert_eq!(active.load(Ordering::SeqCst), 1);

        assert!(installation.uninstall());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while active.load(Ordering::SeqCst) != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the exact provider task must be dropped after its abort"
            );
            std::thread::yield_now();
        }
        assert!(!installation.uninstall());
        engine.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_preserves_exact_registration_transitions_and_failed_attach_is_inert() {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let (control_tx, control_rx) = std::sync::mpsc::sync_channel(1);
        let adapter = new_signer_adapter(
            || {},
            move |control: SignerAdapterControl, _runtime| {
                control_tx.send(control).unwrap();
                Box::pin(std::future::pending())
            },
        );
        let installation =
            install_signer_adapter(Arc::clone(&engine), adapter).expect("install adapter");
        let control = control_rx.recv().unwrap();

        assert_eq!(
            control
                .attach_boxed(Box::new(TestSigner { public_key: None }))
                .await,
            Err(ComponentInterfaceError::SignerMissingPublicKey)
        );
        {
            let lease = installation
                .lease
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            assert!(matches!(
                &*lease.as_ref().unwrap().state.lock().unwrap(),
                DriverState::Running { registration: None }
            ));
        }

        control
            .attach_boxed(Box::new(TestSigner {
                public_key: Some(nmp::SignerPublicKey::new([7; 32])),
            }))
            .await
            .expect("first exact registration");
        {
            let lease = installation
                .lease
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            assert!(matches!(
                &*lease.as_ref().unwrap().state.lock().unwrap(),
                DriverState::Running {
                    registration: Some(_)
                }
            ));
        }
        assert!(matches!(
            control
                .attach_boxed(Box::new(TestSigner {
                    public_key: Some(nmp::SignerPublicKey::new([8; 32])),
                }))
                .await,
            Err(ComponentInterfaceError::CoreRefused { .. })
        ));

        control.detach().await.expect("detach exact registration");
        {
            let lease = installation
                .lease
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            assert!(matches!(
                &*lease.as_ref().unwrap().state.lock().unwrap(),
                DriverState::Running { registration: None }
            ));
        }
        control.detach().await.expect("repeated detach is inert");
        assert!(installation.uninstall());
        assert_eq!(
            control.detach().await,
            Err(ComponentInterfaceError::AdapterClosed)
        );
        engine.shutdown();
    }
}
