//! The pollable thunk (§3.3, HARVEST `nmp-signer-iface::op`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};

#[doc(hidden)]
pub type PendingCancel = Box<dyn FnOnce() + Send + 'static>;

type PendingSignerSlot<T> = Mutex<Option<Sender<Result<T, SignerError>>>>;

/// NMP-owned completion door for one asynchronous signer operation.
///
/// Clones share one terminal slot: the first call to [`Self::resolve`] owns
/// the result, and every later call receives a typed `AlreadyResolved` error.
/// No channel implementation type crosses the public signer boundary.
pub struct PendingSignerSender<T: Send + 'static> {
    sender: Arc<PendingSignerSlot<T>>,
}

impl<T: Send + 'static> Clone for PendingSignerSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: Arc::clone(&self.sender),
        }
    }
}

impl<T: Send + 'static> PendingSignerSender<T> {
    /// Resolve the matching pending operation exactly once.
    pub fn resolve(
        &self,
        result: Result<T, SignerError>,
    ) -> Result<(), PendingSignerResolveError<T>> {
        let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        else {
            return Err(PendingSignerResolveError::AlreadyResolved(result));
        };
        sender
            .send(result)
            .map_err(|error| PendingSignerResolveError::ReceiverDropped(error.0))
    }
}

/// Typed refusal from [`PendingSignerSender::resolve`].
#[derive(Debug)]
pub enum PendingSignerResolveError<T: Send + 'static> {
    /// Another sender clone already claimed the operation's one result slot.
    AlreadyResolved(Result<T, SignerError>),
    /// The pending operation was cancelled or dropped before resolution.
    ReceiverDropped(Result<T, SignerError>),
}

#[doc(hidden)]
#[derive(Clone)]
pub struct PendingSignerCancel(Sender<()>);

#[doc(hidden)]
pub struct PendingSignerCancelled(Receiver<()>);

#[doc(hidden)]
pub fn pending_signer_cancellation() -> (PendingSignerCancel, PendingSignerCancelled) {
    let (cancel, cancelled) = crossbeam_channel::bounded(1);
    (
        PendingSignerCancel(cancel),
        PendingSignerCancelled(cancelled),
    )
}

impl PendingSignerCancel {
    #[doc(hidden)]
    pub fn cancel(&self) {
        let _ = self.0.try_send(());
    }
}

/// One cancellable asynchronous signer result.
///
/// Dropping an unfinished value invokes the adapter-owned cancellation hook.
/// This matters for direct Rust callers: abandoning or timing out a remote
/// RPC must release its bounded correlation slot even when the signer never
/// sends a response.
pub struct PendingSignerOp<T: Send + 'static> {
    receiver: Option<Receiver<Result<T, SignerError>>>,
    cancel: Option<PendingCancel>,
}

impl<T: Send + 'static> PendingSignerOp<T> {
    fn new(receiver: Receiver<Result<T, SignerError>>, cancel: Option<PendingCancel>) -> Self {
        Self {
            receiver: Some(receiver),
            cancel,
        }
    }

    /// Block until the adapter resolves this operation.
    pub fn recv(mut self) -> Result<T, SignerError> {
        let result = self
            .receiver
            .take()
            .expect("pending signer receiver is consumed exactly once")
            .recv()
            .unwrap_or(Err(SignerError::Disconnected));
        self.cancel = None;
        result
    }

    fn recv_timeout(mut self, timeout: Duration) -> Result<T, SignerError> {
        match self
            .receiver
            .as_ref()
            .expect("pending signer receiver is consumed exactly once")
            .recv_timeout(timeout)
        {
            Ok(result) => {
                self.receiver = None;
                self.cancel = None;
                result
            }
            Err(RecvTimeoutError::Timeout) => Err(SignerError::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                self.receiver = None;
                self.cancel = None;
                Err(SignerError::Disconnected)
            }
        }
    }

    fn poll(&mut self) -> Option<Result<T, SignerError>> {
        match self
            .receiver
            .as_ref()
            .expect("pending signer receiver is consumed exactly once")
            .try_recv()
        {
            Ok(result) => {
                self.receiver = None;
                self.cancel = None;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.receiver = None;
                self.cancel = None;
                Some(Err(SignerError::Disconnected))
            }
        }
    }

    #[doc(hidden)]
    pub fn recv_or_cancel(
        mut self,
        cancelled: PendingSignerCancelled,
    ) -> Option<Result<T, SignerError>> {
        let outcome = crossbeam_channel::select_biased! {
            recv(cancelled.0) -> _ => None,
            recv(self.receiver.as_ref().expect("pending signer receiver is consumed exactly once")) -> result => {
                Some(result.unwrap_or(Err(SignerError::Disconnected)))
            }
        };
        self.receiver = None;
        if outcome.is_some() {
            self.cancel = None;
        } else if let Some(cancel) = self.cancel.take() {
            cancel();
        }
        outcome
    }
}

impl<T: Send + 'static> Drop for PendingSignerOp<T> {
    fn drop(&mut self) {
        if self.receiver.is_some() {
            if let Some(cancel) = self.cancel.take() {
                cancel();
            }
        }
    }
}

/// An op that may complete synchronously (`Ready`) or later (`Pending`).
/// `Pending` carries an NMP-owned asynchronous operation that yields exactly
/// one result when the operation completes — callers may poll it
/// (non-blocking, via [`SignerOp::poll`]) or blocks on it (via
/// [`SignerOp::wait`]); no tokio is ever pulled into the engine (D8).
///
/// `LocalKeySigner` resolves synchronously (`Ready`). `Nip46Signer` uses
/// `Pending` for an in-flight remote round-trip; the engine's recv loop drives
/// either kind of signer identically.
pub enum SignerOp<T: Send + 'static> {
    /// Operation completed synchronously.
    Ready(Result<T, SignerError>),
    /// Operation is pending — poll or wait on `rx` for the result.
    Pending(PendingSignerOp<T>),
}

impl<T: Send + 'static> SignerOp<T> {
    /// Construct a ready-now success.
    #[must_use]
    pub fn ok(value: T) -> Self {
        Self::Ready(Ok(value))
    }

    /// Construct a ready-now error.
    #[must_use]
    pub fn err(error: SignerError) -> Self {
        Self::Ready(Err(error))
    }

    /// Create an asynchronous operation and its NMP-owned completion door.
    ///
    /// The sender can move to any caller-owned thread without exposing the
    /// channel mechanism used internally by NMP.
    #[must_use]
    pub fn pending_channel() -> (PendingSignerSender<T>, Self) {
        Self::pending_channel_from_cancel(None)
    }

    /// Create an asynchronous operation with an adapter-owned cancellation
    /// hook and its NMP-owned completion door.
    #[must_use]
    pub fn pending_channel_with_cancel(
        cancel: impl FnOnce() + Send + 'static,
    ) -> (PendingSignerSender<T>, Self) {
        Self::pending_channel_from_cancel(Some(Box::new(cancel)))
    }

    fn pending_channel_from_cancel(
        cancel: Option<PendingCancel>,
    ) -> (PendingSignerSender<T>, Self) {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        (
            PendingSignerSender {
                sender: Arc::new(Mutex::new(Some(sender))),
            },
            Self::Pending(PendingSignerOp::new(receiver, cancel)),
        )
    }

    /// Block the current thread for up to `timeout` waiting for the result.
    pub fn wait(self, timeout: Duration) -> Result<T, SignerError> {
        match self {
            Self::Ready(r) => r,
            Self::Pending(pending) => pending.recv_timeout(timeout),
        }
    }

    /// Non-blocking poll. Returns `None` if still pending, `Some(result)` if
    /// completed. A disconnected channel surfaces as
    /// `Some(Err(SignerError::Disconnected))`.
    pub fn poll(&mut self) -> Option<Result<T, SignerError>> {
        match self {
            Self::Ready(_) => {
                let taken = std::mem::replace(self, Self::Ready(Err(SignerError::Unavailable)));
                match taken {
                    Self::Ready(r) => Some(r),
                    Self::Pending(_) => {
                        // `self` was just matched as `Ready` above, so `taken`
                        // (the value `replace` returned) is that same `Ready`.
                        unreachable!("SignerOp::poll observed inconsistent state")
                    }
                }
            }
            Self::Pending(pending) => {
                let result = pending.poll();
                if result.is_some() {
                    *self = Self::Ready(Err(SignerError::Unavailable));
                }
                result
            }
        }
    }
}

impl<T: Send + 'static> std::fmt::Debug for SignerOp<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(Ok(_)) => f.write_str("SignerOp::Ready(Ok(..))"),
            Self::Ready(Err(e)) => write!(f, "SignerOp::Ready(Err({e:?}))"),
            Self::Pending(_) => f.write_str("SignerOp::Pending(..)"),
        }
    }
}

/// Failure vocabulary for a `SignerOp` (§3.3). A3 may extend this closed
/// set as `LocalKeySigner`'s impls demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerError {
    /// The user or signer explicitly refused this operation. Terminal for
    /// the accepted write: retrying cannot change the decision.
    Rejected(String),
    /// The signer returned a response that cannot satisfy the frozen request
    /// (malformed, forged, or otherwise invalid). Terminal for this write.
    InvalidResponse(String),
    /// No usable signer session is currently available. Retryable after the
    /// matching capability is attached again.
    Unavailable,
    /// The operation did not resolve before the adapter's own deadline.
    /// Retryable; the engine deliberately owns no signer timeout policy.
    Timeout,
    /// The remote session or result channel ended before the operation
    /// resolved. Retryable after reconnection/reattachment.
    Disconnected,
}

impl SignerError {
    /// Whether this result is a final answer for the frozen write intent.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Rejected(_) | Self::InvalidResponse(_))
    }
}

impl std::fmt::Display for SignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(msg) => write!(f, "signer rejected: {msg}"),
            Self::InvalidResponse(msg) => write!(f, "invalid signer response: {msg}"),
            Self::Unavailable => write!(f, "signer unavailable"),
            Self::Timeout => write!(f, "signer op timed out"),
            Self::Disconnected => write!(f, "signer disconnected"),
        }
    }
}

impl std::error::Error for SignerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn terminal_classification_is_explicit() {
        assert!(SignerError::Rejected("no".to_string()).is_terminal());
        assert!(SignerError::InvalidResponse("forged".to_string()).is_terminal());
        assert!(!SignerError::Unavailable.is_terminal());
        assert!(!SignerError::Timeout.is_terminal());
        assert!(!SignerError::Disconnected.is_terminal());
    }

    #[test]
    fn dropped_pending_sender_is_disconnected() {
        let (sender, operation) = SignerOp::<()>::pending_channel();
        drop(sender);
        assert_eq!(
            operation.wait(Duration::from_millis(10)),
            Err(SignerError::Disconnected)
        );
    }

    #[test]
    fn timeout_cancels_the_adapter_owned_pending_slot_once() {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let cancelled_for_op = Arc::clone(&cancelled);
        let (_sender, op) = SignerOp::<()>::pending_channel_with_cancel(move || {
            cancelled_for_op.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(op.wait(Duration::from_millis(1)), Err(SignerError::Timeout));
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cloned_completion_door_resolves_exactly_once_with_typed_refusal() {
        let (sender, operation) = SignerOp::pending_channel();
        let competing_sender = sender.clone();
        sender.resolve(Ok(7u8)).unwrap();
        assert!(matches!(
            competing_sender.resolve(Ok(8u8)),
            Err(PendingSignerResolveError::AlreadyResolved(Ok(8)))
        ));
        assert_eq!(operation.wait(Duration::from_millis(10)), Ok(7));
    }
}
