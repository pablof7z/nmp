use std::sync::Arc;

use nmp_grammar::{EventBuilder, ReplaceableOperationError, ReplaceableSourcePolicy, WritePayload};
use nostr::nips::nip01::Coordinate;
use nostr::{Kind, UnsignedEvent};

use crate::Row;

/// One opaque operation delivered to the capability implementation that owns
/// its format.
#[derive(Clone, Copy)]
pub struct ReplaceableMaterializerOperation<'a> {
    bytes: &'a [u8],
}

impl<'a> ReplaceableMaterializerOperation<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

/// Typed pre-custody refusal from a configured synchronous capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceableMaterializerRefusal {
    pub reason: String,
}

/// Capability-owned synchronous interpretation of opaque operations.
pub trait ReplaceableMaterializer: Send + Sync + 'static {
    fn materialize(
        &self,
        source: &UnsignedEvent,
        current: &UnsignedEvent,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal>;

    /// Construct the capability-defined first value and apply the operations
    /// when NMP has no qualified source or retained generation yet.
    ///
    /// The coordinate's public key is selected from the write intent identity;
    /// implementations own only kind-specific tags/content. NMP validates the
    /// returned coordinate and derives timestamp, id, and signature.
    fn materialize_default(
        &self,
        coordinate: &Coordinate,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal>;
}

pub(crate) struct ReplaceableMaterializerRegistration {
    pub(crate) instance: [u8; 16],
    pub(crate) program: [u8; 16],
    pub(crate) format: [u8; 16],
    pub(crate) materializer: Arc<dyn ReplaceableMaterializer>,
}

/// Registration-bound constructor for the ordinary write payload. The caller
/// chooses the closed source lifetime policy; the engine owns its execution.
///
/// This handle carries the exact engine installation identity. Publishing its
/// payload through another engine, or after replacement, is refused before
/// custody.
#[derive(Clone)]
pub struct RegisteredReplaceableMaterializer {
    pub(crate) instance: [u8; 16],
}

impl RegisteredReplaceableMaterializer {
    pub fn operation(
        &self,
        current: &Row,
        source_policy: ReplaceableSourcePolicy,
        operation: Vec<u8>,
    ) -> Result<WritePayload, ReplaceableOperationError> {
        current
            .body
            .verify_id()
            .map_err(|_| ReplaceableOperationError::CurrentInvalid)?;
        nmp_grammar::ReplaceableOperation::from_registered_parts(
            self.instance,
            current.body.clone(),
            current.body.clone(),
            source_policy,
            operation,
        )
        .map(WritePayload::ReplaceableOperation)
    }

    /// Construct an operation for a coordinate that may not have a value yet.
    ///
    /// The configured capability defines the empty body. This handle names
    /// only kind and parameterized identifier; the write intent identity is
    /// the sole source of the author.
    pub fn first_value_operation(
        &self,
        kind: Kind,
        identifier: String,
        source_policy: ReplaceableSourcePolicy,
        operation: Vec<u8>,
    ) -> Result<WritePayload, ReplaceableOperationError> {
        nmp_grammar::ReplaceableOperation::from_registered_default_parts(
            self.instance,
            kind,
            identifier,
            source_policy,
            operation,
        )
        .map(WritePayload::ReplaceableOperation)
    }
}
