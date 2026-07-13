//! `nmp-signer` — local and NIP-46 implementations of
//! `SigningCapability` + `CryptoCapability`. The original M3 HARVEST target:
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
//! `LocalKeySigner` is the in-process implementation. `Nip46Signer` owns an
//! independent event-driven signer-relay pool and returns the same
//! `SignerOp::Pending` representation, so the engine drives both through one
//! capability boundary.

mod bunker;
mod capability;
mod catalog;
mod local;
mod nip46;
mod op;

pub use bunker::{
    parse_bunker_uri, BunkerParseError, BunkerUri, MAX_BUNKER_URI_LEN, MAX_NIP46_RELAYS,
};
pub use capability::{CryptoCapability, SigningCapability};
pub use catalog::{known_local_signers, LocalSignerApp, LocalSignerProtocol};
pub use local::LocalKeySigner;
pub use nip46::{
    Nip46Cancellation, Nip46ClientMetadata, Nip46ConnectionEvent, Nip46Error, Nip46Invitation,
    Nip46Signer,
};
pub use op::{PendingSignerOp, SignerError, SignerOp};
