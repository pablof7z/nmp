//! The pollable thunk (§3.3, HARVEST `nmp-signer-iface::op`).
//!
//! #704: the completion door is a hand-rolled `Mutex`+`Condvar`+`Waker`
//! primitive (no channel crate, no runtime). It serves BOTH a blocking
//! consumer (`recv`/`recv_timeout`/`wait`, for direct-Rust callers) and an
//! async consumer (`poll_recv`/`Future`, for the engine's shared runtime) over
//! the same one-shot slot. A `Future` needs no runtime, so `nmp-signer` stays
//! runtime-free while the awaiting engine holds NO OS thread while a signer
//! round-trip is outstanding. The cancellation door is bound INTO the primitive
//! (`Canceller`) so both consumers wake on cancel; dropping an unresolved op (or
//! its awaiting future) runs the adapter cancel hook exactly once.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

#[doc(hidden)]
pub type PendingCancel = Box<dyn FnOnce() + Send + 'static>;

enum DoorLifecycle<T> {
    /// No sender has claimed the result and the consumer is still present.
    Open,
    /// A sender claimed the result. `None` means the consumer already took it.
    Resolved(Option<Result<T, SignerError>>),
    /// Cancellation won before any sender claimed the result.
    CancelledUnresolved,
    /// Cancellation won and a later sender claim was truthfully refused.
    CancelledResolved,
    /// The consumer was dropped before any sender claimed the result.
    ReceiverGoneUnresolved,
    /// The consumer is gone and the one sender claim has already been spent.
    ReceiverGoneResolved,
}

struct DoorState<T> {
    /// The result-slot and consumer lifecycle as one legal state.
    lifecycle: DoorLifecycle<T>,
    /// A parked async consumer's waker (at most one; the op is single-consumer).
    waker: Option<Waker>,
}

/// The shared one-shot completion slot behind a [`PendingSignerSender`] /
/// [`PendingSignerOp`] pair.
struct Door<T> {
    state: Mutex<DoorState<T>>,
    changed: Condvar,
    /// Live sender clones; the last drop without a resolve disconnects the op.
    senders: AtomicUsize,
}

impl<T> Door<T> {
    fn wake(state: &mut DoorState<T>) -> Option<Waker> {
        state.waker.take()
    }

    fn cancel(state: &mut DoorState<T>) {
        let previous = std::mem::replace(&mut state.lifecycle, DoorLifecycle::Open);
        state.lifecycle = match previous {
            DoorLifecycle::Open => DoorLifecycle::CancelledUnresolved,
            DoorLifecycle::Resolved(_) => DoorLifecycle::CancelledResolved,
            DoorLifecycle::CancelledUnresolved => DoorLifecycle::CancelledUnresolved,
            DoorLifecycle::CancelledResolved => DoorLifecycle::CancelledResolved,
            DoorLifecycle::ReceiverGoneUnresolved => DoorLifecycle::ReceiverGoneUnresolved,
            DoorLifecycle::ReceiverGoneResolved => DoorLifecycle::ReceiverGoneResolved,
        };
    }

    fn mark_receiver_gone(state: &mut DoorState<T>) {
        let previous = std::mem::replace(&mut state.lifecycle, DoorLifecycle::Open);
        state.lifecycle = match previous {
            DoorLifecycle::Open
            | DoorLifecycle::CancelledUnresolved
            | DoorLifecycle::ReceiverGoneUnresolved => DoorLifecycle::ReceiverGoneUnresolved,
            DoorLifecycle::Resolved(_)
            | DoorLifecycle::CancelledResolved
            | DoorLifecycle::ReceiverGoneResolved => DoorLifecycle::ReceiverGoneResolved,
        };
    }
}

/// NMP-owned completion door for one asynchronous signer operation.
///
/// Clones share one terminal slot: the first call to [`Self::resolve`] owns
/// the result, and every later call receives a typed `AlreadyResolved` error.
/// No channel implementation type crosses the public signer boundary.
pub struct PendingSignerSender<T: Send + 'static> {
    door: Arc<Door<T>>,
}

impl<T: Send + 'static> Clone for PendingSignerSender<T> {
    fn clone(&self) -> Self {
        self.door.senders.fetch_add(1, Ordering::AcqRel);
        Self {
            door: Arc::clone(&self.door),
        }
    }
}

impl<T: Send + 'static> Drop for PendingSignerSender<T> {
    fn drop(&mut self) {
        if self.door.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Last sender gone; if it never resolved, wake the consumer to a
            // `Disconnected` end.
            let waker = {
                let mut state = self.door.state.lock().unwrap();
                match state.lifecycle {
                    DoorLifecycle::Resolved(_)
                    | DoorLifecycle::CancelledResolved
                    | DoorLifecycle::ReceiverGoneResolved => None,
                    DoorLifecycle::Open
                    | DoorLifecycle::CancelledUnresolved
                    | DoorLifecycle::ReceiverGoneUnresolved => {
                        self.door.changed.notify_all();
                        Door::wake(&mut state)
                    }
                }
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

impl<T: Send + 'static> PendingSignerSender<T> {
    /// Resolve the matching pending operation exactly once.
    pub fn resolve(
        &self,
        result: Result<T, SignerError>,
    ) -> Result<(), PendingSignerResolveError<T>> {
        let waker = {
            let mut state = self.door.state.lock().unwrap();
            match state.lifecycle {
                DoorLifecycle::Open => {
                    state.lifecycle = DoorLifecycle::Resolved(Some(result));
                }
                DoorLifecycle::CancelledUnresolved => {
                    state.lifecycle = DoorLifecycle::CancelledResolved;
                    return Err(PendingSignerResolveError::ReceiverDropped(result));
                }
                DoorLifecycle::ReceiverGoneUnresolved => {
                    state.lifecycle = DoorLifecycle::ReceiverGoneResolved;
                    return Err(PendingSignerResolveError::ReceiverDropped(result));
                }
                DoorLifecycle::Resolved(_)
                | DoorLifecycle::CancelledResolved
                | DoorLifecycle::ReceiverGoneResolved => {
                    return Err(PendingSignerResolveError::AlreadyResolved(result));
                }
            }
            self.door.changed.notify_all();
            Door::wake(&mut state)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
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

/// A cloneable handle that cancels one [`PendingSignerOp`] (#704). Firing it
/// wakes both a blocking and an async consumer to a "cancelled" (no-result)
/// end, and runs the adapter cancel hook once when the op is consumed/dropped.
/// Cancellation is bound into the door itself, so no separate channel exists.
#[doc(hidden)]
#[derive(Clone)]
pub struct Canceller<T: Send + 'static> {
    door: Arc<Door<T>>,
}

impl<T: Send + 'static> Canceller<T> {
    #[doc(hidden)]
    pub fn cancel(&self) {
        let waker = {
            let mut state = self.door.state.lock().unwrap();
            Door::cancel(&mut state);
            self.door.changed.notify_all();
            Door::wake(&mut state)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

enum PendingSignerLifecycle {
    Pending(Option<PendingCancel>),
    Finished,
}

#[derive(Clone, Copy)]
enum PendingFinish {
    CancelAdapter,
    SuppressCancel,
}

/// One cancellable asynchronous signer result.
///
/// Dropping an unfinished value (including its awaiting future) invokes the
/// adapter-owned cancellation hook exactly once, releasing the bounded
/// correlation slot even when the signer never responds.
pub struct PendingSignerOp<T: Send + 'static> {
    door: Arc<Door<T>>,
    lifecycle: PendingSignerLifecycle,
}

impl<T: Send + 'static> PendingSignerOp<T> {
    fn new(door: Arc<Door<T>>, cancel: Option<PendingCancel>) -> Self {
        Self {
            door,
            lifecycle: PendingSignerLifecycle::Pending(cancel),
        }
    }

    /// A cancellation handle for this operation (#704). Cancelling ends a
    /// blocking or async consumer with no result and runs the cancel hook.
    #[doc(hidden)]
    #[must_use]
    pub fn canceller(&self) -> Canceller<T> {
        Canceller {
            door: Arc::clone(&self.door),
        }
    }

    /// Drain a terminal outcome from the locked state, or `None` if still open.
    fn take_terminal(
        state: &mut DoorState<T>,
        senders: usize,
    ) -> Option<(Result<T, SignerError>, PendingFinish)> {
        match &mut state.lifecycle {
            DoorLifecycle::CancelledUnresolved | DoorLifecycle::CancelledResolved => {
                Some((Err(SignerError::Disconnected), PendingFinish::CancelAdapter))
            }
            DoorLifecycle::Resolved(value) => Some((
                value.take().unwrap_or(Err(SignerError::Disconnected)),
                PendingFinish::SuppressCancel,
            )),
            DoorLifecycle::Open if senders == 0 => Some((
                Err(SignerError::Disconnected),
                PendingFinish::SuppressCancel,
            )),
            DoorLifecycle::ReceiverGoneUnresolved | DoorLifecycle::ReceiverGoneResolved => Some((
                Err(SignerError::Disconnected),
                PendingFinish::SuppressCancel,
            )),
            DoorLifecycle::Open => None,
        }
    }

    fn finish(&mut self, disposition: PendingFinish) {
        let previous = std::mem::replace(&mut self.lifecycle, PendingSignerLifecycle::Finished);
        let PendingSignerLifecycle::Pending(cancel) = previous else {
            return;
        };
        if matches!(disposition, PendingFinish::CancelAdapter) {
            if let Some(cancel) = cancel {
                cancel();
            }
        }
    }

    /// Block until the adapter resolves this operation (or it is cancelled /
    /// disconnected).
    pub fn recv(mut self) -> Result<T, SignerError> {
        let mut state = self.door.state.lock().unwrap();
        loop {
            let senders = self.door.senders.load(Ordering::Acquire);
            if let Some((outcome, disposition)) = Self::take_terminal(&mut state, senders) {
                drop(state);
                self.finish(disposition);
                return outcome;
            }
            state = self.door.changed.wait(state).unwrap();
        }
    }

    /// Block at most `timeout` for the result (`Timeout` on elapse).
    pub fn recv_timeout(mut self, timeout: Duration) -> Result<T, SignerError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.door.state.lock().unwrap();
        loop {
            let senders = self.door.senders.load(Ordering::Acquire);
            if let Some((outcome, disposition)) = Self::take_terminal(&mut state, senders) {
                drop(state);
                self.finish(disposition);
                return outcome;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(state);
                self.finish(PendingFinish::CancelAdapter);
                return Err(SignerError::Timeout);
            }
            let (next, wait) = self.door.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            if wait.timed_out()
                && Self::take_terminal(&mut state, self.door.senders.load(Ordering::Acquire))
                    .is_none()
            {
                drop(state);
                self.finish(PendingFinish::CancelAdapter);
                return Err(SignerError::Timeout);
            }
        }
    }

    /// Non-blocking poll of the completion slot (`None` while pending).
    fn try_poll(&mut self) -> Option<Result<T, SignerError>> {
        let mut state = self.door.state.lock().unwrap();
        let senders = self.door.senders.load(Ordering::Acquire);
        let terminal = Self::take_terminal(&mut state, senders);
        if let Some((_, disposition)) = terminal.as_ref() {
            let disposition = *disposition;
            drop(state);
            self.finish(disposition);
        }
        terminal.map(|(outcome, _)| outcome)
    }

    /// Waker-aware poll (#704): the engine awaits this on its shared runtime,
    /// holding no OS thread while the signer round-trip is outstanding.
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, SignerError>> {
        let mut state = self.door.state.lock().unwrap();
        let senders = self.door.senders.load(Ordering::Acquire);
        if let Some((outcome, disposition)) = Self::take_terminal(&mut state, senders) {
            state.waker = None;
            drop(state);
            self.finish(disposition);
            return Poll::Ready(outcome);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Awaiting the pending op resolves it on the shared runtime with no thread
/// held; dropping the future before completion fires the cancel hook.
impl<T: Send + 'static> Future for PendingSignerOp<T> {
    type Output = Result<T, SignerError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_recv(cx)
    }
}

impl<T: Send + 'static> Drop for PendingSignerOp<T> {
    fn drop(&mut self) {
        // Mark the consumer gone so a racing `resolve` returns `ReceiverDropped`
        // rather than dropping the result silently.
        if let Ok(mut state) = self.door.state.lock() {
            Door::mark_receiver_gone(&mut state);
        }
        let previous = std::mem::replace(&mut self.lifecycle, PendingSignerLifecycle::Finished);
        if let PendingSignerLifecycle::Pending(Some(cancel)) = previous {
            cancel();
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
        let door = Arc::new(Door {
            state: Mutex::new(DoorState {
                lifecycle: DoorLifecycle::Open,
                waker: None,
            }),
            changed: Condvar::new(),
            senders: AtomicUsize::new(1),
        });
        (
            PendingSignerSender { door: door.clone() },
            Self::Pending(PendingSignerOp::new(door, cancel)),
        )
    }

    /// Await the result on the caller's async runtime (#704), holding no OS
    /// thread. `Ready` resolves immediately; `Pending` awaits the completion
    /// door. Dropping the returned future before completion runs the adapter
    /// cancel hook.
    pub async fn recv_async(self) -> Result<T, SignerError> {
        match self {
            Self::Ready(r) => r,
            Self::Pending(pending) => pending.await,
        }
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
                let result = pending.try_poll();
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
    fn cancel_before_resolve_runs_hook_and_spends_the_resolution_slot() {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let cancelled_for_op = Arc::clone(&cancelled);
        let (sender, operation) = SignerOp::<u8>::pending_channel_with_cancel(move || {
            cancelled_for_op.fetch_add(1, Ordering::SeqCst);
        });
        let SignerOp::Pending(pending) = operation else {
            unreachable!()
        };
        pending.canceller().cancel();

        assert_eq!(pending.recv(), Err(SignerError::Disconnected));
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert!(matches!(
            sender.resolve(Ok(7)),
            Err(PendingSignerResolveError::ReceiverDropped(Ok(7)))
        ));
        assert!(matches!(
            sender.resolve(Ok(8)),
            Err(PendingSignerResolveError::AlreadyResolved(Ok(8)))
        ));
    }

    #[test]
    fn dropped_consumer_runs_hook_and_spends_the_resolution_slot() {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let cancelled_for_op = Arc::clone(&cancelled);
        let (sender, operation) = SignerOp::<u8>::pending_channel_with_cancel(move || {
            cancelled_for_op.fetch_add(1, Ordering::SeqCst);
        });

        drop(operation);
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert!(matches!(
            sender.resolve(Ok(7)),
            Err(PendingSignerResolveError::ReceiverDropped(Ok(7)))
        ));
        assert!(matches!(
            sender.resolve(Ok(8)),
            Err(PendingSignerResolveError::AlreadyResolved(Ok(8)))
        ));
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
