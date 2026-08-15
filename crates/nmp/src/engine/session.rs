use nmp_signer::SigningCapability;
use nostr::PublicKey;

use super::Engine;
use crate::config::EngineConfig;
use crate::error::EngineError;

/// Map an `Engine::new_with_initial_session` failure to a session-restore
/// error, preserving the typed missing-capability refusal so FFI reopen
/// surfaces it as `MissingReplaceableCapability` rather than a generic
/// start-failed string (#1624).
fn map_session_start_error(error: EngineError) -> crate::SessionRestoreError {
    match error {
        EngineError::MissingReplaceableCapability { program, format } => {
            crate::SessionRestoreError::MissingReplaceableCapability { program, format }
        }
        other => crate::SessionRestoreError::EngineStartFailed {
            reason: other.to_string(),
        },
    }
}

fn session_mutation_from_add_signer(
    error: crate::runtime::AddSignerError,
) -> crate::SessionMutationError {
    match error {
        crate::runtime::AddSignerError::RegistryFull { limit } => {
            crate::SessionMutationError::CapabilityRegistryFull { limit }
        }
        crate::runtime::AddSignerError::CapabilityInstanceExhausted => {
            crate::SessionMutationError::CapabilityInstanceExhausted
        }
        crate::runtime::AddSignerError::EngineShuttingDown
        | crate::runtime::AddSignerError::MissingPublicKey => {
            crate::SessionMutationError::EngineClosed
        }
    }
}

impl Engine {
    #[cfg(test)]
    pub(super) fn install_test_local_provider(
        &self,
        secret_key: &str,
    ) -> Result<crate::SessionAccount, crate::SessionMutationError> {
        let signer = nmp_local_signer::LocalKeySigner::parse(secret_key)
            .map_err(|_| crate::SessionMutationError::InvalidSecretKey)?;
        self.with_handle(|handle| handle.add_private_key_account(signer, false))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .map_err(session_mutation_from_add_signer)
    }

    #[cfg(test)]
    pub(super) fn select_test_account(
        &self,
        public_key: Option<PublicKey>,
    ) -> Result<(), EngineError> {
        match public_key {
            Some(public_key) => {
                self.add_public_key_account(public_key, false)
                    .map_err(|_| EngineError::EngineClosed)?;
                self.make_current_account(public_key)
                    .map_err(|_| EngineError::EngineClosed)
            }
            None => {
                self.with_handle(|handle| handle.set_current_account(None))?;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub(super) fn test_current_public_key(&self) -> Result<Option<PublicKey>, EngineError> {
        Ok(self.session()?.current_pubkey)
    }

    pub fn new_with_session(
        config: EngineConfig,
        payload: crate::SessionPayload,
    ) -> Result<Self, crate::SessionRestoreError> {
        let restored = crate::session::decode(&payload)?;
        let provider_count = restored.provider_count();
        if provider_count > config.max_auth_capabilities {
            return Err(crate::SessionRestoreError::CapabilityRegistryFull {
                limit: config.max_auth_capabilities,
            });
        }
        Self::new_with_initial_session(config, restored, super::default_capabilities())
            .map_err(map_session_start_error)
    }

    /// Restore a session into an engine whose compiled replaceable
    /// capabilities are already assembled.
    pub fn new_with_session_and_capabilities(
        config: EngineConfig,
        payload: crate::SessionPayload,
        capabilities: Vec<crate::ReplaceableMaterializerSpec>,
    ) -> Result<Self, crate::SessionRestoreError> {
        let restored = crate::session::decode(&payload)?;
        let provider_count = restored.provider_count();
        if provider_count > config.max_auth_capabilities {
            return Err(crate::SessionRestoreError::CapabilityRegistryFull {
                limit: config.max_auth_capabilities,
            });
        }
        Self::new_with_initial_session(config, restored, capabilities)
            .map_err(map_session_start_error)
    }

    pub fn session(&self) -> Result<crate::SessionSnapshot, EngineError> {
        self.with_handle(|handle| handle.session_snapshot())?
            .ok_or(EngineError::EngineClosed)
    }

    pub fn export_session(&self) -> Result<crate::SessionPayload, EngineError> {
        // The reducer returns only cloned provider owners and metadata. Secret
        // descriptor callbacks then run here, after the reducer command and
        // after the facade lifecycle lock have both been released.
        let handle = self.with_handle(Clone::clone)?;
        let export = handle
            .session_export_sources()
            .ok_or(EngineError::EngineClosed)?;
        let descriptors = export
            .providers
            .into_iter()
            .filter_map(|(public_key, provider)| {
                provider
                    .persistence_descriptor()
                    .map(|descriptor| (public_key, descriptor))
            })
            .collect();
        Ok(crate::session::encode(&export.snapshot, descriptors))
    }

    pub fn add_private_key_account(
        &self,
        secret_key: &[u8; 32],
        make_current: bool,
    ) -> Result<crate::SessionAccount, crate::SessionMutationError> {
        let signer = nmp_local_signer::LocalKeySigner::from_secret_bytes(secret_key)
            .map_err(|_| crate::SessionMutationError::InvalidSecretKey)?;
        let result = self
            .with_handle(|handle| handle.add_private_key_account(signer, make_current))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?;
        result.map_err(session_mutation_from_add_signer)
    }

    pub fn add_public_key_account(
        &self,
        public_key: PublicKey,
        make_current: bool,
    ) -> Result<crate::SessionAccount, crate::SessionMutationError> {
        self.with_handle(|handle| handle.add_public_key_account(public_key, make_current))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .ok_or(crate::SessionMutationError::EngineClosed)
    }

    pub fn make_current_account(
        &self,
        public_key: PublicKey,
    ) -> Result<(), crate::SessionMutationError> {
        let found = self
            .with_handle(|handle| handle.make_current_account(public_key))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .ok_or(crate::SessionMutationError::EngineClosed)?;
        found
            .then_some(())
            .ok_or(crate::SessionMutationError::AccountNotFound { public_key })
    }

    pub fn remove_account(
        &self,
        account: &crate::SessionAccount,
    ) -> Result<bool, crate::SessionMutationError> {
        self.with_handle(|handle| handle.remove_session_account(account.public_key))
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .ok_or(crate::SessionMutationError::EngineClosed)
    }

    pub fn clear_session(&self) -> Result<(), crate::SessionMutationError> {
        self.with_handle(|handle| handle.clear_session())
            .map_err(|_| crate::SessionMutationError::EngineClosed)?
            .ok_or(crate::SessionMutationError::EngineClosed)
    }
}
