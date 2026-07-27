//! Protocol-neutral native signer-provider attachment boundary.
//!
//! Provider components receive one opaque mailbox from [`NmpEngine`]. The
//! mailbox keeps the engine, registration, validation, and runtime lifecycle
//! in core while allowing a separately linked provider to contribute an
//! ordinary [`nmp::SigningCapability`]. No provider kind crosses this module.

use std::sync::Arc;

use crate::convert::FfiError;

/// Exact build identity for the Rust object contract shared with native
/// provider components.
///
/// The build script hashes the governed core Rust source, Cargo resolution,
/// compiler, target, and profile. Provider components compiled separately
/// compare their embedded copy with the identity exported by the loaded core
/// before any external Rust object pointer crosses the native-library seam.
pub const CORE_COMPONENT_IDENTITY: &str = env!("NMP_CORE_COMPONENT_IDENTITY");

/// Return the loaded core library's exact native component identity.
///
/// This value is plain data by design: a provider can compare it before
/// requesting or lowering [`FfiSignerMailbox`].
#[uniffi::export]
pub fn nmp_core_component_identity() -> String {
    CORE_COMPONENT_IDENTITY.to_string()
}

/// Opaque proof for one exact provider capability installation.
///
/// The value is intentionally provider-neutral. A provider retains it only
/// long enough to detach the exact installation it created; it cannot select
/// another signer or remove a replacement registered for the same key.
#[derive(uniffi::Object)]
pub struct FfiSignerRegistration {
    inner: nmp::SignerRegistration,
}

impl std::fmt::Debug for FfiSignerRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiSignerRegistration")
            .field("public_key", &self.inner.public_key())
            .finish_non_exhaustive()
    }
}

/// Opaque provider mailbox bound to one exact core engine.
///
/// Native provider components consume this external UniFFI object rather than
/// importing or mirroring `NmpEngine`. Its Rust-only operations are the
/// ordinary generic signer door and the engine-owned adapter runtime already
/// used by supported provider sessions.
#[derive(uniffi::Object)]
pub struct FfiSignerMailbox {
    engine: Arc<nmp::Engine>,
}

impl FfiSignerMailbox {
    #[doc(hidden)]
    #[must_use]
    pub fn from_engine(engine: Arc<nmp::Engine>) -> Arc<Self> {
        Arc::new(Self { engine })
    }

    /// Attach one concrete provider through core's protocol-neutral signer
    /// capability door.
    #[doc(hidden)]
    pub fn attach<S>(&self, signer: S) -> Result<Arc<FfiSignerRegistration>, FfiError>
    where
        S: nmp::SigningCapability + Send + 'static,
    {
        let registration = self.engine.add_signer(signer)?;
        Ok(Arc::new(FfiSignerRegistration {
            inner: registration,
        }))
    }

    /// Detach only the exact installation proven by `registration`.
    #[doc(hidden)]
    pub fn detach(&self, registration: Arc<FfiSignerRegistration>) -> Result<bool, FfiError> {
        Ok(self.engine.remove_signer(registration.inner.clone())?)
    }

    /// Borrow the one engine-owned runtime for provider session work.
    #[doc(hidden)]
    pub fn adapter_runtime(&self) -> Result<tokio::runtime::Handle, FfiError> {
        Ok(self.engine.adapter_runtime()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_component_identity_is_versioned_and_exact_length() {
        let identity = nmp_core_component_identity();
        let digest = identity
            .strip_prefix("nmp-core-component-v1-")
            .expect("identity carries its schema version");
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
