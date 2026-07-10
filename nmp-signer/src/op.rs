//! The pollable thunk (§3.3, HARVEST `nmp-signer-iface::op`).

/// An op that may complete synchronously (`Ready`) or later (`Pending`).
/// `Pending` is polled on the engine's blocking recv loop — no tokio is
/// ever pulled into the engine (D8).
///
/// Step 0 leaves `Pending` fieldless: A3 decides the concrete poll-handle
/// representation (harvested from `nmp-signer-iface::op`) once
/// `LocalKeySigner` and the `RemoteSignerHandle` seam need one.
pub enum SignerOp<T> {
    Ready(Result<T, SignerError>),
    Pending,
}

/// Failure vocabulary for a `SignerOp` (§3.3). A3 may extend this closed
/// set as `LocalKeySigner`'s impls demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerError {
    Rejected(String),
    Unavailable,
    Timeout,
}
