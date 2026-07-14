//! The pollable thunk (§3.3, HARVEST `nmp-signer-iface::op`).

use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};

#[doc(hidden)]
pub type PendingCancel = Box<dyn FnOnce() + Send + 'static>;

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
    pub fn into_parts(mut self) -> (Receiver<Result<T, SignerError>>, Option<PendingCancel>) {
        let receiver = self
            .receiver
            .take()
            .expect("pending signer receiver is consumed exactly once");
        let cancel = self.cancel.take();
        (receiver, cancel)
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
/// `Pending` carries a receiver that yields exactly one result when the
/// operation completes — the engine's blocking recv loop polls it
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

    /// Construct an asynchronous operation without adapter cancellation.
    /// This is the portable constructor for custom `nmp`-only signers.
    #[must_use]
    pub fn pending(receiver: Receiver<Result<T, SignerError>>) -> Self {
        Self::Pending(PendingSignerOp::new(receiver, None))
    }

    /// Construct an asynchronous operation with an adapter-owned cancellation
    /// hook. Governed callers invoke the hook when their exact operation is
    /// cancelled or the owning engine shuts down.
    #[must_use]
    pub fn pending_with_cancel(
        receiver: Receiver<Result<T, SignerError>>,
        cancel: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self::Pending(PendingSignerOp::new(receiver, Some(Box::new(cancel))))
    }

    pub(crate) fn pending_from_parts(
        receiver: Receiver<Result<T, SignerError>>,
        cancel: Option<PendingCancel>,
    ) -> Self {
        Self::Pending(PendingSignerOp::new(receiver, cancel))
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
        let (tx, rx) = crossbeam_channel::unbounded::<Result<(), SignerError>>();
        drop(tx);
        assert_eq!(
            SignerOp::pending(rx).wait(Duration::from_millis(10)),
            Err(SignerError::Disconnected)
        );
    }

    #[test]
    fn timeout_cancels_the_adapter_owned_pending_slot_once() {
        let (_tx, rx) = crossbeam_channel::unbounded::<Result<(), SignerError>>();
        let cancelled = Arc::new(AtomicUsize::new(0));
        let cancelled_for_op = Arc::clone(&cancelled);
        let op = SignerOp::pending_with_cancel(rx, move || {
            cancelled_for_op.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(op.wait(Duration::from_millis(1)), Err(SignerError::Timeout));
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    }
}
