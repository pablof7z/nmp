//! [`Subscription`] / [`DiagnosticsSubscription`] -- the app-facing handles
//! [`Engine::observe`](crate::Engine::observe)/
//! [`Engine::observe_diagnostics`](crate::Engine::observe_diagnostics)
//! return. Both fold `nmp-ffi`'s `NmpQueryHandle`/`NmpDiagnosticsHandle`
//! `Drop` discipline into the direct-Rust surface (canonical-facade-52-plan.md
//! §1): withdrawing a subscription is never a step an app must remember to
//! take, only one it may take early via [`Subscription::cancel`]/
//! [`DiagnosticsSubscription::cancel`].

use std::sync::mpsc::RecvError;

use nmp_engine::core::DiagnosticsSnapshot;
use nmp_engine::runtime::{DiagnosticsHandle, Handle, LatestReceiver, QueryHandle, RowsMsg};

/// A live query subscription. `Drop` withdraws it -- an app never needs a
/// second container or lifecycle hook to make that happen; call
/// [`Self::cancel`] instead of `drop`ping the value for an explicit early
/// teardown that reads as intent rather than scope exit.
pub struct Subscription {
    handle: Handle,
    query_handle: QueryHandle,
    rows: std::sync::mpsc::Receiver<RowsMsg>,
}

impl Subscription {
    pub(crate) fn new(
        handle: Handle,
        query_handle: QueryHandle,
        rows: std::sync::mpsc::Receiver<RowsMsg>,
    ) -> Self {
        Self {
            handle,
            query_handle,
            rows,
        }
    }

    /// Block for the next `RowsMsg` batch (raw rows + this query's aggregate
    /// coverage). `Err` once the engine thread has shut down and the
    /// channel disconnects.
    pub fn recv(&self) -> Result<RowsMsg, RecvError> {
        self.rows.recv()
    }

    /// Withdraw the subscription now, rather than waiting for `Drop`.
    /// Equivalent to `drop(subscription)` -- exists as a named method for
    /// call sites where an explicit early teardown reads more clearly than
    /// a scope exit.
    pub fn cancel(self) {}
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.handle.unsubscribe(self.query_handle);
    }
}

/// A live diagnostics stream. Same `Drop` discipline as [`Subscription`].
pub struct DiagnosticsSubscription {
    diag_handle: DiagnosticsHandle,
    snapshots: LatestReceiver<DiagnosticsSnapshot>,
}

impl DiagnosticsSubscription {
    pub(crate) fn new(
        diag_handle: DiagnosticsHandle,
        snapshots: LatestReceiver<DiagnosticsSnapshot>,
    ) -> Self {
        Self {
            diag_handle,
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
}

impl Drop for DiagnosticsSubscription {
    fn drop(&mut self) {
        self.diag_handle.cancel();
    }
}
