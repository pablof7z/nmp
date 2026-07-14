//! [`Subscription`] / [`DiagnosticsSubscription`] -- the app-facing handles
//! [`Engine::observe`](crate::Engine::observe)/
//! [`Engine::observe_diagnostics`](crate::Engine::observe_diagnostics)
//! return. Both fold `nmp-ffi`'s `NmpQueryHandle`/`NmpDiagnosticsHandle`
//! `Drop` discipline into the direct-Rust surface (canonical-facade-52-plan.md
//! §1): withdrawing a subscription is never a step an app must remember to
//! take, only one it may take early via [`Subscription::cancel`]/
//! [`DiagnosticsSubscription::cancel`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvError, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nmp_engine::core::DiagnosticsSnapshot;
use nmp_engine::core::{HistoryBatch, HistoryContinuation, HistoryLoadError};
use nmp_engine::runtime::{
    DiagnosticsHandle, Handle, HistoryHandle, LatestReceiver, QueryHandle, RowsMsg,
};

/// The facade's single opaque cancellation capability (#52; codex-nova's
/// ratified shape), shared by both [`Subscription`] and
/// [`DiagnosticsSubscription`]. Exposes only [`Self::cancel`] -- no
/// `Handle`/`QueryHandle`/`DiagnosticsHandle` (the raw mechanism-capability
/// types `nmp-ffi` used to hold directly) ever appears in a stable facade
/// signature; only this opaque, `Clone + Send + Sync` token does. `recv()`
/// blocks, so a dedicated drain loop (e.g. `nmp-ffi`'s `NmpQueryHandle`)
/// must own the whole `Subscription`/`DiagnosticsSubscription` outright;
/// this token is what lets a SEPARATE handle still trigger withdrawal
/// immediately from elsewhere, rather than waiting for the drain loop's
/// next `recv()` to notice the disconnect.
///
/// Cancellation is idempotent across every clone AND the owning
/// subscription's own `Drop`: exactly ONE withdrawal (`Handle::
/// unsubscribe`/`DiagnosticsHandle::cancel`) ever fires, no matter how many
/// clones call [`Self::cancel`] or whether `Drop` also runs -- an
/// `AtomicBool` guard shared through the `Arc` makes the first caller (in
/// either role) win and every other call a no-op. `Subscription`/
/// `DiagnosticsSubscription` each hold one of these (built in their own
/// `new`) and route their own `Drop` through that SAME instance's
/// [`Self::cancel`], so a caller holding a clone and the owning value's own
/// teardown converge on one guarded action -- never a double-withdrawal.
#[derive(Clone)]
pub struct ObservationCancel {
    inner: Arc<CancelState>,
}

struct CancelState {
    done: AtomicBool,
    // A boxed closure rather than an enum over `Query`/`Diagnostics`
    // variants: it captures exactly the one withdrawal action each
    // constructor below needs (`Handle::unsubscribe`/`DiagnosticsHandle::
    // cancel`) with no further vocabulary added here, and it is what makes
    // the guard itself trivially provable in isolation (see this module's
    // tests) without spinning up a real engine just to count calls.
    action: Box<dyn Fn() + Send + Sync>,
}

impl ObservationCancel {
    fn new(action: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(CancelState {
                done: AtomicBool::new(false),
                action: Box::new(action),
            }),
        }
    }

    /// Withdraw the underlying subscription/diagnostics stream now. Safe to
    /// call from any clone, any number of times, and safe to race the
    /// owning value's own `Drop` -- the first call (from whichever clone,
    /// or `Drop`) wins the guard; every other call, including a
    /// post-`shutdown` one, is a safe no-op.
    pub fn cancel(&self) {
        if self.inner.done.swap(true, Ordering::AcqRel) {
            return;
        }
        (self.inner.action)();
    }
}

/// A live query subscription. `Drop` withdraws it -- an app never needs a
/// second container or lifecycle hook to make that happen; call
/// [`Self::cancel`] instead of `drop`ping the value for an explicit early
/// teardown that reads as intent rather than scope exit.
pub struct Subscription {
    cancel: ObservationCancel,
    rows: std::sync::mpsc::Receiver<RowsMsg>,
}

/// A coordinated, bounded history read. The engine owns every acquisition
/// handle and cursor generation; this facade retains only the opaque runtime
/// capability needed to request the next older window and cancel the whole
/// session.
pub struct HistorySubscription {
    cancel: ObservationCancel,
    engine: Handle,
    handle: HistoryHandle,
    batches: std::sync::mpsc::Receiver<HistoryBatch>,
}

impl HistorySubscription {
    pub(crate) fn new(
        engine: Handle,
        handle: HistoryHandle,
        batches: std::sync::mpsc::Receiver<HistoryBatch>,
    ) -> Self {
        let cancel_engine = engine.clone();
        Self {
            cancel: ObservationCancel::new(move || cancel_engine.unsubscribe_history(handle)),
            engine,
            handle,
            batches,
        }
    }

    pub fn recv(&self) -> Result<HistoryBatch, RecvError> {
        self.batches.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<HistoryBatch, RecvTimeoutError> {
        self.batches.recv_timeout(timeout)
    }

    pub fn load_older(&self, continuation: HistoryContinuation) -> Result<(), HistoryLoadError> {
        self.engine.load_older(self.handle, continuation)
    }

    #[must_use]
    pub fn cancel_handle(&self) -> ObservationCancel {
        self.cancel.clone()
    }

    pub fn cancel(self) {}
}

impl Drop for HistorySubscription {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Subscription {
    pub(crate) fn new(
        handle: Handle,
        query_handle: QueryHandle,
        rows: std::sync::mpsc::Receiver<RowsMsg>,
    ) -> Self {
        Self {
            cancel: ObservationCancel::new(move || handle.unsubscribe(query_handle)),
            rows,
        }
    }

    /// Block for the next `RowsMsg` batch (raw rows + this query's scoped
    /// acquisition evidence). `Err` once the engine thread has shut down and the
    /// channel disconnects.
    pub fn recv(&self) -> Result<RowsMsg, RecvError> {
        self.rows.recv()
    }

    /// Wait at most `timeout` for the next live-query frame. This is the
    /// same stream as [`Self::recv`], with no polling or second cache; it is
    /// useful to protocol actions whose acquisition phase has an explicit,
    /// bounded deadline.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<RowsMsg, RecvTimeoutError> {
        self.rows.recv_timeout(timeout)
    }

    /// Withdraw the subscription now, rather than waiting for `Drop`.
    /// Equivalent to `drop(subscription)` -- exists as a named method for
    /// call sites where an explicit early teardown reads more clearly than
    /// a scope exit.
    pub fn cancel(self) {}

    /// The facade's opaque cancellation capability for this subscription --
    /// see [`ObservationCancel`]'s doc. A clone lets a caller trigger
    /// withdrawal from elsewhere (e.g. a dedicated drain thread that owns
    /// `recv()`, since it blocks) while this value's own `Drop` still
    /// converges on the same one-withdrawal guard.
    pub fn cancel_handle(&self) -> ObservationCancel {
        self.cancel.clone()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A live diagnostics stream. Same `Drop` discipline as [`Subscription`].
pub struct DiagnosticsSubscription {
    cancel: ObservationCancel,
    snapshots: LatestReceiver<DiagnosticsSnapshot>,
}

impl DiagnosticsSubscription {
    pub(crate) fn new(
        diag_handle: DiagnosticsHandle,
        snapshots: LatestReceiver<DiagnosticsSnapshot>,
    ) -> Self {
        Self {
            cancel: ObservationCancel::new(move || diag_handle.cancel()),
            snapshots,
        }
    }

    /// Block for the next `DiagnosticsSnapshot` -- delivers the CURRENT
    /// snapshot immediately on the first call, then a fresh one on every
    /// recompile/EOSE-driven coverage change. `None` once the stream is
    /// withdrawn.
    pub fn recv(&self) -> Option<DiagnosticsSnapshot> {
        self.snapshots.recv()
    }

    /// Withdraw this diagnostics observer now, rather than waiting for
    /// `Drop`.
    pub fn cancel(self) {}

    /// Same rationale as [`Subscription::cancel_handle`] -- returns the
    /// SAME [`ObservationCancel`] type (the one facade-owned cancel token,
    /// shared by both subscription kinds).
    pub fn cancel_handle(&self) -> ObservationCancel {
        self.cancel.clone()
    }
}

impl Drop for DiagnosticsSubscription {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// codex-nova's non-negotiable proof #2, isolated from any real engine:
    /// repeated `cancel()` calls across clones, PLUS the guard firing again
    /// (standing in for the owning value's `Drop`, which routes through the
    /// same `ObservationCancel::cancel()` call), must trigger the
    /// underlying withdrawal action EXACTLY ONCE.
    #[test]
    fn cancels_exactly_once_across_clones_and_a_drop_equivalent_call() {
        let count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&count);
        let cancel = ObservationCancel::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });

        let clone_a = cancel.clone();
        let clone_b = cancel.clone();

        cancel.cancel();
        clone_a.cancel();
        clone_b.cancel();
        cancel.cancel(); // simulates `Drop` firing after callers already cancelled

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "the underlying withdrawal action must fire exactly once, no matter how many \
             clones (or a subsequent Drop) call cancel()"
        );
    }

    /// The reverse ordering: the guard fires first (standing in for
    /// `Drop`), and every clone's later `cancel()` call must still be a
    /// no-op rather than double-firing.
    #[test]
    fn a_drop_equivalent_call_before_clones_still_cancels_exactly_once() {
        let count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&count);
        let cancel = ObservationCancel::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });

        let clone_a = cancel.clone();
        let clone_b = cancel.clone();

        cancel.cancel(); // simulates `Drop` firing first
        clone_a.cancel();
        clone_b.cancel();

        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
