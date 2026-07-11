//! The UniFFI foreign-trait observers (M4 plan §4). A live query is a
//! *stream*, and UniFFI has no native stream type -- the bridge is a
//! callback interface the Rust side drives (a dedicated drain thread
//! blocking-`recv`s the M3 channel, D8: blocking recv, never poll) which a
//! thin Swift layer (a later builder) adapts into `AsyncStream`.

use crate::types::{FfiCoverage, FfiRowDelta, FfiWriteStatus};

/// Drains a live subscription's `Receiver<RowsMsg>` (M4 §4b). `on_batch` is
/// called once per delivered batch, in order, on a dedicated drain thread —
/// never on the engine thread itself, so a slow Swift-side consumer cannot
/// stall `EngineCore`'s own recv loop. `on_closed` fires exactly once, when
/// the engine has torn the subscription down (cancel, or the row channel's
/// `Sender` was dropped for any other reason) — after which no further
/// `on_batch` call will ever occur.
#[uniffi::export(callback_interface)]
pub trait RowObserver: Send + Sync {
    fn on_batch(&self, deltas: Vec<FfiRowDelta>, coverage: FfiCoverage);
    fn on_closed(&self);
}

/// Drains a publish's `Receiver<WriteStatus>` (ledger #9: enqueue is not
/// converged -- this may be called many times per publish, ending only when
/// the intent reaches every relay's terminal, or never at all for an
/// `Ephemeral` intent, mirroring `Handle::publish`'s own receiver).
#[uniffi::export(callback_interface)]
pub trait ReceiptObserver: Send + Sync {
    fn on_status(&self, status: FfiWriteStatus);
}
