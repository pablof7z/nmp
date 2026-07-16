//! Native projection of app-owned NIP-42 authorization policy.
//!
//! A foreign callback receives an immutable request and a Rust-owned
//! completion object. Resolving during the callback is a ready answer;
//! retaining the completion makes the operation pending. Returning without
//! either action fails closed as `Unavailable`.

use std::sync::{Arc, Condvar, Mutex};

type PolicyResult = Result<nmp::AuthPolicyDecision, nmp::AuthPolicyError>;

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FfiAuthPolicyRequest {
    pub expected_public_key: String,
    pub relay: String,
    pub challenge: String,
    pub transport_generation: u64,
    pub epoch_sequence: u64,
}

impl From<nmp::AuthPolicyRequest> for FfiAuthPolicyRequest {
    fn from(request: nmp::AuthPolicyRequest) -> Self {
        Self {
            expected_public_key: request.expected_pubkey().to_hex(),
            relay: request.relay().to_string(),
            challenge: request.challenge().to_string(),
            transport_generation: request.transport_generation(),
            epoch_sequence: request.epoch_sequence(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FfiAuthPolicyOutcome {
    Allow,
    Deny { reason: String },
    Unavailable,
    Technical { reason: String },
}

impl FfiAuthPolicyOutcome {
    fn into_policy_result(self) -> PolicyResult {
        match self {
            Self::Allow => Ok(nmp::AuthPolicyDecision::Allow),
            Self::Deny { reason } => Ok(nmp::AuthPolicyDecision::Deny { reason }),
            Self::Unavailable => Err(nmp::AuthPolicyError::Unavailable),
            Self::Technical { reason } => Err(nmp::AuthPolicyError::Technical { reason }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Error)]
pub enum FfiAuthPolicyCompletionError {
    AlreadyCompleted,
    Cancelled,
    ReceiverGone,
}

impl std::fmt::Display for FfiAuthPolicyCompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyCompleted => f.write_str("AUTH policy request already completed"),
            Self::Cancelled => f.write_str("AUTH policy request was cancelled"),
            Self::ReceiverGone => f.write_str("AUTH policy receiver is gone"),
        }
    }
}

impl std::error::Error for FfiAuthPolicyCompletionError {}

enum CompletionState {
    Evaluating {
        result: Option<PolicyResult>,
        sender: Option<nmp::AuthPolicyPendingSender>,
    },
    Pending(nmp::AuthPolicyPendingSender),
    Resolving,
    Completed,
    Cancelled,
}

#[cfg(test)]
type CancelProbe = Box<dyn FnOnce() + Send>;

struct ResolvingOwner {
    sender: Option<nmp::AuthPolicyPendingSender>,
    state: Arc<Mutex<CompletionState>>,
    settled: Arc<Condvar>,
    active: bool,
}

impl ResolvingOwner {
    fn new(
        sender: nmp::AuthPolicyPendingSender,
        state: Arc<Mutex<CompletionState>>,
        settled: Arc<Condvar>,
    ) -> Self {
        Self {
            sender: Some(sender),
            state,
            settled,
            active: true,
        }
    }

    fn send(&mut self, result: PolicyResult) -> Result<(), nmp::AuthPolicyResolveError> {
        self.sender
            .take()
            .expect("resolving owner sends once")
            .resolve(result)
    }

    fn finish(mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert!(
            matches!(*state, CompletionState::Resolving),
            "a resolver that owns completion cannot be cancelled"
        );
        *state = CompletionState::Completed;
        drop(state);
        self.settled.notify_all();
        self.active = false;
    }
}

impl Drop for ResolvingOwner {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Disconnect the last pending sender before waking a losing
        // cancellation, so its retained receiver observes a terminal channel
        // even when resolver code unwinds before sending.
        drop(self.sender.take());
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if matches!(*state, CompletionState::Resolving) {
            *state = CompletionState::Completed;
        }
        drop(state);
        self.settled.notify_all();
    }
}

#[derive(uniffi::Object)]
pub struct FfiAuthPolicyCompletion {
    state: Arc<Mutex<CompletionState>>,
    settled: Arc<Condvar>,
    #[cfg(test)]
    before_send: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_send: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    cancel_probe: Arc<Mutex<Option<CancelProbe>>>,
}

impl FfiAuthPolicyCompletion {
    fn evaluating() -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(Mutex::new(CompletionState::Evaluating {
                result: None,
                sender: None,
            })),
            settled: Arc::new(Condvar::new()),
            #[cfg(test)]
            before_send: Mutex::new(None),
            #[cfg(test)]
            after_send: Mutex::new(None),
            #[cfg(test)]
            cancel_probe: Arc::new(Mutex::new(None)),
        })
    }

    fn state(&self) -> Arc<Mutex<CompletionState>> {
        Arc::clone(&self.state)
    }
}

#[uniffi::export]
impl FfiAuthPolicyCompletion {
    /// Resolve this exact request once. A cancellation that linearized first
    /// is distinct from a repeated completion and a dropped runtime receiver.
    pub fn resolve(
        &self,
        outcome: FfiAuthPolicyOutcome,
    ) -> Result<(), FfiAuthPolicyCompletionError> {
        let result = outcome.into_policy_result();
        let sender = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match &mut *state {
                CompletionState::Evaluating { result: slot, .. } if slot.is_none() => {
                    *slot = Some(result);
                    return Ok(());
                }
                CompletionState::Evaluating { .. }
                | CompletionState::Resolving
                | CompletionState::Completed => {
                    return Err(FfiAuthPolicyCompletionError::AlreadyCompleted);
                }
                CompletionState::Cancelled => {
                    return Err(FfiAuthPolicyCompletionError::Cancelled);
                }
                CompletionState::Pending(_) => {
                    let CompletionState::Pending(sender) =
                        std::mem::replace(&mut *state, CompletionState::Resolving)
                    else {
                        unreachable!("pending completion state changed while locked")
                    };
                    sender
                }
            }
        };
        let mut owner =
            ResolvingOwner::new(sender, Arc::clone(&self.state), Arc::clone(&self.settled));

        #[cfg(test)]
        if let Some(hook) = self
            .before_send
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            hook();
        }
        let resolution = owner.send(result);
        #[cfg(test)]
        if let Some(hook) = self
            .after_send
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            hook();
        }
        owner.finish();
        match resolution {
            Ok(()) => Ok(()),
            Err(nmp::AuthPolicyResolveError::AlreadyResolved(_)) => {
                Err(FfiAuthPolicyCompletionError::AlreadyCompleted)
            }
            Err(nmp::AuthPolicyResolveError::ReceiverDropped(_)) => {
                Err(FfiAuthPolicyCompletionError::ReceiverGone)
            }
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait FfiAuthPolicyCallback: Send + Sync {
    fn evaluate(&self, request: FfiAuthPolicyRequest, completion: Arc<FfiAuthPolicyCompletion>);

    /// Called from the runtime worker after cancellation has won the pending
    /// completion linearization. Implementations may safely reenter NMP.
    fn on_cancelled(&self, request: FfiAuthPolicyRequest);
}

/// The adapter's own ready-or-pending fact, kept separate from the opaque
/// [`nmp::AuthPolicyOp`] newtype (which exposes constructors, never
/// variants) so the inline/pending split stays directly observable to this
/// module's tests. `Pending` carries the engine-owned operation whole:
/// dropping it drops the engine op inside, which is exactly the runtime's
/// terminal-cancel door.
enum FfiPolicyEvaluation {
    Ready(PolicyResult),
    Pending(nmp::AuthPolicyOp),
}

pub(crate) struct FfiAuthPolicyAdapter {
    callback: Arc<dyn FfiAuthPolicyCallback>,
}

impl FfiAuthPolicyAdapter {
    pub(crate) fn new(callback: Box<dyn FfiAuthPolicyCallback>) -> Self {
        Self {
            callback: Arc::from(callback),
        }
    }

    fn evaluate_ffi(&self, request: FfiAuthPolicyRequest) -> FfiPolicyEvaluation {
        let completion = FfiAuthPolicyCompletion::evaluating();
        let state = completion.state();
        let weak_state = Arc::downgrade(&state);
        let callback = Arc::clone(&self.callback);
        let cancellation_request = request.clone();
        let settled = Arc::clone(&completion.settled);
        #[cfg(test)]
        let cancel_probe = Arc::clone(&completion.cancel_probe);
        let (sender, operation) = nmp::AuthPolicyOp::pending_channel_with_cancel(move || {
            #[cfg(test)]
            if let Some(probe) = cancel_probe
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
            {
                probe();
            }
            let notify = match weak_state.upgrade() {
                None => true,
                Some(state) => {
                    let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
                    loop {
                        match *state {
                            CompletionState::Pending(_) => {
                                *state = CompletionState::Cancelled;
                                break true;
                            }
                            CompletionState::Resolving => {
                                state = settled
                                    .wait(state)
                                    .unwrap_or_else(|poison| poison.into_inner());
                            }
                            CompletionState::Completed | CompletionState::Cancelled => {
                                break false;
                            }
                            CompletionState::Evaluating { .. } => {
                                *state = CompletionState::Cancelled;
                                break false;
                            }
                        }
                    }
                }
            };
            if notify {
                callback.on_cancelled(cancellation_request);
            }
        });
        {
            let mut guard = state.lock().unwrap_or_else(|poison| poison.into_inner());
            let CompletionState::Evaluating {
                sender: sender_slot,
                ..
            } = &mut *guard
            else {
                unreachable!("new completion must start in evaluating state")
            };
            *sender_slot = Some(sender);
        }

        self.callback.evaluate(request, Arc::clone(&completion));
        let retained_by_foreign = Arc::strong_count(&completion) > 1;
        let mut guard = state.lock().unwrap_or_else(|poison| poison.into_inner());
        let (inline_result, sender) = match &mut *guard {
            CompletionState::Evaluating { result, sender } => (
                result.take(),
                sender
                    .take()
                    .expect("pending sender installed before callback"),
            ),
            _ => unreachable!("completion cannot leave evaluating state during callback"),
        };
        if let Some(result) = inline_result {
            *guard = CompletionState::Completed;
            drop(guard);
            drop(sender);
            drop(operation);
            return FfiPolicyEvaluation::Ready(result);
        }

        *guard = CompletionState::Pending(sender);
        drop(guard);
        if !retained_by_foreign {
            // Drop our two strong references before returning the pending
            // receiver. With no foreign retain, this drops the sole sender;
            // the runtime maps that disconnect to `Unavailable`.
            drop(completion);
            drop(state);
        }
        FfiPolicyEvaluation::Pending(operation)
    }
}

impl nmp::AuthPolicy for FfiAuthPolicyAdapter {
    fn evaluate(&self, request: nmp::AuthPolicyRequest) -> nmp::AuthPolicyOp {
        match self.evaluate_ffi(request.into()) {
            FfiPolicyEvaluation::Ready(result) => nmp::AuthPolicyOp::ready(result),
            FfiPolicyEvaluation::Pending(operation) => operation,
        }
    }
}

/// Opaque proof for one exact local-account installation. Dropping this
/// object does not remove the account; removal is explicit and repeatable.
#[derive(uniffi::Object)]
pub struct FfiAccountRegistration {
    pub(crate) inner: nmp::AccountRegistration,
}

impl std::fmt::Debug for FfiAccountRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiAccountRegistration")
            .field("public_key", &self.inner.public_key())
            .finish_non_exhaustive()
    }
}

#[uniffi::export]
impl FfiAccountRegistration {
    pub fn public_key(&self) -> String {
        self.inner.public_key().to_hex()
    }
}

/// Opaque proof for one exact AUTH-policy installation. It exposes only the
/// expected account identity, never the capability instance.
#[derive(uniffi::Object)]
pub struct FfiAuthPolicyRegistration {
    pub(crate) inner: nmp::AuthPolicyRegistration,
}

impl std::fmt::Debug for FfiAuthPolicyRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiAuthPolicyRegistration")
            .field("expected_public_key", &self.inner.expected_public_key())
            .finish_non_exhaustive()
    }
}

#[uniffi::export]
impl FfiAuthPolicyRegistration {
    pub fn expected_public_key(&self) -> String {
        self.inner.expected_public_key().to_hex()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn request() -> FfiAuthPolicyRequest {
        FfiAuthPolicyRequest {
            expected_public_key: "11".repeat(32),
            relay: "wss://relay.example.com/".to_string(),
            challenge: "exact challenge".to_string(),
            transport_generation: 7,
            epoch_sequence: 9,
        }
    }

    struct InlinePolicy;

    impl FfiAuthPolicyCallback for InlinePolicy {
        fn evaluate(
            &self,
            _request: FfiAuthPolicyRequest,
            completion: Arc<FfiAuthPolicyCompletion>,
        ) {
            completion.resolve(FfiAuthPolicyOutcome::Allow).unwrap();
        }

        fn on_cancelled(&self, _request: FfiAuthPolicyRequest) {
            panic!("ready policy cannot be cancelled");
        }
    }

    #[test]
    fn inline_completion_is_ready_and_exactly_once() {
        let adapter = FfiAuthPolicyAdapter::new(Box::new(InlinePolicy));
        assert!(matches!(
            adapter.evaluate_ffi(request()),
            FfiPolicyEvaluation::Ready(Ok(nmp::AuthPolicyDecision::Allow))
        ));
    }

    #[test]
    fn completion_reports_only_actual_terminal_errors() {
        let inline = FfiAuthPolicyCompletion::evaluating();
        inline.resolve(FfiAuthPolicyOutcome::Allow).unwrap();
        assert_eq!(
            inline.resolve(FfiAuthPolicyOutcome::Allow),
            Err(FfiAuthPolicyCompletionError::AlreadyCompleted)
        );

        let receiver_gone = FfiAuthPolicyCompletion::evaluating();
        let (sender, operation) = nmp::AuthPolicyOp::pending_channel();
        *receiver_gone.state.lock().unwrap() = CompletionState::Pending(sender);
        drop(operation);
        assert_eq!(
            receiver_gone.resolve(FfiAuthPolicyOutcome::Allow),
            Err(FfiAuthPolicyCompletionError::ReceiverGone)
        );
    }

    struct DroppingPolicy;

    impl FfiAuthPolicyCallback for DroppingPolicy {
        fn evaluate(
            &self,
            _request: FfiAuthPolicyRequest,
            _completion: Arc<FfiAuthPolicyCompletion>,
        ) {
        }

        fn on_cancelled(&self, _request: FfiAuthPolicyRequest) {
            // The real runtime consumes the disconnected sender as
            // `Unavailable`; this unit test drops the opaque pending op.
        }
    }

    #[test]
    fn returning_without_resolve_or_retain_is_unavailable() {
        let adapter = FfiAuthPolicyAdapter::new(Box::new(DroppingPolicy));
        assert!(matches!(
            adapter.evaluate_ffi(request()),
            FfiPolicyEvaluation::Pending(_)
        ));
    }

    struct RetainingPolicy {
        completion: Mutex<Option<Arc<FfiAuthPolicyCompletion>>>,
        cancellations: AtomicUsize,
        cancellation_reentry: Mutex<Vec<Result<(), FfiAuthPolicyCompletionError>>>,
        engine: Arc<nmp::Engine>,
    }

    impl FfiAuthPolicyCallback for RetainingPolicy {
        fn evaluate(
            &self,
            _request: FfiAuthPolicyRequest,
            completion: Arc<FfiAuthPolicyCompletion>,
        ) {
            *self.completion.lock().unwrap() = Some(completion);
        }

        fn on_cancelled(&self, request: FfiAuthPolicyRequest) {
            assert_eq!(request.transport_generation, 7);
            assert_eq!(self.engine.active_account().unwrap(), None);
            let completion = self
                .completion
                .lock()
                .unwrap()
                .as_ref()
                .expect("retained completion")
                .clone();
            self.cancellation_reentry
                .lock()
                .unwrap()
                .push(completion.resolve(FfiAuthPolicyOutcome::Allow));
            self.cancellations.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct SharedRetainingPolicy(Arc<RetainingPolicy>);

    impl FfiAuthPolicyCallback for SharedRetainingPolicy {
        fn evaluate(
            &self,
            request: FfiAuthPolicyRequest,
            completion: Arc<FfiAuthPolicyCompletion>,
        ) {
            self.0.evaluate(request, completion);
        }

        fn on_cancelled(&self, request: FfiAuthPolicyRequest) {
            self.0.on_cancelled(request);
        }
    }

    fn retaining_adapter() -> (FfiAuthPolicyAdapter, Arc<RetainingPolicy>, Arc<nmp::Engine>) {
        let engine = Arc::new(nmp::Engine::new(nmp::EngineConfig::default()).unwrap());
        let callback = Arc::new(RetainingPolicy {
            completion: Mutex::new(None),
            cancellations: AtomicUsize::new(0),
            cancellation_reentry: Mutex::new(Vec::new()),
            engine: Arc::clone(&engine),
        });
        let adapter =
            FfiAuthPolicyAdapter::new(Box::new(SharedRetainingPolicy(Arc::clone(&callback))));
        (adapter, callback, engine)
    }

    #[test]
    fn retained_completion_is_pending_and_drop_cancels_once() {
        let (adapter, callback, engine) = retaining_adapter();
        let operation = adapter.evaluate_ffi(request());
        assert!(matches!(&operation, FfiPolicyEvaluation::Pending(_)));
        drop(operation);
        assert_eq!(callback.cancellations.load(Ordering::Acquire), 1);
        assert_eq!(
            *callback.cancellation_reentry.lock().unwrap(),
            vec![Err(FfiAuthPolicyCompletionError::Cancelled)],
            "on_cancelled must run after the completion-state lock is released"
        );
        assert_eq!(
            callback
                .completion
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .resolve(FfiAuthPolicyOutcome::Allow),
            Err(FfiAuthPolicyCompletionError::Cancelled)
        );
        engine.shutdown();
    }

    #[test]
    fn queued_resolution_wins_simultaneous_terminal_cancel_without_receiver_gone() {
        let (adapter, callback, engine) = retaining_adapter();
        let operation = adapter.evaluate_ffi(request());
        let completion = callback
            .completion
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        let (sent_tx, sent_rx) = mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = mpsc::sync_channel(0);
        *completion.after_send.lock().unwrap() = Some(Box::new(move || {
            sent_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        }));

        let resolving = {
            let completion = Arc::clone(&completion);
            thread::spawn(move || completion.resolve(FfiAuthPolicyOutcome::Allow))
        };
        sent_rx
            .recv()
            .expect("result must be queued before terminal cancel");
        let (dropped_tx, dropped_rx) = mpsc::sync_channel(0);
        let dropping = thread::spawn(move || {
            drop(operation);
            dropped_tx.send(()).unwrap();
        });
        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("queued result must let terminal drop finish");
        dropping
            .join()
            .expect("queued result must let terminal drop finish");
        assert_eq!(callback.cancellations.load(Ordering::Acquire), 0);

        finish_tx.send(()).unwrap();
        assert_eq!(resolving.join().unwrap(), Ok(()));
        assert_eq!(callback.cancellations.load(Ordering::Acquire), 0);
        engine.shutdown();
    }

    #[test]
    fn cancel_arriving_during_resolving_waits_and_cannot_drop_receiver() {
        let (adapter, callback, engine) = retaining_adapter();
        let operation = adapter.evaluate_ffi(request());
        let completion = callback
            .completion
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        let (resolving_tx, resolving_rx) = mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = mpsc::sync_channel(0);
        *completion.before_send.lock().unwrap() = Some(Box::new(move || {
            resolving_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        }));
        let (cancel_tx, cancel_rx) = mpsc::sync_channel(0);
        *completion.cancel_probe.lock().unwrap() = Some(Box::new(move || {
            cancel_tx.send(()).unwrap();
        }));

        let resolving = {
            let completion = Arc::clone(&completion);
            thread::spawn(move || completion.resolve(FfiAuthPolicyOutcome::Allow))
        };
        resolving_rx
            .recv()
            .expect("resolver must own Resolving before cancellation");
        let (dropped_tx, dropped_rx) = mpsc::sync_channel(0);
        let dropping = thread::spawn(move || {
            drop(operation);
            dropped_tx.send(()).unwrap();
        });
        cancel_rx.recv().unwrap();
        assert_eq!(callback.cancellations.load(Ordering::Acquire), 0);

        finish_tx.send(()).unwrap();
        assert_eq!(resolving.join().unwrap(), Ok(()));
        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("resolver send must release losing cancellation");
        dropping
            .join()
            .expect("losing cancellation must consume resolver result");
        assert_eq!(callback.cancellations.load(Ordering::Acquire), 0);
        engine.shutdown();
    }

    #[test]
    fn resolver_unwind_after_ownership_disconnects_waiter_without_hang() {
        let (adapter, callback, engine) = retaining_adapter();
        let operation = adapter.evaluate_ffi(request());
        let completion = callback
            .completion
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        let (resolving_tx, resolving_rx) = mpsc::sync_channel(0);
        let (panic_tx, panic_rx) = mpsc::sync_channel(0);
        *completion.before_send.lock().unwrap() = Some(Box::new(move || {
            resolving_tx.send(()).unwrap();
            panic_rx.recv().unwrap();
            panic!("injected resolver unwind before send");
        }));
        let (cancel_tx, cancel_rx) = mpsc::sync_channel(0);
        *completion.cancel_probe.lock().unwrap() = Some(Box::new(move || {
            cancel_tx.send(()).unwrap();
        }));

        let resolving = {
            let completion = Arc::clone(&completion);
            thread::spawn(move || completion.resolve(FfiAuthPolicyOutcome::Allow))
        };
        resolving_rx
            .recv()
            .expect("resolver must own completion before injected unwind");
        let (dropped_tx, dropped_rx) = mpsc::sync_channel(0);
        let dropping = thread::spawn(move || {
            drop(operation);
            dropped_tx.send(()).unwrap();
        });
        cancel_rx.recv().unwrap();
        panic_tx.send(()).unwrap();
        assert!(resolving.join().is_err());
        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("last-sender unwind must release losing cancellation");
        dropping
            .join()
            .expect("last-sender unwind must disconnect the retained receiver");
        assert_eq!(callback.cancellations.load(Ordering::Acquire), 0);
        assert_eq!(
            completion.resolve(FfiAuthPolicyOutcome::Allow),
            Err(FfiAuthPolicyCompletionError::AlreadyCompleted)
        );
        engine.shutdown();
    }
}
