//! The pollable thunk (§3.3, HARVEST `nmp-signer-iface::op`).

use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

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
    Pending(Receiver<Result<T, SignerError>>),
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

    /// Block the current thread for up to `timeout` waiting for the result.
    pub fn wait(self, timeout: Duration) -> Result<T, SignerError> {
        match self {
            Self::Ready(r) => r,
            Self::Pending(rx) => match rx.recv_timeout(timeout) {
                Ok(r) => r,
                Err(RecvTimeoutError::Timeout) => Err(SignerError::Timeout),
                Err(RecvTimeoutError::Disconnected) => Err(SignerError::Disconnected),
            },
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
            Self::Pending(rx) => match rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(SignerError::Disconnected)),
            },
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
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), SignerError>>();
        drop(tx);
        assert_eq!(
            SignerOp::Pending(rx).wait(Duration::from_millis(10)),
            Err(SignerError::Disconnected)
        );
    }
}
