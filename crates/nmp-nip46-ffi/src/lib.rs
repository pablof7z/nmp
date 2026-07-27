//! Optional native NIP-46 provider component.
//!
//! The core `nmp-ffi` component owns the engine and generic signer capability
//! lifecycle. This component owns every NIP-46 record, error, observer,
//! invitation, connection, and checkpoint symbol.

pub mod signer;

uniffi::setup_scaffolding!();
