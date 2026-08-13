use std::time::Duration;

use crate::{DecryptCapability, EncryptCapability, SignerError, SignerOp};

use super::{
    DecryptPayloadRequest, EncryptPayloadRequest, EncryptedPayload, PayloadFence,
    TransientPlaintext,
};

#[derive(Debug, PartialEq, Eq)]
pub enum PayloadError {
    CiphertextTooLarge { actual: usize, max: u32 },
    PlaintextTooLarge { actual: usize, max: u32 },
    Capability(SignerError),
}

impl From<SignerError> for PayloadError {
    fn from(error: SignerError) -> Self {
        Self::Capability(error)
    }
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CiphertextTooLarge { actual, max } => {
                write!(formatter, "ciphertext is {actual} bytes; limit is {max}")
            }
            Self::PlaintextTooLarge { actual, max } => {
                write!(formatter, "plaintext is {actual} bytes; limit is {max}")
            }
            Self::Capability(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capability(error) => Some(error),
            Self::CiphertextTooLarge { .. } | Self::PlaintextTooLarge { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StalePayloadResult;

impl std::fmt::Display for StalePayloadResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("encrypted payload result does not match the current materialization")
    }
}

impl std::error::Error for StalePayloadResult {}

pub struct FencedPlaintext {
    fence: PayloadFence,
    plaintext: TransientPlaintext,
}

impl FencedPlaintext {
    pub fn accept(self, current: PayloadFence) -> Result<TransientPlaintext, StalePayloadResult> {
        if self.fence == current {
            Ok(self.plaintext)
        } else {
            Err(StalePayloadResult)
        }
    }
}

pub struct FencedCiphertext {
    fence: PayloadFence,
    ciphertext: EncryptedPayload,
}

impl FencedCiphertext {
    pub fn accept(self, current: PayloadFence) -> Result<EncryptedPayload, StalePayloadResult> {
        if self.fence == current {
            Ok(self.ciphertext)
        } else {
            Err(StalePayloadResult)
        }
    }
}

pub struct DecryptOperation {
    fence: PayloadFence,
    plaintext_limit: u32,
    operation: SignerOp<TransientPlaintext>,
}

impl DecryptOperation {
    pub fn wait(self, timeout: Duration) -> Result<FencedPlaintext, PayloadError> {
        finish_plaintext(
            self.fence,
            self.plaintext_limit,
            self.operation.wait(timeout),
        )
    }

    pub async fn recv_async(self) -> Result<FencedPlaintext, PayloadError> {
        let fence = self.fence;
        let limit = self.plaintext_limit;
        finish_plaintext(fence, limit, self.operation.recv_async().await)
    }
}

pub struct EncryptOperation {
    fence: PayloadFence,
    ciphertext_limit: u32,
    operation: SignerOp<EncryptedPayload>,
}

impl EncryptOperation {
    pub fn wait(self, timeout: Duration) -> Result<FencedCiphertext, PayloadError> {
        finish_ciphertext(
            self.fence,
            self.ciphertext_limit,
            self.operation.wait(timeout),
        )
    }

    pub async fn recv_async(self) -> Result<FencedCiphertext, PayloadError> {
        let fence = self.fence;
        let limit = self.ciphertext_limit;
        finish_ciphertext(fence, limit, self.operation.recv_async().await)
    }
}

/// Runtime-free dispatcher for bounded encrypted-payload work.
pub struct EncryptedPayloadService;

impl EncryptedPayloadService {
    pub fn decrypt(
        capability: &dyn DecryptCapability,
        fence: PayloadFence,
        ciphertext: String,
    ) -> Result<DecryptOperation, PayloadError> {
        let limits = fence.limits;
        if ciphertext.len() > limits.ciphertext_bytes as usize {
            return Err(PayloadError::CiphertextTooLarge {
                actual: ciphertext.len(),
                max: limits.ciphertext_bytes,
            });
        }
        let operation = capability.decrypt(DecryptPayloadRequest::new(
            fence.policy.scheme,
            fence.policy.peer,
            ciphertext,
        ));
        Ok(DecryptOperation {
            fence,
            plaintext_limit: limits.plaintext_bytes,
            operation,
        })
    }

    pub fn encrypt(
        capability: &dyn EncryptCapability,
        fence: PayloadFence,
        plaintext: TransientPlaintext,
    ) -> Result<EncryptOperation, PayloadError> {
        let limits = fence.limits;
        if plaintext.len() > limits.plaintext_bytes as usize {
            return Err(PayloadError::PlaintextTooLarge {
                actual: plaintext.len(),
                max: limits.plaintext_bytes,
            });
        }
        let operation = capability.encrypt(EncryptPayloadRequest::new(
            fence.policy.scheme,
            fence.policy.peer,
            plaintext,
        ));
        Ok(EncryptOperation {
            fence,
            ciphertext_limit: limits.ciphertext_bytes,
            operation,
        })
    }
}

fn finish_plaintext(
    fence: PayloadFence,
    limit: u32,
    result: Result<TransientPlaintext, SignerError>,
) -> Result<FencedPlaintext, PayloadError> {
    let plaintext = result?;
    if plaintext.len() > limit as usize {
        return Err(PayloadError::PlaintextTooLarge {
            actual: plaintext.len(),
            max: limit,
        });
    }
    Ok(FencedPlaintext { fence, plaintext })
}

fn finish_ciphertext(
    fence: PayloadFence,
    limit: u32,
    result: Result<EncryptedPayload, SignerError>,
) -> Result<FencedCiphertext, PayloadError> {
    let ciphertext = result?;
    if ciphertext.len() > limit as usize {
        return Err(PayloadError::CiphertextTooLarge {
            actual: ciphertext.len(),
            max: limit,
        });
    }
    Ok(FencedCiphertext { fence, ciphertext })
}
