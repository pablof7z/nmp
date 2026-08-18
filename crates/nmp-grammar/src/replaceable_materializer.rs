//! The capability-write contract (#1707): what a replaceable-event
//! capability (NIP-02 follow, NIP-29 saved groups, and every future one)
//! implements to turn its own typed operations into the ordinary
//! [`WritePayload`]. Moved here from `nmp` alongside [`crate::Row`] --
//! `nmp` executes a compiled capability, it does not need to know what any
//! capability means, and this trait's own signature was already
//! `nmp_grammar`/`nostr` only. The one thing that looked like it might not
//! be (`Row`) moved down in the same package first.

use std::sync::Arc;

use crate::{EventBuilder, ReplaceableOperationError, WritePayload};
use crate::{ReplaceableOperation, Row};
use nostr::nips::nip01::Coordinate;
use nostr::{Kind, UnsignedEvent};

/// One opaque operation delivered to the capability implementation that owns
/// its format.
#[derive(Clone, Copy)]
pub struct ReplaceableMaterializerOperation<'a> {
    bytes: &'a [u8],
}

impl<'a> ReplaceableMaterializerOperation<'a> {
    /// `pub`, not `pub(crate)`: `nmp` constructs these on the opposite side
    /// of the #1707 package boundary. Harmless -- apps only ever receive one
    /// as a trait-method parameter, never construct one themselves, so this
    /// widening bypasses nothing the engine protects.
    pub fn new(bytes: &'a [u8]) -> Self {
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
///
/// Both methods must be pure: deterministic in their arguments, and free of
/// side effects. NMP calls them with no store transaction open and no
/// promise of calling them once -- a compare-and-swap that loses re-prepares
/// the whole materialization from the newer snapshot, and a newly qualified
/// relay source re-applies every contributing operation onto it. An
/// implementation that counts calls, mutates shared state, or reads a clock
/// is reading NMP's retry behaviour, not its own operations.
pub trait ReplaceableMaterializer: Send + Sync + 'static {
    /// Apply every operation in `operations` to `source`, in order, exactly
    /// once each.
    ///
    /// `source` is the value the operations are composed against, never a
    /// value that already carries their effects: the retained relay source
    /// when there is one, and the newly qualified relay source when one
    /// arrives and the whole list is replayed onto it. Implementations may
    /// therefore assume each operation is new to `source` -- but must not
    /// assume the result of a previous call is ever handed back.
    fn materialize(
        &self,
        source: &UnsignedEvent,
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

/// The engine's own internal wiring from a compiled capability's identity to
/// its implementation, keyed and stored inside `EngineCore`. Never
/// re-exported from `nmp`'s own crate root -- an app reaches a capability
/// only through [`ReplaceableMaterializerSpec`]/[`RegisteredReplaceableMaterializer`],
/// never this type. Fields are `pub` (not `pub(crate)`) because `nmp`'s
/// engine internals -- the only real readers -- sit in a different crate
/// after #1707; the type staying unreachable from `nmp::` is what keeps
/// this from being an app-facing widening.
pub struct ReplaceableMaterializerRegistration {
    pub program: [u8; 16],
    pub format: [u8; 16],
    pub materializer: Arc<dyn ReplaceableMaterializer>,
}

/// Constructor for the ordinary write payload bound to one compiled
/// program/format. The engine owns its execution.
///
/// This handle names only the compiled capability identity supplied before
/// engine construction. Publishing its payload through an engine that does
/// not include that program/format is refused before custody.
#[derive(Clone, Copy)]
pub struct RegisteredReplaceableMaterializer {
    pub(crate) program: [u8; 16],
    pub(crate) format: [u8; 16],
}

impl RegisteredReplaceableMaterializer {
    pub fn operation(
        &self,
        current: &Row,
        operation: Vec<u8>,
    ) -> Result<WritePayload, ReplaceableOperationError> {
        // `Row`'s body is private even within this crate's own module
        // boundary; rebuilt from its public accessors rather than reaching
        // into it, exactly the shape `event_for_store`/`signed_event`
        // already use internally.
        let body = UnsignedEvent {
            id: Some(current.id()),
            pubkey: current.pubkey(),
            created_at: current.created_at(),
            kind: current.kind(),
            tags: current.tags().clone(),
            content: current.content().to_string(),
        };
        body.verify_id()
            .map_err(|_| ReplaceableOperationError::CurrentInvalid)?;
        ReplaceableOperation::from_registered_parts(
            self.program,
            self.format,
            body.clone(),
            body,
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
        operation: Vec<u8>,
    ) -> Result<WritePayload, ReplaceableOperationError> {
        ReplaceableOperation::from_registered_default_parts(
            self.program,
            self.format,
            kind,
            identifier,
            operation,
        )
        .map(WritePayload::ReplaceableOperation)
    }
}

/// One compiled capability implementation supplied before engine recovery.
pub struct ReplaceableMaterializerSpec {
    program: [u8; 16],
    format: [u8; 16],
    materializer: Arc<dyn ReplaceableMaterializer>,
}

impl ReplaceableMaterializerSpec {
    #[must_use]
    pub fn new<M>(program: [u8; 16], format: [u8; 16], materializer: M) -> Self
    where
        M: ReplaceableMaterializer,
    {
        Self {
            program,
            format,
            materializer: Arc::new(materializer),
        }
    }

    /// This spec's compiled program identity. `nmp`'s engine construction
    /// reads this (and `format`) to refuse a duplicate program/format pair
    /// before any engine thread starts -- an engine-internal read from a
    /// different crate after #1707, not an app-facing need, which is why
    /// this is a narrow accessor rather than a widened field.
    #[must_use]
    pub fn program(&self) -> [u8; 16] {
        self.program
    }

    /// This spec's compiled format identity. See [`Self::program`].
    #[must_use]
    pub fn format(&self) -> [u8; 16] {
        self.format
    }

    #[must_use]
    pub fn handle(&self) -> RegisteredReplaceableMaterializer {
        RegisteredReplaceableMaterializer {
            program: self.program,
            format: self.format,
        }
    }

    /// `pub`, not `pub(crate)`: `nmp`'s own engine construction is the only
    /// caller, and it sits in a different crate after #1707. Not
    /// re-exported from `nmp`'s own crate root, so an app never sees this
    /// door -- see [`ReplaceableMaterializerRegistration`]'s own doc.
    pub fn into_registration(self) -> ReplaceableMaterializerRegistration {
        ReplaceableMaterializerRegistration {
            program: self.program,
            format: self.format,
            materializer: self.materializer,
        }
    }
}
