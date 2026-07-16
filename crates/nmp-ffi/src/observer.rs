//! The UniFFI foreign-trait observers (M4 plan §4). A live query is a
//! *stream*, and UniFFI has no native stream type -- the bridge is a
//! callback interface the Rust side drives (a dedicated drain thread
//! blocking-`recv`s the M3 channel, D8: blocking recv, never poll) which a
//! thin Swift layer (a later builder) adapts into `AsyncStream`.

use crate::types::{
    FfiDiagnosticsSnapshot, FfiFrame, FfiSignEventFailure, FfiSignedEvent, FfiWriteStatus,
};

/// Drains a live subscription's frames (M4 §4b) -- the ONE observer both
/// observation modes share (#485): unbounded frames carry the exact delta
/// transition, windowed frames carry the complete bounded row set plus its
/// growth fact (see [`FfiFrame`]'s doc for why delivery derives from
/// boundedness and rows never cross the wire twice). `on_frame` is called
/// once per delivered frame, in order, on a dedicated drain thread — never
/// on the engine thread itself, so a slow native consumer cannot stall
/// `EngineCore`'s own recv loop. Both delivery modes conflate under
/// backpressure: windowed frames keep the latest complete snapshot and
/// unbounded frames compose one exact transition rebased onto the last
/// delivered state, so a slow consumer sees fewer intermediate frames but
/// its next frame still reaches newest state. `on_closed` fires exactly once, when the engine
/// has torn the subscription down (cancel, or the frame channel's `Sender`
/// was dropped for any other reason) — after which no further `on_frame`
/// call will ever occur. `frame.evidence` is the query's scoped per-source
/// acquisition evidence (`docs/design/scoped-evidence-49-12-plan.md` §4) --
/// never a collapsed completeness verdict.
#[uniffi::export(callback_interface)]
pub trait RowObserver: Send + Sync {
    fn on_frame(&self, frame: FfiFrame);
    fn on_closed(&self);
}

/// Drains a publish's `Receiver<WriteStatus>` (ledger #9: enqueue is not
/// converged -- this may be called many times per publish, ending only when
/// the intent reaches every relay's terminal, or never at all for an
/// `Ephemeral` intent, mirroring `Handle::publish`'s own receiver).
/// `on_closed` fires exactly once, when the receipt's `Sender` is dropped
/// (the intent has resolved or the engine has shut down) -- after which no
/// further `on_status` call will ever occur, mirroring `RowObserver` and
/// `DiagnosticsObserver`.
#[uniffi::export(callback_interface)]
pub trait ReceiptObserver: Send + Sync {
    fn on_status(&self, status: FfiWriteStatus);
    fn on_closed(&self);
}

/// Drains a live diagnostics stream (`nmp::Engine::observe_diagnostics`'s
/// `DiagnosticsSubscription`, M5 plan §1.2 step 5). `on_snapshot` fires once
/// per delivered snapshot, in order, on a dedicated drain thread — never on
/// the engine thread itself. Because the underlying mailbox is latest-wins
/// (see `nmp::DiagnosticsSubscription::recv`'s doc), a slow Swift-side
/// consumer may observe fewer snapshots than were actually produced, but
/// never a stale one out of order. `on_closed` fires exactly once, when the
/// observer is cancelled or the engine shuts down.
#[uniffi::export(callback_interface)]
pub trait DiagnosticsObserver: Send + Sync {
    fn on_snapshot(&self, snapshot: FfiDiagnosticsSnapshot);
    fn on_closed(&self);
}

/// Exactly-once completion callback for one governed sign-only operation.
/// Success carries a fully verified event; failure is terminal. Cancellation
/// uses the same terminal callback so native async continuations cannot hang.
#[uniffi::export(callback_interface)]
pub trait SignEventObserver: Send + Sync {
    fn on_signed(&self, event: FfiSignedEvent);
    fn on_failed(&self, failure: FfiSignEventFailure);
}
