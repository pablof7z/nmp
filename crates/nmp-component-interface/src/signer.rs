use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use nmp_signer::SigningCapability;
use tokio::sync::{mpsc, oneshot};

const COMMAND_CAPACITY: usize = 1;

/// Reachable failures at the provider-contribution boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentInterfaceError {
    EngineClosed,
    SignerMissingPublicKey,
    CapabilityRegistryFull { limit: usize },
    CapabilityInstanceExhausted,
    AdapterClosed,
    CoreRefused { reason: String },
}

impl std::fmt::Display for ComponentInterfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EngineClosed => f.write_str("core engine already shut down"),
            Self::SignerMissingPublicKey => {
                f.write_str("signer capability has no stable public key")
            }
            Self::CapabilityRegistryFull { limit } => {
                write!(f, "signer capability registry is full at {limit} entries")
            }
            Self::CapabilityInstanceExhausted => {
                f.write_str("signer capability instance space exhausted")
            }
            Self::AdapterClosed => f.write_str("provider signer adapter is closed"),
            Self::CoreRefused { reason } => write!(f, "core refused signer operation: {reason}"),
        }
    }
}

impl std::error::Error for ComponentInterfaceError {}

/// One provider task admitted to the existing core-owned adapter runtime.
pub type ProviderAdapterTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Opaque scheduling capability minted by the core at successful install.
///
/// Independently linked components carry distinct Tokio thread-local context,
/// even when they contain the same Tokio version. Every provider future is
/// therefore entered through this capability on each poll; neither an ambient
/// runtime lookup nor a raw runtime handle escapes to provider code.
#[derive(Clone)]
pub struct SignerAdapterRuntime {
    handle: tokio::runtime::Handle,
}

struct RuntimeContextFuture<F> {
    handle: tokio::runtime::Handle,
    future: Pin<Box<F>>,
}

impl<F> Unpin for RuntimeContextFuture<F> {}

impl<F: Future> Future for RuntimeContextFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        let handle = this.handle.clone();
        let _entered = handle.enter();
        this.future.as_mut().poll(context)
    }
}

impl SignerAdapterRuntime {
    fn from_core(handle: tokio::runtime::Handle) -> Self {
        Self { handle }
    }

    /// Enter this runtime in the calling component for every poll.
    pub fn contextualize<F>(&self, future: F) -> impl Future<Output = F::Output> + Send + 'static
    where
        F: Future + Send + 'static,
    {
        RuntimeContextFuture {
            handle: self.handle.clone(),
            future: Box::pin(future),
        }
    }

    /// Schedule one context-wrapped task on the exact core runtime.
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(self.contextualize(future))
    }
}

pub enum SignerAdapterCommand {
    Attach {
        signer: Box<dyn SigningCapability + Send>,
        reply: oneshot::Sender<Result<(), ComponentInterfaceError>>,
    },
    Detach {
        reply: oneshot::Sender<Result<(), ComponentInterfaceError>>,
    },
}

/// Non-clone provider control. The lane lock is held through the core ack, so
/// at most one command can be outstanding even when provider callbacks race.
pub struct SignerAdapterControl {
    commands: mpsc::Sender<SignerAdapterCommand>,
    lane: tokio::sync::Mutex<()>,
}

impl SignerAdapterControl {
    pub async fn attach_boxed(
        &self,
        signer: Box<dyn SigningCapability + Send>,
    ) -> Result<(), ComponentInterfaceError> {
        let _lane = self.lane.lock().await;
        let (reply, acknowledgement) = oneshot::channel();
        self.commands
            .send(SignerAdapterCommand::Attach { signer, reply })
            .await
            .map_err(|_| ComponentInterfaceError::AdapterClosed)?;
        acknowledgement
            .await
            .map_err(|_| ComponentInterfaceError::AdapterClosed)?
    }

    pub async fn detach(&self) -> Result<(), ComponentInterfaceError> {
        let _lane = self.lane.lock().await;
        let (reply, acknowledgement) = oneshot::channel();
        self.commands
            .send(SignerAdapterCommand::Detach { reply })
            .await
            .map_err(|_| ComponentInterfaceError::AdapterClosed)?;
        acknowledgement
            .await
            .map_err(|_| ComponentInterfaceError::AdapterClosed)?
    }
}

type Cancel = Box<dyn FnOnce() + Send + 'static>;

/// Exact provider cancellation, consumed at most once by pre-install drop,
/// provider/task end, installation close, or installation drop.
pub struct SignerAdapterCancellation {
    cancel: Mutex<Option<Cancel>>,
}

impl SignerAdapterCancellation {
    fn new(cancel: impl FnOnce() + Send + 'static) -> Arc<Self> {
        Arc::new(Self {
            cancel: Mutex::new(Some(Box::new(cancel))),
        })
    }

    pub fn cancel(&self) -> bool {
        let cancel = self
            .cancel
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(cancel) = cancel {
            cancel();
            true
        } else {
            false
        }
    }
}

impl Drop for SignerAdapterCancellation {
    fn drop(&mut self) {
        if let Some(cancel) = self
            .cancel
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            cancel();
        }
    }
}

struct PendingSignerAdapter {
    task: TaskFactory,
    control: SignerAdapterControl,
    commands: mpsc::Receiver<SignerAdapterCommand>,
    cancellation: Arc<SignerAdapterCancellation>,
}

type TaskFactory = Box<
    dyn FnOnce(SignerAdapterControl, SignerAdapterRuntime) -> ProviderAdapterTask + Send + 'static,
>;

/// Pending parts moved exactly once into the core installation path.
pub struct TakenSignerAdapter {
    task: TaskFactory,
    control: SignerAdapterControl,
    commands: mpsc::Receiver<SignerAdapterCommand>,
    cancellation: Arc<SignerAdapterCancellation>,
}

/// Started parts produced only after the core has committed installation
/// state. Starting is infallible and consumes the factory once.
pub struct StartedSignerAdapter {
    pub task: ProviderAdapterTask,
    pub commands: mpsc::Receiver<SignerAdapterCommand>,
    pub cancellation: Arc<SignerAdapterCancellation>,
}

impl TakenSignerAdapter {
    #[must_use]
    pub fn start(self, core_runtime: tokio::runtime::Handle) -> StartedSignerAdapter {
        StartedSignerAdapter {
            task: (self.task)(self.control, SignerAdapterRuntime::from_core(core_runtime)),
            commands: self.commands,
            cancellation: self.cancellation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerAdapterTakeError {
    AlreadyTaken,
}

/// Opaque, take-once signer contribution. It contains no core authority.
#[derive(uniffi::Object)]
pub struct FfiSignerAdapter {
    pending: Mutex<Option<PendingSignerAdapter>>,
}

/// Create one bounded provider contribution. The task factory is retained
/// untouched; provider code cannot run until successful core installation
/// consumes and starts it.
#[must_use]
pub fn new_signer_adapter(
    cancel: impl FnOnce() + Send + 'static,
    task: impl FnOnce(SignerAdapterControl, SignerAdapterRuntime) -> ProviderAdapterTask
        + Send
        + 'static,
) -> Arc<FfiSignerAdapter> {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let control = SignerAdapterControl {
        commands,
        lane: tokio::sync::Mutex::new(()),
    };
    Arc::new(FfiSignerAdapter {
        pending: Mutex::new(Some(PendingSignerAdapter {
            task: Box::new(task),
            control,
            commands: receiver,
            cancellation: SignerAdapterCancellation::new(cancel),
        })),
    })
}

impl FfiSignerAdapter {
    /// Consume this contribution for installation. Only `nmp-ffi` has an
    /// engine door; direct interface consumers can take and discard parts but
    /// cannot construct, remove, or replay a core registration.
    #[doc(hidden)]
    pub fn take_for_install(&self) -> Result<TakenSignerAdapter, SignerAdapterTakeError> {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .map(|pending| TakenSignerAdapter {
                task: pending.task,
                control: pending.control,
                commands: pending.commands,
                cancellation: pending.cancellation,
            })
            .ok_or(SignerAdapterTakeError::AlreadyTaken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn adapter_parts_are_consumed_once() {
        let adapter = new_signer_adapter(|| {}, |_, _| Box::pin(async {}));

        assert!(adapter.take_for_install().is_ok());
        assert_eq!(
            adapter.take_for_install().err(),
            Some(SignerAdapterTakeError::AlreadyTaken)
        );
    }

    #[test]
    fn task_factory_is_lazy_and_started_once() {
        let starts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&starts);
        let adapter = new_signer_adapter(
            || {},
            move |_, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            },
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let taken = adapter.take_for_install().unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        let started = taken.start(runtime.handle().clone());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        drop(started);
        assert_eq!(
            adapter.take_for_install().err(),
            Some(SignerAdapterTakeError::AlreadyTaken)
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_uninstalled_adapter_cancels_once() {
        let cancellations = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&cancellations);
        let adapter = new_signer_adapter(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
            },
            |_, _| Box::pin(async {}),
        );

        drop(adapter);
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    }
}
