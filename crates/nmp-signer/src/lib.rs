//! `nmp-signer` — the dependency-light protocol-neutral signer/crypto
//! capability contract and its one cancellable completion model.
//! The original M3 HARVEST target:
//! `crates/nmp-signer-iface/src/{op,signing,handle,nip44_session}.rs` in
//! the old repo (`nostr-multi-platform`) — the `SignerOp` poll-thunk shape is
//! re-justified there; the exact immutable request/result values and
//! `CryptoCapability` co-location are fresh framing.
//!
//! NO tokio anywhere in this crate: [`SignerOp`] is a pollable thunk driven
//! by the engine's blocking recv loop (D8), never an `async fn`.
//!
//! Local and remote implementations live in separate provider crates. They
//! implement these traits without adding a second engine, signing,
//! cancellation, or publication lifecycle.

mod capability;
mod op;
mod value;

pub use capability::{CryptoCapability, SigningCapability};
pub use op::{
    Canceller, PendingSignerOp, PendingSignerResolveError, PendingSignerSender, SignerError,
    SignerOp,
};
pub use value::{SignerPublicKey, SignerSignedEvent, SignerSignedEventParts, SignerUnsignedEvent};
