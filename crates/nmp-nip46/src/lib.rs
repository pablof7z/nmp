//! Optional NIP-46 signer provider.
//!
//! This crate owns bunker/invitation parsing, NIP-46 messages, relay/session
//! transport, checkpoint state, and the concrete remote signer. It implements
//! `nmp-signer`'s protocol-neutral capability traits and adds no engine,
//! publication, retry, or receipt lifecycle.

mod bunker;
mod catalog;
mod nip46;

pub use bunker::{
    parse_bunker_uri, BunkerParseError, BunkerUri, MAX_BUNKER_URI_LEN, MAX_NIP46_RELAYS,
};
pub use catalog::{known_nip46_signers, Nip46SignerApp};
pub use nip46::{
    Nip46Cancellation, Nip46ClientMetadata, Nip46ConnectionEvent, Nip46Error, Nip46Invitation,
    Nip46Origin, Nip46SessionCheckpoint, Nip46Signer,
};
