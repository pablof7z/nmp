//! [`Subscription`] / [`DiagnosticsSubscription`] -- the app-facing handles
//! [`Engine::observe`](crate::Engine::observe)/
//! [`Engine::observe_diagnostics`](crate::Engine::observe_diagnostics)
//! return. Both fold `nmp-ffi`'s `NmpQueryHandle`/`NmpDiagnosticsHandle`
//! `Drop` discipline into the direct-Rust surface (canonical-facade-52-plan.md
//! §1): withdrawing a subscription is never a step an app must remember to
//! take, only one it may take early via [`Subscription::cancel`]/
//! [`DiagnosticsSubscription::cancel`].
//!
//! ## One read noun, windowing is a policy on it (#485)
//!
//! [`Engine::observe`](crate::Engine::observe) takes an optional [`Window`].
//! There is ONE [`Subscription`] type for both modes; delivery is DERIVED from
//! boundedness, never a free knob:
//!
//! - `window: None` ⇒ the unbounded query result, delivered as an exact
//!   rebased [`RowDelta`] transition and `window: None`. Intermediate reducer
//!   emits may be conflated for a slow observer, but applying the next frame
//!   to its last delivered state yields the newest state. The full row set is
//!   never redelivered — doing so would be the O(rows²) redelivery class this
//!   design exists to avoid.
//! - `Some(`[`Window::Expandable`]`)` ⇒ a bounded newest-first window
//!   delivered as conflated latest-state snapshot frames. Each [`Frame`]
//!   carries `window: Some(`[`WindowContents`]`)` (the complete current row
//!   set plus its [`WindowLoad`] growth fact) and receiver-derived `deltas`.
//!   Grow it with [`Subscription::request_rows`].

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvError, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nmp_engine::core::{AcquisitionEvidence, HistoryAdvanceError, Row, RowDelta, WindowLoad};
use nmp_engine::runtime::{
    DiagnosticsHandle, Handle, HistoryHandle, HistoryReceiver, LatestReceiver, QueryHandle,
    RowsReceiver,
};

use crate::diagnostics::DiagnosticsSnapshot;

/// Window policy on the read noun (#485). One real variant today; future
/// policies (e.g. latest-only, anchored) are new variants on this
/// `#[non_exhaustive]` enum, never new nouns — windowing stays a POLICY on
/// [`Engine::observe`](crate::Engine::observe), not a parallel `observe_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Window {
    /// A bounded newest-first window: it starts with `initial` canonical rows
    /// and grows only by explicit [`Subscription::request_rows`], never above
    /// `max`. `NonZeroUsize` makes an empty window unrepresentable; `initial`
    /// must not exceed `max` (validated at `observe`).
    Expandable {
        initial: NonZeroUsize,
        max: NonZeroUsize,
    },
}

/// The facade's single opaque cancellation capability (#52; codex-nova's
/// ratified shape), shared by [`Subscription`], [`WindowHandle`], and
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
/// subscription's own `Drop`: exactly ONE withdrawal ever fires, no matter
/// how many clones call [`Self::cancel`] or whether `Drop` also runs -- an
/// `AtomicBool` guard shared through the `Arc` makes the first caller win and
/// every other call a no-op.
#[derive(Clone)]
pub struct ObservationCancel {
    inner: Arc<CancelState>,
}

struct CancelState {
    done: AtomicBool,
    // A boxed closure rather than an enum over `Query`/`Window`/`Diagnostics`
    // variants: it captures exactly the one withdrawal action each
    // constructor below needs with no further vocabulary added here, and it is
    // what makes the guard itself trivially provable in isolation (see this
    // module's tests) without spinning up a real engine just to count calls.
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

/// One delivered observation frame (#485). ONE vocabulary for both delivery
/// modes; which fields are populated is derived from whether the observation
/// was opened with a [`Window`].
#[derive(Debug, Clone)]
pub struct Frame {
    /// The exact transition from this subscription's previously received
    /// frame. Unbounded observations: the producer-composed exact transition
    /// rebased onto the last batch actually delivered. Windowed observations:
    /// derived receiver-side against the last delivered frame. Both modes may
    /// conflate intermediate reducer emits for a slow observer.
    pub deltas: Vec<RowDelta>,
    /// The complete current bounded row set plus its growth fact. `Some` iff
    /// the subscription was opened with a [`Window`]. Unbounded observations
    /// never redeliver the full set (the O(rows²) redelivery class), so this
    /// is always `None` for them.
    pub window: Option<WindowContents>,
    /// The query's scoped, per-source acquisition evidence.
    pub evidence: AcquisitionEvidence,
}

/// The complete current contents of a bounded window plus its growth fact.
#[derive(Debug, Clone)]
pub struct WindowContents {
    /// Canonical newest-first (`created_at DESC, event_id ASC`) window rows.
    pub rows: Vec<Row>,
    /// Mechanical growth state of the window as of this frame.
    pub load: WindowLoad,
}

/// The ways [`Subscription::request_rows`]/[`WindowHandle::request_rows`] can
/// fail (#485). Growth is declarative — there is no opaque continuation token
/// to mismatch and no generation to go stale — so the only failures are the
/// structural one (this observation has no window) and the two ways a staged
/// advance can be rolled back before it ever became observable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestRowsError {
    /// This subscription observes the full live set; there is no window to
    /// grow. Only observations opened with a [`Window`] can `request_rows`.
    Unwindowed,
    /// The engine thread has shut down.
    EngineClosed,
    /// The canonical store could not serve the advance (the staged load was
    /// rolled back with exact prior-projection restoration).
    StoreUnavailable,
    /// No planned source could serve the advance (the staged load was rolled
    /// back with exact prior-projection restoration).
    TransportUnavailable { reason: String },
}

impl std::fmt::Display for RequestRowsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unwindowed => {
                f.write_str("request_rows requires a windowed observation; this one is unbounded")
            }
            Self::EngineClosed => f.write_str("engine already shut down"),
            Self::StoreUnavailable => {
                f.write_str("window advance could not read or resolve the canonical store")
            }
            Self::TransportUnavailable { reason } => {
                write!(f, "window advance transport unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for RequestRowsError {}

impl RequestRowsError {
    fn from_advance(error: HistoryAdvanceError) -> Self {
        match error {
            HistoryAdvanceError::StoreUnavailable => Self::StoreUnavailable,
            HistoryAdvanceError::TransportUnavailable { reason } => {
                Self::TransportUnavailable { reason }
            }
        }
    }
}

/// A live observation. `Drop` withdraws it -- an app never needs a second
/// container or lifecycle hook to make that happen; call [`Self::cancel`]
/// instead of `drop`ping the value for an explicit early teardown that reads
/// as intent rather than scope exit.
///
/// One type serves both the unbounded delta stream and a bounded [`Window`];
/// [`Self::window_handle`] returns `Some` iff this observation is windowed.
pub struct Subscription {
    cancel: ObservationCancel,
    delivery: Delivery,
    window: Option<WindowHandle>,
}

enum Delivery {
    Unbounded(RowsReceiver),
    Windowed(HistoryReceiver),
}

/// Cloneable growth capability for a windowed observation (#485).
///
/// [`Subscription::recv`] blocks, so a drain thread typically owns the
/// `Subscription` outright; this handle lets a SEPARATE thread grow the same
/// window via [`Self::request_rows`] or withdraw it via [`Self::cancel`]. The
/// embedded cancellation guard is the SAME guard the owning [`Subscription`]
/// routes its `Drop` through, so either side withdraws the whole observation
/// exactly once.
#[derive(Clone)]
pub struct WindowHandle {
    cancel: ObservationCancel,
    engine: Handle,
    handle: HistoryHandle,
}

impl WindowHandle {
    /// Monotonically raise the window's row target to at least `at_least`,
    /// clamped to the window's declared `max`. Idempotent; a value at or below
    /// the current target is a no-op. Growth outcomes arrive as [`WindowLoad`]
    /// facts in subsequent frames — this call only declares the intent.
    pub fn request_rows(&self, at_least: usize) -> Result<(), RequestRowsError> {
        match self.engine.request_rows(self.handle, at_least) {
            None => Err(RequestRowsError::EngineClosed),
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(RequestRowsError::from_advance(error)),
        }
    }

    /// Withdraw the whole observation now (idempotent; converges on the same
    /// one-withdrawal guard as the owning [`Subscription`]'s `Drop`).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Subscription {
    pub(crate) fn new(handle: Handle, query_handle: QueryHandle, rows: RowsReceiver) -> Self {
        Self {
            cancel: ObservationCancel::new(move || handle.unsubscribe(query_handle)),
            delivery: Delivery::Unbounded(rows),
            window: None,
        }
    }

    pub(crate) fn new_windowed(
        engine: Handle,
        handle: HistoryHandle,
        batches: HistoryReceiver,
    ) -> Self {
        let cancel_engine = engine.clone();
        let cancel = ObservationCancel::new(move || cancel_engine.unsubscribe_history(handle));
        let window = WindowHandle {
            cancel: cancel.clone(),
            engine,
            handle,
        };
        Self {
            cancel,
            delivery: Delivery::Windowed(batches),
            window: Some(window),
        }
    }

    /// Block for the next observation [`Frame`]. Unbounded: the exact rebased
    /// transition to the newest state. Windowed: the newest self-contained
    /// bounded state. Intermediate reducer emits may be conflated in either
    /// mode, while the returned `deltas` always describe an exact transition
    /// from this receiver's last return. `Err` once the engine thread has shut
    /// down and the channel disconnects.
    pub fn recv(&self) -> Result<Frame, RecvError> {
        match &self.delivery {
            Delivery::Unbounded(rows) => {
                let (deltas, evidence) = rows.recv()?;
                Ok(Frame {
                    deltas,
                    window: None,
                    evidence,
                })
            }
            Delivery::Windowed(batches) => {
                let batch = batches.recv()?;
                Ok(Frame {
                    deltas: batch.deltas,
                    window: Some(WindowContents {
                        rows: batch.rows,
                        load: batch.load,
                    }),
                    evidence: batch.evidence,
                })
            }
        }
    }

    /// Wait at most `timeout` for the next [`Frame`]. The same stream as
    /// [`Self::recv`], with no polling or second cache.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Frame, RecvTimeoutError> {
        match &self.delivery {
            Delivery::Unbounded(rows) => {
                let (deltas, evidence) = rows.recv_timeout(timeout)?;
                Ok(Frame {
                    deltas,
                    window: None,
                    evidence,
                })
            }
            Delivery::Windowed(batches) => {
                let batch = batches.recv_timeout(timeout)?;
                Ok(Frame {
                    deltas: batch.deltas,
                    window: Some(WindowContents {
                        rows: batch.rows,
                        load: batch.load,
                    }),
                    evidence: batch.evidence,
                })
            }
        }
    }

    /// Windowed observations only: monotonically raise the window's row target
    /// to at least `at_least`, clamped to the declared `max`. Idempotent; a
    /// value at or below the current target is a no-op. Returns
    /// [`RequestRowsError::Unwindowed`] for an unbounded observation. Growth
    /// outcomes arrive as [`WindowLoad`] facts in subsequent frames.
    pub fn request_rows(&self, at_least: usize) -> Result<(), RequestRowsError> {
        match &self.window {
            Some(handle) => handle.request_rows(at_least),
            None => Err(RequestRowsError::Unwindowed),
        }
    }

    /// A cloneable growth/cancel capability for a windowed observation.
    /// `None` for unbounded observations — the capability's existence is
    /// derived from the window policy, not a separate flag.
    #[must_use]
    pub fn window_handle(&self) -> Option<WindowHandle> {
        self.window.clone()
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
    snapshots: LatestReceiver<nmp_engine::core::DiagnosticsSnapshot>,
}

impl DiagnosticsSubscription {
    pub(crate) fn new(
        diag_handle: DiagnosticsHandle,
        snapshots: LatestReceiver<nmp_engine::core::DiagnosticsSnapshot>,
    ) -> Self {
        Self {
            cancel: ObservationCancel::new(move || diag_handle.cancel()),
            snapshots,
        }
    }

    /// Block for the next [`DiagnosticsSnapshot`] -- delivers the CURRENT
    /// snapshot immediately on the first call, then a fresh one on every
    /// recompile/EOSE-driven coverage change. `None` once the stream is
    /// withdrawn. This is the ONE delivery boundary where the engine's
    /// snapshot is converted into the facade-owned mirror (see
    /// [`crate::diagnostics`]'s module doc).
    pub fn recv(&self) -> Option<DiagnosticsSnapshot> {
        self.snapshots.recv().map(DiagnosticsSnapshot::from_engine)
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
