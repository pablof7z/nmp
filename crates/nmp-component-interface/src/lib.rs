//! Canonical Rust/UniFFI contract shared by independently built native pieces.
//!
//! This crate is internal mechanism, not an app-facing component registry or a
//! third workload noun. It owns only the opaque signer object seam that may
//! cross from the one core artifact into a separately selected signer
//! component.

mod signer;

pub use signer::{
    new_signer_adapter, ComponentInterfaceError, FfiSignerAdapter, ProviderAdapterTask,
    SignerAdapterCancellation, SignerAdapterCommand, SignerAdapterControl, SignerAdapterRuntime,
    SignerAdapterTakeError, StartedSignerAdapter, TakenSignerAdapter,
};

/// Complete build/ABI identity of this crossing contract.
pub const COMPONENT_INTERFACE_IDENTITY: &str = env!("NMP_COMPONENT_INTERFACE_IDENTITY");

/// Plain data for checking a packaged binding against the loaded core before
/// requesting any external object.
#[uniffi::export]
pub fn nmp_component_interface_identity() -> String {
    COMPONENT_INTERFACE_IDENTITY.to_owned()
}

uniffi::setup_scaffolding!();
