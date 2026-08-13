//! Local-key implementation of NMP's protocol-neutral signer interface.
//!
//! This crate owns the in-process secret and operation-scoped cryptography.
//! The dependency-light `nmp-signer` crate owns only values, capability
//! traits, errors, and the cancellable completion door.

mod local;
mod local_crypto;

pub use local::{LocalKeySigner, LocalKeySignerError, LOCAL_KEY_PROVIDER_ID};
