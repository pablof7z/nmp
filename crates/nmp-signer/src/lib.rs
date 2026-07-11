//! `nmp-signer` — `SigningCapability` + `CryptoCapability` +
//! `LocalKeySigner` (M3 plan §3.3). HARVEST target:
//! `crates/nmp-signer-iface/src/{op,signing,handle,nip44_session}.rs` in
//! the old repo (`nostr-multi-platform`) — the `SignerOp` poll-thunk shape,
//! `SignedEvent`/`UnsignedEvent` vocabulary, and `RemoteSignerHandle` trait
//! shape are re-justified there; `CryptoCapability` co-location and the
//! local-key impl are fresh framing (M0 amendment: the key lives in the
//! engine, so decrypt is co-located with signing).
//!
//! NO tokio anywhere in this crate: [`SignerOp`] is a pollable thunk driven
//! by the engine's blocking recv loop (D8), never an `async fn`.
//!
//! Step 0 scaffold only: trait/type declarations from §3.3. A3 fills in
//! `LocalKeySigner`'s `SigningCapability`/`CryptoCapability` impls and the
//! `SignerOp::Pending` poll representation.

mod capability;
mod local;
mod op;
mod remote;

pub use capability::{CryptoCapability, SigningCapability};
pub use local::LocalKeySigner;
pub use op::{SignerError, SignerOp};
pub use remote::RemoteSignerHandle;
