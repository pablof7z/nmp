//! Durable store-owned transitions for semantic edits of replaceable state.
//!
//! This layer deliberately cannot interpret an edit. Capability modules own
//! the closed program grammar; the store persists each still-contributing
//! program as versioned opaque bytes, in acceptance order. A receipt survives
//! after its heavier program body is resolved. An active coordinate has zero
//! or one current materialization shared by every contributing receipt.

use std::collections::{BTreeMap, BTreeSet};

use nostr::nips::nip01::Coordinate;
use nostr::{Event, EventId, JsonUtil, Timestamp, UnsignedEvent};

use crate::{MemoryStore, PersistenceError, RedbStore, VerifiedSignature};

pub(crate) const MAX_PROGRAM_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_MATERIALIZATION_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_COORDINATE_IDENTIFIER_BYTES: usize = 65_536;
pub(crate) const MAX_CONTRIBUTING_OPERATIONS: usize = 1_000_000;
pub(crate) const MAX_RESOLUTION_REASON_BYTES: usize = 4_096;

/// Store-allocated identity of one accepted semantic operation and its
/// independent receipt. These are deliberately the same identity: there is
/// one retention-owned receipt per accepted operation, never a second
/// caller-allocated receipt namespace that can disagree with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaterializationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticPlan {
    version: u16,
    bytes: Vec<u8>,
}

impl SemanticPlan {
    pub fn new(version: u16, bytes: Vec<u8>) -> Result<Self, SemanticStoreError> {
        if version == 0 {
            return Err(SemanticStoreError::InvalidPlanVersion);
        }
        if bytes.len() > MAX_PROGRAM_BYTES {
            return Err(SemanticStoreError::PlanTooLarge);
        }
        Ok(Self { version, bytes })
    }

    #[must_use]
    pub fn version(&self) -> u16 {
        self.version
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationResolution {
    Contributing,
    Resolved,
    Cancelled,
    Refused(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOperation {
    pub operation_id: OperationId,
    pub resolution: OperationResolution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticEditReceipt {
    pub operation_id: OperationId,
    pub coordinate: Coordinate,
    pub accepted_at: Timestamp,
    pub resolution: OperationResolution,
    pub current_materialization: Option<MaterializationId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticOperation {
    pub operation_id: OperationId,
    pub accepted_at: Timestamp,
    pub plan: SemanticPlan,
}

/// Opaque caller-provided source identity plus a store-owned CAS ordinal.
///
/// The store never claims to verify relay provenance, source qualification,
/// or replacement-winner status; those are upstream responsibilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRevision {
    ordinal: u64,
    event_id: Option<EventId>,
    created_at: Option<Timestamp>,
}

impl SourceRevision {
    fn initial(source: Option<(EventId, Timestamp)>) -> Self {
        match source {
            None => Self {
                ordinal: 0,
                event_id: None,
                created_at: None,
            },
            Some((event_id, created_at)) => Self {
                ordinal: 1,
                event_id: Some(event_id),
                created_at: Some(created_at),
            },
        }
    }

    fn reconcile(
        previous: &Self,
        source: Option<(EventId, Timestamp)>,
    ) -> Result<Self, SemanticStoreError> {
        if previous.identity() == source {
            return Ok(previous.clone());
        }
        Ok(Self {
            ordinal: previous
                .ordinal
                .checked_add(1)
                .ok_or(SemanticStoreError::SourceRevisionExhausted)?,
            event_id: source.map(|(event_id, _)| event_id),
            created_at: source.map(|(_, created_at)| created_at),
        })
    }

    pub(crate) fn from_parts(
        ordinal: u64,
        event_id: Option<EventId>,
        created_at: Option<Timestamp>,
    ) -> Result<Self, SemanticStoreError> {
        if event_id.is_some() != created_at.is_some() || (ordinal == 0) != event_id.is_none() {
            return Err(SemanticStoreError::SourceRevisionChanged);
        }
        Ok(Self {
            ordinal,
            event_id,
            created_at,
        })
    }

    #[must_use]
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }

    #[must_use]
    pub fn identity(&self) -> Option<(EventId, Timestamp)> {
        self.event_id.zip(self.created_at)
    }

    #[must_use]
    pub fn event_id(&self) -> Option<EventId> {
        self.event_id
    }

    #[must_use]
    pub fn created_at(&self) -> Option<Timestamp> {
        self.created_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentMaterialization {
    Pending(UnsignedEvent),
    Signed(Event),
}

impl CurrentMaterialization {
    #[must_use]
    pub fn unsigned(&self) -> UnsignedEvent {
        match self {
            Self::Pending(unsigned) => unsigned.clone(),
            Self::Signed(event) => UnsignedEvent {
                id: Some(event.id),
                pubkey: event.pubkey,
                created_at: event.created_at,
                kind: event.kind,
                tags: event.tags.clone(),
                content: event.content.clone(),
            },
        }
    }

    #[must_use]
    pub fn event_id(&self) -> EventId {
        match self {
            Self::Pending(unsigned) => EventId::new(
                &unsigned.pubkey,
                &unsigned.created_at,
                &unsigned.kind,
                &unsigned.tags,
                &unsigned.content,
            ),
            Self::Signed(event) => event.id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticGeneration {
    pub materialization_id: MaterializationId,
    pub members: BTreeSet<OperationId>,
    pub event: CurrentMaterialization,
}

/// The two store-owned fences describing the coordinate's current projection.
/// A resource can be active with `generation == None` while its retained
/// operations are still content-pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticCurrentState {
    pub source_revision: SourceRevision,
    pub generation: Option<SemanticGeneration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredSemanticResource {
    pub coordinate: Coordinate,
    pub operations: Vec<SemanticOperation>,
    pub current: SemanticCurrentState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticAccept {
    pub coordinate: Coordinate,
    pub expected_source_revision: Option<SourceRevision>,
    pub expected_current_materialization: Option<MaterializationId>,
    /// Exact source event selected by the caller. The store owns and allocates
    /// the corresponding revision ordinal; callers can only fence using the
    /// `SourceRevision` returned by an earlier committed transition.
    pub source: Option<(EventId, Timestamp)>,
    pub accepted_at: Timestamp,
    pub plan: SemanticPlan,
    /// `None` accepts a content-pending operation without inventing event
    /// bytes, an event id, a signer request, or a delivery lane.
    pub materialized: Option<UnsignedEvent>,
    /// Still-contributing PRIOR operations in acceptance order. The newly
    /// accepted operation is always appended and contributes to this
    /// generation; an immediately-resolved/no-op acceptance is not admitted
    /// by this door.
    pub contributing_operations: Vec<OperationId>,
    pub resolved_operations: Vec<ResolvedOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticRematerialize {
    pub coordinate: Coordinate,
    pub expected_source_revision: SourceRevision,
    pub expected_current_materialization: Option<MaterializationId>,
    /// Exact newly selected source, or the unchanged current source. A source
    /// identity change advances the store-owned revision by exactly one.
    pub source: Option<(EventId, Timestamp)>,
    /// `Some` exactly when `contributing_operations` is nonempty. Resolving
    /// every operation removes the active resource/current body and supplies
    /// `None`; a body with no receipt members is unrepresentable.
    pub materialized: Option<UnsignedEvent>,
    pub contributing_operations: Vec<OperationId>,
    pub resolved_operations: Vec<ResolvedOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticPromotion {
    pub coordinate: Coordinate,
    pub expected_source_revision: SourceRevision,
    pub expected_materialization: MaterializationId,
    pub verified: VerifiedSignature,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticPromotionOutcome {
    Promoted(SemanticGeneration),
    Stale,
}

#[derive(Debug)]
pub enum SemanticStoreError {
    Persistence(PersistenceError),
    InvalidCoordinate,
    InvalidPlanVersion,
    PlanTooLarge,
    MaterializationDoesNotMatchCoordinate,
    DuplicateOperation(OperationId),
    UnknownOperation(OperationId),
    NonCanonicalOperationOrder,
    SourceRevisionChanged,
    CurrentMaterializationChanged,
    MaterializationMembershipMismatch,
    MaterializationTimestampDidNotAdvance,
    MaterializationAlreadySigned,
    MaterializationTooLarge,
    MaterializationIdExhausted,
    OperationIdExhausted,
    TooManyContributingOperations,
    ResolutionReasonTooLarge,
    SourceRevisionExhausted,
}

impl From<PersistenceError> for SemanticStoreError {
    fn from(error: PersistenceError) -> Self {
        Self::Persistence(error)
    }
}

impl std::fmt::Display for SemanticStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SemanticStoreError {}

#[cfg(any(test, feature = "bench-instrumentation"))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SemanticStoreCounters {
    pub coordinate_point_reads: u64,
    pub operation_bodies_examined: u64,
    pub operation_bodies_written: u64,
    pub operation_bodies_removed: u64,
    pub materializations_written: u64,
    pub materializations_removed: u64,
    pub commits: u64,
    pub recovery_rows: u64,
}

pub trait SemanticEditStore {
    fn accept_semantic_edit(
        &mut self,
        accept: SemanticAccept,
    ) -> Result<(SemanticEditReceipt, SemanticCurrentState), SemanticStoreError>;

    fn rematerialize_semantic_edit(
        &mut self,
        rematerialize: SemanticRematerialize,
    ) -> Result<Option<SemanticCurrentState>, SemanticStoreError>;

    fn promote_semantic_materialization(
        &mut self,
        promotion: SemanticPromotion,
    ) -> Result<SemanticPromotionOutcome, SemanticStoreError>;

    fn recover_semantic_resources(
        &self,
    ) -> Result<Vec<RecoveredSemanticResource>, SemanticStoreError>;

    fn semantic_receipt(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<SemanticEditReceipt>, SemanticStoreError>;

    #[cfg(any(test, feature = "bench-instrumentation"))]
    fn semantic_store_counters(&self) -> SemanticStoreCounters;
    #[cfg(any(test, feature = "bench-instrumentation"))]
    fn reset_semantic_store_counters(&self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticResourceState {
    pub(crate) coordinate: Coordinate,
    pub(crate) source_revision: SourceRevision,
    pub(crate) operations: Vec<SemanticOperation>,
    pub(crate) last_materialization_id: Option<MaterializationId>,
    pub(crate) generation: Option<SemanticGenerationRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticGenerationRecord {
    pub(crate) materialization_id: MaterializationId,
    pub(crate) members: BTreeSet<OperationId>,
    pub(crate) body: SemanticMaterializationRecord,
}

impl SemanticResourceState {
    pub(crate) fn current(&self) -> Result<SemanticCurrentState, SemanticStoreError> {
        Ok(SemanticCurrentState {
            source_revision: self.source_revision.clone(),
            generation: self
                .generation
                .as_ref()
                .map(SemanticGenerationRecord::materialize)
                .transpose()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SemanticMaterializationRecord {
    Pending(String),
    Signed(String),
}

impl SemanticGenerationRecord {
    pub(crate) fn from_unsigned(
        materialization_id: MaterializationId,
        members: BTreeSet<OperationId>,
        mut event: UnsignedEvent,
    ) -> Result<Self, SemanticStoreError> {
        event.ensure_id();
        let unsigned_json = event.try_as_json().map_err(|error| {
            PersistenceError::invariant(format!("encode semantic materialization: {error}"))
        })?;
        Ok(Self {
            materialization_id,
            members,
            body: SemanticMaterializationRecord::Pending(unsigned_json),
        })
    }

    pub(crate) fn materialize(&self) -> Result<SemanticGeneration, SemanticStoreError> {
        let event = match &self.body {
            SemanticMaterializationRecord::Pending(json) => {
                let unsigned = UnsignedEvent::from_json(json).map_err(|error| {
                    PersistenceError::invariant(format!("decode semantic materialization: {error}"))
                })?;
                unsigned.verify_id().map_err(|error| {
                    PersistenceError::invariant(format!(
                        "invalid semantic materialization id: {error}"
                    ))
                })?;
                CurrentMaterialization::Pending(unsigned)
            }
            SemanticMaterializationRecord::Signed(json) => {
                let signed = Event::from_json(json).map_err(|error| {
                    PersistenceError::invariant(format!(
                        "decode signed semantic materialization: {error}"
                    ))
                })?;
                signed.verify().map_err(|error| {
                    PersistenceError::invariant(format!(
                        "invalid signed semantic materialization: {error}"
                    ))
                })?;
                CurrentMaterialization::Signed(signed)
            }
        };
        Ok(SemanticGeneration {
            materialization_id: self.materialization_id,
            members: self.members.clone(),
            event,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticTransitionPlan {
    pub(crate) previous: Option<SemanticResourceState>,
    pub(crate) next: Option<SemanticResourceState>,
    pub(crate) receipt_updates: Vec<SemanticEditReceipt>,
    pub(crate) materialization_high_water: Option<MaterializationId>,
    pub(crate) operation_bodies_examined: u64,
    pub(crate) operation_bodies_written: u64,
    pub(crate) operation_bodies_removed: u64,
}

pub(crate) fn plan_accept(
    previous: Option<SemanticResourceState>,
    receipts: &[SemanticEditReceipt],
    materialization_high_water: Option<MaterializationId>,
    operation_id: OperationId,
    accept: SemanticAccept,
) -> Result<SemanticTransitionPlan, SemanticStoreError> {
    validate_coordinate(&accept.coordinate)?;
    if let Some(previous) = &previous {
        validate_resource_state(previous)?;
        validate_active_receipts(previous, receipts)?;
    } else if !receipts.is_empty() {
        return Err(PersistenceError::invariant(
            "semantic receipts supplied without an active resource",
        )
        .into());
    }
    let (source_revision, current_materialization, mut operations) = match previous.as_ref() {
        None => (SourceRevision::initial(accept.source), None, Vec::new()),
        Some(state) => (
            SourceRevision::reconcile(&state.source_revision, accept.source)?,
            state
                .generation
                .as_ref()
                .map(|generation| generation.materialization_id),
            state.operations.clone(),
        ),
    };
    if accept.expected_source_revision.as_ref()
        != previous.as_ref().map(|state| &state.source_revision)
    {
        return Err(SemanticStoreError::SourceRevisionChanged);
    }
    if accept.expected_current_materialization != current_materialization {
        return Err(SemanticStoreError::CurrentMaterializationChanged);
    }
    let resolved = resolved_operations(accept.resolved_operations)?;
    validate_resolution_partition(
        &operations,
        &accept.contributing_operations,
        &resolved.keys().copied().collect(),
    )?;
    let mut members = accept.contributing_operations;
    members.push(operation_id);
    if members.len() > MAX_CONTRIBUTING_OPERATIONS {
        return Err(SemanticStoreError::TooManyContributingOperations);
    }
    let members = canonical_members(&operations, members, operation_id)?;
    operations.retain(|operation| !resolved.contains_key(&operation.operation_id));
    operations.push(SemanticOperation {
        operation_id,
        accepted_at: accept.accepted_at,
        plan: accept.plan,
    });
    let generation = accept
        .materialized
        .map(|materialized| {
            validate_materialized(&accept.coordinate, &materialized)?;
            validate_successor_time(
                &materialized,
                Some(&source_revision),
                previous
                    .as_ref()
                    .and_then(|state| state.generation.as_ref()),
                operations.iter(),
            )?;
            SemanticGenerationRecord::from_unsigned(
                next_materialization_id(materialization_high_water)?,
                members.clone(),
                materialized,
            )
        })
        .transpose()?;
    let current_materialization = generation
        .as_ref()
        .map(|generation| generation.materialization_id);
    let mut receipt_updates = receipts
        .iter()
        .map(|receipt| {
            let mut receipt = receipt.clone();
            if let Some(resolution) = resolved.get(&receipt.operation_id) {
                receipt.resolution = resolution.clone();
                receipt.current_materialization = None;
            } else if members.contains(&receipt.operation_id) {
                receipt.current_materialization = current_materialization;
            }
            receipt
        })
        .collect::<Vec<_>>();
    receipt_updates.push(SemanticEditReceipt {
        operation_id,
        coordinate: accept.coordinate.clone(),
        accepted_at: accept.accepted_at,
        resolution: OperationResolution::Contributing,
        current_materialization,
    });
    let operation_bodies_examined = previous
        .as_ref()
        .map(|state| state.operations.len() as u64)
        .unwrap_or(0);
    let last_materialization_id = generation
        .as_ref()
        .map(|generation| generation.materialization_id)
        .or_else(|| {
            previous
                .as_ref()
                .and_then(|state| state.last_materialization_id)
        });
    if previous
        .as_ref()
        .and_then(|state| state.last_materialization_id)
        .is_some_and(|active| Some(active) != materialization_high_water)
    {
        return Err(SemanticStoreError::CurrentMaterializationChanged);
    }
    Ok(SemanticTransitionPlan {
        previous,
        next: Some(SemanticResourceState {
            coordinate: accept.coordinate,
            source_revision,
            operations,
            last_materialization_id,
            generation,
        }),
        receipt_updates,
        materialization_high_water: last_materialization_id,
        operation_bodies_examined,
        operation_bodies_written: 1,
        operation_bodies_removed: resolved.len() as u64,
    })
}

pub(crate) fn plan_rematerialize(
    previous: SemanticResourceState,
    rematerialize: SemanticRematerialize,
    receipts: &[SemanticEditReceipt],
) -> Result<SemanticTransitionPlan, SemanticStoreError> {
    validate_coordinate(&rematerialize.coordinate)?;
    validate_resource_state(&previous)?;
    validate_active_receipts(&previous, receipts)?;
    if previous.source_revision != rematerialize.expected_source_revision {
        return Err(SemanticStoreError::SourceRevisionChanged);
    }
    if previous
        .generation
        .as_ref()
        .map(|generation| generation.materialization_id)
        != rematerialize.expected_current_materialization
    {
        return Err(SemanticStoreError::CurrentMaterializationChanged);
    }
    let source_revision =
        SourceRevision::reconcile(&previous.source_revision, rematerialize.source)?;
    let resolved = resolved_operations(rematerialize.resolved_operations)?;
    validate_resolution_partition(
        &previous.operations,
        &rematerialize.contributing_operations,
        &resolved.keys().copied().collect(),
    )?;
    let members = canonical_members(
        &previous.operations,
        rematerialize.contributing_operations,
        OperationId(u64::MAX),
    )?;
    if members.len() > MAX_CONTRIBUTING_OPERATIONS {
        return Err(SemanticStoreError::TooManyContributingOperations);
    }
    let operations = previous
        .operations
        .iter()
        .filter(|operation| !resolved.contains_key(&operation.operation_id))
        .cloned()
        .collect::<Vec<_>>();
    if operations
        .iter()
        .map(|op| op.operation_id)
        .collect::<BTreeSet<_>>()
        != members
    {
        return Err(SemanticStoreError::NonCanonicalOperationOrder);
    }
    let generation = match (operations.is_empty(), rematerialize.materialized) {
        (true, None) => None,
        (false, None) => None,
        (false, Some(materialized)) => {
            validate_materialized(&rematerialize.coordinate, &materialized)?;
            validate_successor_time(
                &materialized,
                Some(&source_revision),
                previous.generation.as_ref(),
                operations.iter(),
            )?;
            Some(SemanticGenerationRecord::from_unsigned(
                next_materialization_id(previous.last_materialization_id)?,
                members,
                materialized,
            )?)
        }
        _ => return Err(SemanticStoreError::MaterializationMembershipMismatch),
    };
    let current_materialization = generation
        .as_ref()
        .map(|generation| generation.materialization_id);
    let receipt_updates = receipts
        .iter()
        .map(|receipt| {
            let mut receipt = receipt.clone();
            if let Some(resolution) = resolved.get(&receipt.operation_id) {
                receipt.resolution = resolution.clone();
                receipt.current_materialization = None;
            } else if generation
                .as_ref()
                .is_some_and(|generation| generation.members.contains(&receipt.operation_id))
            {
                receipt.current_materialization = current_materialization;
            } else {
                receipt.current_materialization = None;
            }
            receipt
        })
        .collect();
    Ok(SemanticTransitionPlan {
        previous: Some(previous.clone()),
        next: (!operations.is_empty()).then(|| SemanticResourceState {
            coordinate: rematerialize.coordinate,
            source_revision,
            last_materialization_id: current_materialization.or(previous.last_materialization_id),
            operations,
            generation,
        }),
        receipt_updates,
        materialization_high_water: current_materialization.or(previous.last_materialization_id),
        operation_bodies_examined: previous.operations.len() as u64,
        operation_bodies_written: 0,
        operation_bodies_removed: resolved.len() as u64,
    })
}

fn validate_coordinate(coordinate: &Coordinate) -> Result<(), SemanticStoreError> {
    if crate::address_key::address_key_for_coordinate(coordinate).is_none()
        || coordinate.identifier.len() > MAX_COORDINATE_IDENTIFIER_BYTES
    {
        return Err(SemanticStoreError::InvalidCoordinate);
    }
    Ok(())
}

pub(crate) fn validate_materialized(
    coordinate: &Coordinate,
    materialized: &UnsignedEvent,
) -> Result<(), SemanticStoreError> {
    validate_coordinate(coordinate)?;
    let matches = materialized.pubkey == coordinate.public_key
        && materialized.kind == coordinate.kind
        && (!(30_000..=39_999).contains(&coordinate.kind.as_u16())
            || materialized.tags.identifier().unwrap_or("") == coordinate.identifier);
    if !matches || materialized.verify_id().is_err() {
        return Err(SemanticStoreError::MaterializationDoesNotMatchCoordinate);
    }
    if materialized
        .try_as_json()
        .map_err(|error| {
            PersistenceError::invariant(format!("encode semantic materialization: {error}"))
        })?
        .len()
        > MAX_MATERIALIZATION_BYTES
    {
        return Err(SemanticStoreError::MaterializationTooLarge);
    }
    Ok(())
}

fn validate_successor_time<'a>(
    materialized: &UnsignedEvent,
    source: Option<&SourceRevision>,
    previous: Option<&SemanticGenerationRecord>,
    contributing: impl IntoIterator<Item = &'a SemanticOperation>,
) -> Result<(), SemanticStoreError> {
    if source
        .and_then(SourceRevision::created_at)
        .is_some_and(|created_at| materialized.created_at <= created_at)
    {
        return Err(SemanticStoreError::MaterializationTimestampDidNotAdvance);
    }
    if let Some(previous) = previous {
        let previous_created_at = previous.materialize()?.event.unsigned().created_at;
        if materialized.created_at <= previous_created_at {
            return Err(SemanticStoreError::MaterializationTimestampDidNotAdvance);
        }
    }
    if contributing
        .into_iter()
        .map(|operation| operation.accepted_at)
        .max()
        .is_some_and(|accepted_at| materialized.created_at < accepted_at)
    {
        return Err(SemanticStoreError::MaterializationTimestampDidNotAdvance);
    }
    Ok(())
}

fn validate_resolution_partition(
    operations: &[SemanticOperation],
    contributing: &[OperationId],
    resolved: &BTreeSet<OperationId>,
) -> Result<(), SemanticStoreError> {
    let existing = operations
        .iter()
        .map(|operation| operation.operation_id)
        .collect::<Vec<_>>();
    let contributing_set = contributing.iter().copied().collect::<BTreeSet<_>>();
    if contributing.len() != contributing_set.len() {
        return Err(SemanticStoreError::NonCanonicalOperationOrder);
    }
    for operation_id in &existing {
        let retained = contributing_set.contains(operation_id);
        let removed = resolved.contains(operation_id);
        if retained == removed {
            return Err(if retained {
                SemanticStoreError::DuplicateOperation(*operation_id)
            } else {
                SemanticStoreError::UnknownOperation(*operation_id)
            });
        }
    }
    if contributing_set
        .iter()
        .chain(resolved.iter())
        .any(|id| !existing.contains(id))
    {
        return Err(SemanticStoreError::UnknownOperation(
            *contributing_set
                .iter()
                .chain(resolved.iter())
                .find(|id| !existing.contains(id))
                .expect("checked"),
        ));
    }
    let ordered = operations
        .iter()
        .filter(|operation| contributing_set.contains(&operation.operation_id))
        .map(|operation| operation.operation_id)
        .collect::<Vec<_>>();
    if ordered != contributing {
        return Err(SemanticStoreError::NonCanonicalOperationOrder);
    }
    Ok(())
}

pub(crate) fn validate_active_receipts(
    state: &SemanticResourceState,
    receipts: &[SemanticEditReceipt],
) -> Result<(), SemanticStoreError> {
    let current_materialization = state
        .generation
        .as_ref()
        .map(|generation| generation.materialization_id);
    let valid = receipts.len() == state.operations.len()
        && receipts
            .iter()
            .zip(&state.operations)
            .all(|(receipt, operation)| {
                receipt.operation_id == operation.operation_id
                    && receipt.coordinate == state.coordinate
                    && receipt.accepted_at == operation.accepted_at
                    && receipt.resolution == OperationResolution::Contributing
                    && receipt.current_materialization == current_materialization
            });
    if !valid {
        return Err(PersistenceError::invariant(
            "active semantic operations and receipts disagree",
        )
        .into());
    }
    Ok(())
}

pub(crate) fn validate_resource_state(
    state: &SemanticResourceState,
) -> Result<(), SemanticStoreError> {
    let operation_ids = state
        .operations
        .iter()
        .map(|operation| operation.operation_id)
        .collect::<BTreeSet<_>>();
    let valid = !state.operations.is_empty()
        && operation_ids.len() == state.operations.len()
        && state
            .operations
            .windows(2)
            .all(|pair| pair[0].operation_id < pair[1].operation_id)
        && state
            .generation
            .as_ref()
            .is_none_or(|generation| generation.members == operation_ids)
        && state.generation.as_ref().is_none_or(|generation| {
            state.last_materialization_id == Some(generation.materialization_id)
        });
    if !valid {
        return Err(PersistenceError::invariant("invalid active semantic resource state").into());
    }
    Ok(())
}

fn resolved_operations(
    resolved: Vec<ResolvedOperation>,
) -> Result<BTreeMap<OperationId, OperationResolution>, SemanticStoreError> {
    let mut by_id = BTreeMap::new();
    for resolved in resolved {
        if matches!(
            &resolved.resolution,
            OperationResolution::Refused(reason)
                if reason.len() > MAX_RESOLUTION_REASON_BYTES
        ) {
            return Err(SemanticStoreError::ResolutionReasonTooLarge);
        }
        if matches!(resolved.resolution, OperationResolution::Contributing) {
            return Err(SemanticStoreError::NonCanonicalOperationOrder);
        }
        if by_id
            .insert(resolved.operation_id, resolved.resolution)
            .is_some()
        {
            return Err(SemanticStoreError::DuplicateOperation(
                resolved.operation_id,
            ));
        }
    }
    Ok(by_id)
}

fn canonical_members(
    operations: &[SemanticOperation],
    proposed: Vec<OperationId>,
    accepted: OperationId,
) -> Result<BTreeSet<OperationId>, SemanticStoreError> {
    let mut known = operations
        .iter()
        .map(|operation| operation.operation_id)
        .collect::<BTreeSet<_>>();
    if accepted != OperationId(u64::MAX) {
        known.insert(accepted);
    }
    let proposed_set = proposed.iter().copied().collect::<BTreeSet<_>>();
    if proposed.len() != proposed_set.len() {
        return Err(SemanticStoreError::NonCanonicalOperationOrder);
    }
    if proposed_set.iter().any(|id| !known.contains(id)) {
        return Err(SemanticStoreError::UnknownOperation(
            *proposed_set
                .iter()
                .find(|id| !known.contains(id))
                .expect("checked"),
        ));
    }
    let expected = operations
        .iter()
        .filter(|op| proposed_set.contains(&op.operation_id))
        .map(|op| op.operation_id)
        .chain((accepted != OperationId(u64::MAX)).then_some(accepted))
        .collect::<Vec<_>>();
    if expected != proposed {
        return Err(SemanticStoreError::NonCanonicalOperationOrder);
    }
    Ok(proposed_set)
}

fn next_materialization_id(
    high_water: Option<MaterializationId>,
) -> Result<MaterializationId, SemanticStoreError> {
    let next = high_water
        .map(|materialization_id| materialization_id.0)
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(SemanticStoreError::MaterializationIdExhausted)?;
    Ok(MaterializationId(next))
}

pub(crate) fn promote_current(
    mut current: SemanticResourceState,
    promotion: &SemanticPromotion,
) -> Result<(SemanticPromotionOutcome, Option<SemanticResourceState>), SemanticStoreError> {
    let Some(generation) = current.generation.as_ref() else {
        return Ok((SemanticPromotionOutcome::Stale, None));
    };
    if current.coordinate != promotion.coordinate
        || current.source_revision != promotion.expected_source_revision
        || generation.materialization_id != promotion.expected_materialization
        || generation.materialize()?.event.event_id() != promotion.verified.event_id()
    {
        return Ok((SemanticPromotionOutcome::Stale, None));
    }
    if matches!(&generation.body, SemanticMaterializationRecord::Signed(_)) {
        return Err(SemanticStoreError::MaterializationAlreadySigned);
    }
    let SemanticMaterializationRecord::Pending(unsigned_json) = &generation.body else {
        unreachable!("signed case returned above")
    };
    let unsigned = UnsignedEvent::from_json(unsigned_json).map_err(|error| {
        PersistenceError::invariant(format!("decode semantic materialization: {error}"))
    })?;
    let signed = unsigned
        .add_signature(promotion.verified.signature())
        .map_err(|error| {
            PersistenceError::invariant(format!(
                "verified semantic signature did not bind stored body: {error}"
            ))
        })?;
    let signed_json = signed.try_as_json().map_err(|error| {
        PersistenceError::invariant(format!("encode signed semantic materialization: {error}"))
    })?;
    if signed_json.len() > MAX_MATERIALIZATION_BYTES {
        return Err(SemanticStoreError::MaterializationTooLarge);
    }
    current
        .generation
        .as_mut()
        .expect("generation checked above")
        .body = SemanticMaterializationRecord::Signed(signed_json);
    let outcome = SemanticPromotionOutcome::Promoted(
        current
            .generation
            .as_ref()
            .expect("generation retained")
            .materialize()?,
    );
    Ok((outcome, Some(current)))
}

pub(crate) fn recovered(
    state: SemanticResourceState,
) -> Result<RecoveredSemanticResource, SemanticStoreError> {
    Ok(RecoveredSemanticResource {
        coordinate: state.coordinate,
        operations: state.operations,
        current: SemanticCurrentState {
            source_revision: state.source_revision,
            generation: state
                .generation
                .map(|generation| generation.materialize())
                .transpose()?,
        },
    })
}

// The concrete impls live with their storage layouts.
const _: fn(&MemoryStore) = |_| {};
const _: fn(&RedbStore) = |_| {};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nostr::{Keys, Kind, Tag};
    use tempfile::TempDir;

    use super::*;

    const SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    fn keys() -> Keys {
        Keys::parse(SECRET).expect("fixed semantic-edit key")
    }

    fn coordinate(identifier: &str) -> Coordinate {
        Coordinate::new(Kind::from(30_001u16), keys().public_key()).identifier(identifier)
    }

    fn unsigned(coordinate: &Coordinate, created_at: u64, content: &str) -> UnsignedEvent {
        let mut event = UnsignedEvent::new(
            coordinate.public_key,
            Timestamp::from(created_at),
            coordinate.kind,
            [Tag::identifier(coordinate.identifier.clone())],
            content,
        );
        event.ensure_id();
        event
    }

    fn no_source() -> Option<(EventId, Timestamp)> {
        None
    }

    fn source(created_at: u64) -> Option<(EventId, Timestamp)> {
        let event = unsigned(&coordinate("source"), created_at, "source")
            .sign_with_keys(&keys())
            .expect("sign source fixture");
        Some((event.id, event.created_at))
    }

    fn plan(byte: u8) -> SemanticPlan {
        SemanticPlan::new(1, vec![byte]).expect("valid opaque plan")
    }

    fn first_accept(
        coordinate: &Coordinate,
        source: Option<(EventId, Timestamp)>,
        created_at: u64,
        content: &str,
        plan_byte: u8,
    ) -> SemanticAccept {
        SemanticAccept {
            coordinate: coordinate.clone(),
            expected_source_revision: None,
            expected_current_materialization: None,
            source,
            accepted_at: Timestamp::from(created_at),
            plan: plan(plan_byte),
            materialized: Some(unsigned(coordinate, created_at, content)),
            contributing_operations: Vec::new(),
            resolved_operations: Vec::new(),
        }
    }

    fn successor_accept(
        coordinate: &Coordinate,
        source: Option<(EventId, Timestamp)>,
        previous: &SemanticCurrentState,
        contributing_operations: Vec<OperationId>,
        resolved_operations: Vec<ResolvedOperation>,
        created_at: u64,
        content: &str,
        plan_byte: u8,
    ) -> SemanticAccept {
        SemanticAccept {
            coordinate: coordinate.clone(),
            expected_source_revision: Some(previous.source_revision.clone()),
            expected_current_materialization: previous
                .generation
                .as_ref()
                .map(|generation| generation.materialization_id),
            source,
            accepted_at: Timestamp::from(created_at),
            plan: plan(plan_byte),
            materialized: Some(unsigned(coordinate, created_at, content)),
            contributing_operations,
            resolved_operations,
        }
    }

    fn resource_digest(store: &dyn SemanticEditStore, max_operation: u64) -> String {
        let resources = store
            .recover_semantic_resources()
            .expect("recover semantic resources")
            .into_iter()
            .map(|resource| {
                (
                    format!(
                        "{}:{}:{}",
                        resource.coordinate.public_key.to_hex(),
                        resource.coordinate.kind.as_u16(),
                        resource.coordinate.identifier
                    ),
                    format!(
                        "{:?}|{:?}|{:?}|{:?}",
                        resource.current.source_revision,
                        resource.operations,
                        resource
                            .current
                            .generation
                            .as_ref()
                            .map(|generation| generation.materialization_id),
                        resource
                            .current
                            .generation
                            .as_ref()
                            .map(|generation| &generation.event)
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let receipts = (1..=max_operation)
            .filter_map(|id| {
                store
                    .semantic_receipt(OperationId(id))
                    .expect("read semantic receipt")
                    .map(|receipt| (id, format!("{receipt:?}")))
            })
            .collect::<BTreeMap<_, _>>();
        blake3::hash(format!("{resources:?}|{receipts:?}").as_bytes())
            .to_hex()
            .to_string()
    }

    fn assert_receipts_follow_successor(store: &mut dyn SemanticEditStore) {
        let target = coordinate("shared");
        let source_revision = no_source();
        let (first_receipt, first) = store
            .accept_semantic_edit(first_accept(
                &target,
                source_revision.clone(),
                1,
                "alice",
                1,
            ))
            .expect("accept first operation");
        let (second_receipt, second) = store
            .accept_semantic_edit(successor_accept(
                &target,
                source_revision,
                &first,
                vec![first_receipt.operation_id],
                Vec::new(),
                2,
                "alice+bob",
                2,
            ))
            .expect("accept compatible operation");

        let second_generation = second.generation.as_ref().expect("second body");
        assert_eq!(second_generation.members.len(), 2);
        assert_eq!(
            store
                .semantic_receipt(first_receipt.operation_id)
                .unwrap()
                .unwrap()
                .current_materialization,
            Some(second_generation.materialization_id),
            "a prior contributing receipt must move to the shared successor"
        );
        assert_eq!(
            store
                .semantic_receipt(second_receipt.operation_id)
                .unwrap()
                .unwrap()
                .current_materialization,
            Some(second_generation.materialization_id)
        );
        let recovered = store.recover_semantic_resources().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].operations.len(), 2);
        assert_eq!(
            recovered[0]
                .current
                .generation
                .as_ref()
                .expect("recovered body")
                .members
                .len(),
            2
        );
    }

    fn assert_content_pending_round_trip(store: &mut dyn SemanticEditStore) {
        let target = coordinate("content-pending");
        let mut accept = first_accept(&target, no_source(), 10, "unused", 7);
        accept.materialized = None;
        let (receipt, current) = store.accept_semantic_edit(accept).unwrap();
        assert_eq!(current.source_revision.ordinal(), 0);
        assert_eq!(current.generation, None);
        assert_eq!(receipt.current_materialization, None);

        let recovered = store.recover_semantic_resources().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].operations.len(), 1);
        assert_eq!(recovered[0].operations[0].plan.bytes(), &[7]);
        assert_eq!(recovered[0].current.generation, None);

        let materialized = store
            .rematerialize_semantic_edit(SemanticRematerialize {
                coordinate: target.clone(),
                expected_source_revision: current.source_revision,
                expected_current_materialization: None,
                source: no_source(),
                materialized: Some(unsigned(&target, 10, "materialized later")),
                contributing_operations: vec![receipt.operation_id],
                resolved_operations: Vec::new(),
            })
            .unwrap()
            .expect("active resource remains");
        let generation = materialized.generation.expect("first body installed");
        assert_eq!(generation.materialization_id, MaterializationId(1));
        assert_eq!(generation.members, BTreeSet::from([receipt.operation_id]));
        assert_eq!(
            store
                .semantic_receipt(receipt.operation_id)
                .unwrap()
                .unwrap()
                .current_materialization,
            Some(MaterializationId(1))
        );
    }

    #[test]
    fn content_pending_acceptance_persists_without_an_event_body_then_materializes() {
        assert_content_pending_round_trip(&mut MemoryStore::new());
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pending.redb");
        {
            let mut redb = RedbStore::open(&path).unwrap();
            let target = coordinate("pending-reopen");
            let mut accept = first_accept(&target, no_source(), 10, "unused", 8);
            accept.materialized = None;
            let (receipt, current) = redb.accept_semantic_edit(accept).unwrap();
            assert_eq!(current.generation, None);
            assert_eq!(receipt.current_materialization, None);
        }
        {
            let reopened = RedbStore::open(&path).unwrap();
            let recovered = reopened.recover_semantic_resources().unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].operations[0].plan.bytes(), &[8]);
            assert_eq!(recovered[0].current.generation, None);
        }
        assert_content_pending_round_trip(
            &mut RedbStore::open(dir.path().join("pending-full.redb")).unwrap(),
        );
    }

    fn assert_inactive_recreation_uses_successor_id(store: &mut dyn SemanticEditStore) {
        let target = coordinate("inactive");
        let (receipt, current) = store
            .accept_semantic_edit(first_accept(&target, no_source(), 1, "first", 1))
            .unwrap();
        let first_generation = current.generation.as_ref().unwrap();
        assert_eq!(first_generation.materialization_id, MaterializationId(1));
        let inactive = store
            .rematerialize_semantic_edit(SemanticRematerialize {
                coordinate: target.clone(),
                expected_source_revision: current.source_revision.clone(),
                expected_current_materialization: Some(first_generation.materialization_id),
                source: no_source(),
                materialized: None,
                contributing_operations: Vec::new(),
                resolved_operations: vec![ResolvedOperation {
                    operation_id: receipt.operation_id,
                    resolution: OperationResolution::Resolved,
                }],
            })
            .unwrap();
        assert_eq!(inactive, None);
        assert!(store.recover_semantic_resources().unwrap().is_empty());
        assert_eq!(
            store
                .semantic_receipt(receipt.operation_id)
                .unwrap()
                .unwrap()
                .resolution,
            OperationResolution::Resolved
        );
        let (_, recreated) = store
            .accept_semantic_edit(first_accept(&target, no_source(), 2, "second", 2))
            .unwrap();
        assert_eq!(
            recreated.generation.unwrap().materialization_id,
            MaterializationId(2),
            "the inactive high-water must prevent old signatures matching a recreated body"
        );
    }

    #[test]
    fn all_resolved_removes_active_resource_but_retains_receipt_and_generation_high_water() {
        assert_inactive_recreation_uses_successor_id(&mut MemoryStore::new());
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("inactive.redb");
        {
            let mut redb = RedbStore::open(&path).unwrap();
            let target = coordinate("inactive-reopen");
            let (receipt, current) = redb
                .accept_semantic_edit(first_accept(&target, no_source(), 1, "first", 1))
                .unwrap();
            redb.rematerialize_semantic_edit(SemanticRematerialize {
                coordinate: target,
                expected_source_revision: current.source_revision,
                expected_current_materialization: Some(
                    current.generation.unwrap().materialization_id,
                ),
                source: no_source(),
                materialized: None,
                contributing_operations: Vec::new(),
                resolved_operations: vec![ResolvedOperation {
                    operation_id: receipt.operation_id,
                    resolution: OperationResolution::Cancelled,
                }],
            })
            .unwrap();
        }
        {
            let mut reopened = RedbStore::open(&path).unwrap();
            let (_, recreated) = reopened
                .accept_semantic_edit(first_accept(
                    &coordinate("inactive-reopen"),
                    no_source(),
                    2,
                    "second",
                    2,
                ))
                .unwrap();
            assert_eq!(
                recreated.generation.unwrap().materialization_id,
                MaterializationId(2)
            );
        }
        assert_inactive_recreation_uses_successor_id(
            &mut RedbStore::open(dir.path().join("inactive-full.redb")).unwrap(),
        );
    }

    #[test]
    fn source_revision_is_store_owned_and_stale_fenced() {
        let target = coordinate("source-revision");
        let first_source = source(5);
        let second_source = source(8);
        let mut store = MemoryStore::new();
        let (receipt, first) = store
            .accept_semantic_edit(first_accept(&target, first_source, 6, "first", 1))
            .unwrap();
        assert_eq!(first.source_revision.ordinal(), 1);
        let second = store
            .rematerialize_semantic_edit(SemanticRematerialize {
                coordinate: target.clone(),
                expected_source_revision: first.source_revision.clone(),
                expected_current_materialization: Some(
                    first.generation.as_ref().unwrap().materialization_id,
                ),
                source: second_source,
                materialized: Some(unsigned(&target, 9, "rebased")),
                contributing_operations: vec![receipt.operation_id],
                resolved_operations: Vec::new(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(second.source_revision.ordinal(), 2);
        let before = resource_digest(&store, 1);
        let stale = store
            .rematerialize_semantic_edit(SemanticRematerialize {
                coordinate: target.clone(),
                expected_source_revision: first.source_revision,
                expected_current_materialization: Some(
                    second.generation.as_ref().unwrap().materialization_id,
                ),
                source: source(10),
                materialized: Some(unsigned(&target, 11, "stale")),
                contributing_operations: vec![receipt.operation_id],
                resolved_operations: Vec::new(),
            })
            .unwrap_err();
        assert!(matches!(stale, SemanticStoreError::SourceRevisionChanged));
        assert_eq!(resource_digest(&store, 1), before);
        assert!(SourceRevision::from_parts(0, source(5).map(|source| source.0), None).is_err());
        assert!(SourceRevision::from_parts(1, None, None).is_err());
    }

    #[test]
    fn generation_high_water_exhaustion_is_typed_before_any_transition() {
        let target = coordinate("exhausted");
        let error = plan_accept(
            None,
            &[],
            Some(MaterializationId(u64::MAX)),
            OperationId(1),
            first_accept(&target, no_source(), 1, "body", 1),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SemanticStoreError::MaterializationIdExhausted
        ));
        let exhausted_source = SourceRevision::from_parts(
            u64::MAX,
            source(1).map(|source| source.0),
            Some(Timestamp::from(1)),
        )
        .unwrap();
        assert!(matches!(
            SourceRevision::reconcile(&exhausted_source, source(2)),
            Err(SemanticStoreError::SourceRevisionExhausted)
        ));
    }

    #[test]
    fn materialization_cannot_predate_a_contributing_operation() {
        let target = coordinate("operation-time");
        let mut accept = first_accept(&target, no_source(), 10, "body", 1);
        accept.materialized = Some(unsigned(&target, 9, "too old"));
        let mut store = MemoryStore::new();
        assert!(matches!(
            store.accept_semantic_edit(accept),
            Err(SemanticStoreError::MaterializationTimestampDidNotAdvance)
        ));
        assert!(store.recover_semantic_resources().unwrap().is_empty());
        assert!(store.semantic_receipt(OperationId(1)).unwrap().is_none());
    }

    #[test]
    fn deterministic_state_machine_matches_memory_redb_and_reference_after_every_step() {
        let target = coordinate("state-machine");
        let dir = TempDir::new().unwrap();
        let mut memory = MemoryStore::new();
        let mut redb = RedbStore::open(dir.path().join("state-machine.redb")).unwrap();
        let mut live = Vec::<OperationId>::new();
        let mut terminal = Vec::<OperationId>::new();

        for step in 1..=24u64 {
            let previous = memory
                .recover_semantic_resources()
                .unwrap()
                .into_iter()
                .next();
            let resolved = if step % 5 == 0 && !live.is_empty() {
                let operation_id = live.remove(0);
                terminal.push(operation_id);
                vec![ResolvedOperation {
                    operation_id,
                    resolution: OperationResolution::Resolved,
                }]
            } else {
                Vec::new()
            };
            let mut command = SemanticAccept {
                coordinate: target.clone(),
                expected_source_revision: previous
                    .as_ref()
                    .map(|resource| resource.current.source_revision.clone()),
                expected_current_materialization: previous.as_ref().and_then(|resource| {
                    resource
                        .current
                        .generation
                        .as_ref()
                        .map(|generation| generation.materialization_id)
                }),
                source: no_source(),
                accepted_at: Timestamp::from(step),
                plan: plan((step % 251) as u8),
                materialized: Some(unsigned(&target, step, &format!("state-{step}"))),
                contributing_operations: live.clone(),
                resolved_operations: resolved,
            };
            if step % 7 == 0 {
                command.materialized = None;
            }
            let memory_outcome = memory.accept_semantic_edit(command.clone()).unwrap();
            let redb_outcome = redb.accept_semantic_edit(command).unwrap();
            assert_eq!(
                memory_outcome, redb_outcome,
                "outcome mismatch at step {step}"
            );
            live.push(memory_outcome.0.operation_id);
            assert_eq!(resource_digest(&memory, step), resource_digest(&redb, step));

            for store in [
                &memory as &dyn SemanticEditStore,
                &redb as &dyn SemanticEditStore,
            ] {
                let resource = store.recover_semantic_resources().unwrap().remove(0);
                assert_eq!(
                    resource
                        .operations
                        .iter()
                        .map(|operation| operation.operation_id)
                        .collect::<Vec<_>>(),
                    live
                );
                assert_eq!(
                    resource
                        .current
                        .generation
                        .as_ref()
                        .map(|generation| generation.members.clone()),
                    (step % 7 != 0).then(|| live.iter().copied().collect())
                );
                for operation_id in &terminal {
                    assert_eq!(
                        store
                            .semantic_receipt(*operation_id)
                            .unwrap()
                            .unwrap()
                            .resolution,
                        OperationResolution::Resolved
                    );
                }
            }
        }
    }

    #[test]
    fn compatible_operations_share_one_body_and_move_both_receipts() {
        assert_receipts_follow_successor(&mut MemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        assert_receipts_follow_successor(
            &mut RedbStore::open(dir.path().join("store.redb")).unwrap(),
        );
    }

    #[test]
    fn acceptance_only_compacts_explicitly_terminal_operations() {
        let target = coordinate("normalize");
        let source_revision = no_source();
        let mut store = MemoryStore::new();
        let (first_receipt, first) = store
            .accept_semantic_edit(first_accept(
                &target,
                source_revision.clone(),
                1,
                "first",
                1,
            ))
            .unwrap();
        let (second_receipt, second) = store
            .accept_semantic_edit(successor_accept(
                &target,
                source_revision.clone(),
                &first,
                vec![first_receipt.operation_id],
                Vec::new(),
                2,
                "first+second",
                2,
            ))
            .unwrap();
        store.reset_semantic_store_counters();
        let (_third_receipt, third) = store
            .accept_semantic_edit(successor_accept(
                &target,
                source_revision,
                &second,
                vec![second_receipt.operation_id],
                vec![ResolvedOperation {
                    operation_id: first_receipt.operation_id,
                    resolution: OperationResolution::Cancelled,
                }],
                3,
                "second+third",
                3,
            ))
            .unwrap();
        assert_eq!(third.generation.as_ref().unwrap().members.len(), 2);
        let recovered = store.recover_semantic_resources().unwrap();
        assert_eq!(recovered[0].operations.len(), 2);
        let first = store
            .semantic_receipt(first_receipt.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(first.resolution, OperationResolution::Cancelled);
        assert_eq!(first.current_materialization, None);
        let counters = store.semantic_store_counters();
        assert_eq!(counters.operation_bodies_examined, 4); // 2 for accept + 2 for recovery.
        assert_eq!(counters.operation_bodies_removed, 1);
        assert_eq!(counters.operation_bodies_written, 1);
        assert_eq!(counters.commits, 1);
    }

    #[test]
    fn materialization_must_outrank_source_and_prior_generation_without_burning_an_id() {
        let target = coordinate("timestamps");
        let source_revision = source(5);
        let mut store = MemoryStore::new();
        let (first_receipt, first) = store
            .accept_semantic_edit(first_accept(
                &target,
                source_revision.clone(),
                6,
                "first",
                1,
            ))
            .unwrap();
        let before = resource_digest(&store, 1);
        let error = store
            .accept_semantic_edit(successor_accept(
                &target,
                source_revision.clone(),
                &first,
                vec![first_receipt.operation_id],
                Vec::new(),
                6,
                "not-newer",
                2,
            ))
            .unwrap_err();
        assert!(matches!(
            error,
            SemanticStoreError::MaterializationTimestampDidNotAdvance
        ));
        assert_eq!(resource_digest(&store, 1), before);
        let (second_receipt, _) = store
            .accept_semantic_edit(successor_accept(
                &target,
                source_revision,
                &first,
                vec![first_receipt.operation_id],
                Vec::new(),
                7,
                "newer",
                3,
            ))
            .unwrap();
        assert_eq!(second_receipt.operation_id, OperationId(2));
    }

    #[test]
    fn stale_signature_cannot_promote_a_successor() {
        let target = coordinate("signature");
        let source_revision = no_source();
        let mut store = MemoryStore::new();
        let first_unsigned = unsigned(&target, 1, "first");
        let (first_receipt, first) = store
            .accept_semantic_edit(SemanticAccept {
                materialized: Some(first_unsigned.clone()),
                ..first_accept(&target, source_revision.clone(), 1, "first", 1)
            })
            .unwrap();
        let (_, second) = store
            .accept_semantic_edit(successor_accept(
                &target,
                source_revision.clone(),
                &first,
                vec![first_receipt.operation_id],
                Vec::new(),
                2,
                "second",
                2,
            ))
            .unwrap();
        let signed_first = first_unsigned.sign_with_keys(&keys()).unwrap();
        let outcome = store
            .promote_semantic_materialization(SemanticPromotion {
                coordinate: target.clone(),
                expected_source_revision: first.source_revision.clone(),
                expected_materialization: first.generation.as_ref().unwrap().materialization_id,
                verified: VerifiedSignature::verify(&signed_first).unwrap(),
            })
            .unwrap();
        assert_eq!(outcome, SemanticPromotionOutcome::Stale);
        let recovered = store.recover_semantic_resources().unwrap();
        assert_eq!(
            recovered[0]
                .current
                .generation
                .as_ref()
                .unwrap()
                .materialization_id,
            second.generation.as_ref().unwrap().materialization_id
        );
        assert!(matches!(
            recovered[0].current.generation.as_ref().unwrap().event,
            CurrentMaterialization::Pending(_)
        ));
    }

    #[test]
    fn redb_reopen_is_read_only_and_matches_memory_canonical_state() {
        let target = coordinate("parity");
        let source_revision = no_source();
        let mut memory = MemoryStore::new();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.redb");
        let mut redb = RedbStore::open(&path).unwrap();
        for store in [
            &mut memory as &mut dyn SemanticEditStore,
            &mut redb as &mut dyn SemanticEditStore,
        ] {
            let (first_receipt, first) = store
                .accept_semantic_edit(first_accept(&target, source_revision.clone(), 1, "one", 1))
                .unwrap();
            store
                .accept_semantic_edit(successor_accept(
                    &target,
                    source_revision.clone(),
                    &first,
                    vec![first_receipt.operation_id],
                    Vec::new(),
                    2,
                    "two",
                    2,
                ))
                .unwrap();
        }
        assert_eq!(resource_digest(&memory, 2), resource_digest(&redb, 2));
        let before = resource_digest(&redb, 2);
        drop(redb);
        let reopened = RedbStore::open(&path).unwrap();
        reopened.reset_semantic_store_counters();
        assert_eq!(resource_digest(&reopened, 2), before);
        let counters = reopened.semantic_store_counters();
        assert_eq!(counters.commits, 0, "unchanged recovery must write nothing");
        assert_eq!(counters.recovery_rows, 1);
        assert_eq!(counters.operation_bodies_examined, 2);
    }

    #[test]
    fn hot_coordinate_compaction_retains_receipts_but_one_live_program() {
        let target = coordinate("hot");
        let source_revision = no_source();
        let mut store = MemoryStore::new();
        let (first_receipt, mut generation) = store
            .accept_semantic_edit(first_accept(&target, source_revision.clone(), 1, "1", 1))
            .unwrap();
        let mut operations = vec![first_receipt.operation_id];
        for index in 2..=1_000u64 {
            let (receipt, next) = store
                .accept_semantic_edit(successor_accept(
                    &target,
                    source_revision.clone(),
                    &generation,
                    operations.clone(),
                    Vec::new(),
                    index,
                    &index.to_string(),
                    (index % 251) as u8,
                ))
                .unwrap();
            operations.push(receipt.operation_id);
            generation = next;
        }
        store.reset_semantic_store_counters();
        let resolved = operations
            .iter()
            .copied()
            .map(|operation_id| ResolvedOperation {
                operation_id,
                resolution: OperationResolution::Resolved,
            })
            .collect();
        let (last, _) = store
            .accept_semantic_edit(successor_accept(
                &target,
                source_revision,
                &generation,
                Vec::new(),
                resolved,
                1_001,
                "compacted",
                9,
            ))
            .unwrap();
        let counters = store.semantic_store_counters();
        assert_eq!(counters.operation_bodies_examined, 1_000);
        assert_eq!(counters.operation_bodies_removed, 1_000);
        assert_eq!(counters.operation_bodies_written, 1);
        assert_eq!(counters.commits, 1);
        let recovered = store.recover_semantic_resources().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].operations.len(), 1);
        assert_eq!(recovered[0].operations[0].operation_id, last.operation_id);
        for operation_id in 1..=1_001 {
            assert!(store
                .semantic_receipt(OperationId(operation_id))
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn one_coordinate_update_does_not_examine_unrelated_resources() {
        let source_revision = no_source();
        let mut store = MemoryStore::new();
        let mut current = Vec::new();
        for index in 0..300u64 {
            let target = coordinate(&format!("resource-{index:03}"));
            let (receipt, generation) = store
                .accept_semantic_edit(first_accept(
                    &target,
                    source_revision.clone(),
                    1,
                    "first",
                    1,
                ))
                .unwrap();
            current.push((target, receipt, generation));
        }
        store.reset_semantic_store_counters();
        let (target, receipt, generation) = &current[137];
        store
            .accept_semantic_edit(successor_accept(
                target,
                source_revision,
                generation,
                vec![receipt.operation_id],
                Vec::new(),
                2,
                "second",
                2,
            ))
            .unwrap();
        assert_eq!(
            store.semantic_store_counters(),
            SemanticStoreCounters {
                coordinate_point_reads: 1,
                operation_bodies_examined: 1,
                operation_bodies_written: 1,
                operation_bodies_removed: 0,
                materializations_written: 1,
                materializations_removed: 1,
                commits: 1,
                recovery_rows: 0,
            }
        );
    }

    #[test]
    fn redb_point_update_touches_only_the_target_coordinate() {
        let dir = TempDir::new().unwrap();
        let mut store = RedbStore::open(dir.path().join("point.redb")).unwrap();
        let mut current = Vec::new();
        for index in 0..64u64 {
            let target = coordinate(&format!("redb-resource-{index:03}"));
            let (receipt, generation) = store
                .accept_semantic_edit(first_accept(&target, no_source(), 1, "first", 1))
                .unwrap();
            current.push((target, receipt, generation));
        }
        store.reset_semantic_store_counters();
        let (target, receipt, generation) = &current[31];
        store
            .accept_semantic_edit(successor_accept(
                target,
                no_source(),
                generation,
                vec![receipt.operation_id],
                Vec::new(),
                2,
                "second",
                2,
            ))
            .unwrap();
        assert_eq!(
            store.semantic_store_counters(),
            SemanticStoreCounters {
                coordinate_point_reads: 1,
                operation_bodies_examined: 1,
                operation_bodies_written: 1,
                operation_bodies_removed: 0,
                materializations_written: 1,
                materializations_removed: 1,
                commits: 1,
                recovery_rows: 0,
            }
        );
    }
}
