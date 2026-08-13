//! Backend-neutral transitions for durable replaceable operations.
//!
//! The store never interprets a replay program. It owns ordering, exact CAS
//! witnesses, materialization identity, and the ordinary intent/receipt links.
//! Full event bytes live only in the canonical event table.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::AccessContext;
use nostr::nips::nip01::Coordinate;
use nostr::{EventId, RelayUrl, Timestamp, UnsignedEvent};

use crate::{IntentId, IntentSigState, MaterializationRef, PersistenceError, StoredEvent};

pub(crate) const MAX_PROGRAM_BYTES: usize = 64 * 1024;
pub(crate) const MAX_COORDINATE_IDENTIFIER_BYTES: usize = 65_536;
pub(crate) const MAX_CONTRIBUTING_OPERATIONS: usize = 1_000_000;
pub(crate) const MAX_RESOLUTION_REASON_BYTES: usize = 4_096;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct MaterializationId(pub u64);

/// Exact owner/implementation identity of an opaque replay program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReplayProgramId(pub [u8; 16]);

/// Exact durable encoding contract for opaque replay bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReplayFormatId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourcePlanId(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccessContextId(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceRoundId(pub [u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemanticSource {
    pub relay: RelayUrl,
    pub access: AccessContext,
}

impl SemanticSource {
    #[must_use]
    pub fn new(relay: RelayUrl, access: AccessContext) -> Self {
        Self { relay, access }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticSourceRequest {
    pub round: SourceRoundId,
    pub source: SemanticSource,
    pub transport_generation: u64,
    pub request_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticSourceTerminal {
    Eose,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticSourceMemberState {
    Pending,
    Open(SemanticSourceRequest),
    Settled {
        request: SemanticSourceRequest,
        terminal: SemanticSourceTerminal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiniteSemanticSourceRound {
    pub id: SourceRoundId,
    pub sources: BTreeMap<SemanticSource, SemanticSourceMemberState>,
}

impl FiniteSemanticSourceRound {
    pub fn new(
        id: SourceRoundId,
        sources: BTreeSet<SemanticSource>,
    ) -> Result<Self, SemanticRefusal> {
        if sources.is_empty() {
            return Err(SemanticRefusal::EmptySourceRound);
        }
        Ok(Self {
            id,
            sources: sources
                .into_iter()
                .map(|source| (source, SemanticSourceMemberState::Pending))
                .collect(),
        })
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.sources
            .values()
            .all(|state| matches!(state, SemanticSourceMemberState::Settled { .. }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SemanticSourcePolicy {
    #[default]
    Continuing,
    Finite(FiniteSemanticSourceRound),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticSourceRoundFact {
    RequestOpened(SemanticSourceRequest),
    RequestSettled {
        request: SemanticSourceRequest,
        terminal: SemanticSourceTerminal,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticSourceRoundOutcome {
    Advanced,
    AlreadyApplied,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticDestinationPlanClosure {
    NoDestinations,
    AllCurrentDestinationsTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticCohortClose {
    pub coordinate: Coordinate,
    pub expected_source_revision: SourceRevision,
    pub expected_program_digest: SemanticProgramDigest,
    pub expected_materialization: MaterializationRef,
    pub destination: SemanticDestinationPlanClosure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticCohortCloseOutcome {
    Closed { members: Vec<IntentId> },
    SourceRoundOpen,
    DestinationOpen,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticPlan {
    version: u16,
    bytes: Vec<u8>,
}

impl SemanticPlan {
    pub fn new(version: u16, bytes: Vec<u8>) -> Result<Self, SemanticRefusal> {
        if version == 0 {
            return Err(SemanticRefusal::InvalidPlanVersion);
        }
        if bytes.len() > MAX_PROGRAM_BYTES {
            return Err(SemanticRefusal::PlanTooLarge);
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
    pub intent_id: IntentId,
    pub resolution: OperationResolution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartingSource {
    Absent,
    Event(EventId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartingSourceRequirement {
    pub plan: SourcePlanId,
    pub access: AccessContextId,
    pub source: StartingSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationSourceRequirement {
    Awaiting(StartingSourceRequirement),
    Qualified(StartingSourceRequirement),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualifiedSource {
    Unresolved,
    Absent,
    Event {
        event_id: EventId,
        created_at: Timestamp,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEvidence {
    pub plan: SourcePlanId,
    pub access: AccessContextId,
    pub qualified: QualifiedSource,
}

/// Store-owned ordinal plus the complete evidence identity used by CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRevision {
    ordinal: u64,
    evidence: SourceEvidence,
}

impl SourceRevision {
    fn initial(evidence: SourceEvidence) -> Self {
        Self {
            ordinal: u64::from(!matches!(evidence.qualified, QualifiedSource::Unresolved)),
            evidence,
        }
    }

    fn reconcile(previous: &Self, evidence: SourceEvidence) -> Result<Self, SemanticRefusal> {
        if previous.evidence == evidence {
            return Ok(previous.clone());
        }
        Ok(Self {
            ordinal: previous
                .ordinal
                .checked_add(1)
                .ok_or(SemanticRefusal::SourceRevisionExhausted)?,
            evidence,
        })
    }

    pub(crate) fn from_parts(
        ordinal: u64,
        evidence: SourceEvidence,
    ) -> Result<Self, SemanticRefusal> {
        if ordinal == 0 && !matches!(evidence.qualified, QualifiedSource::Unresolved) {
            return Err(SemanticRefusal::InvalidSourceRevision);
        }
        Ok(Self { ordinal, evidence })
    }

    #[must_use]
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }

    #[must_use]
    pub fn evidence(&self) -> &SourceEvidence {
        &self.evidence
    }

    #[must_use]
    pub fn event_id(&self) -> Option<EventId> {
        match self.evidence.qualified {
            QualifiedSource::Event { event_id, .. } => Some(event_id),
            QualifiedSource::Unresolved | QualifiedSource::Absent => None,
        }
    }

    #[must_use]
    pub fn created_at(&self) -> Option<Timestamp> {
        match self.evidence.qualified {
            QualifiedSource::Event { created_at, .. } => Some(created_at),
            QualifiedSource::Unresolved | QualifiedSource::Absent => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticOperation {
    pub intent_id: IntentId,
    pub program: ReplayProgramId,
    pub format: ReplayFormatId,
    pub source_requirement: OperationSourceRequirement,
    pub accepted_at: Timestamp,
    pub plan: SemanticPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SemanticProgramDigest(pub(crate) [u8; 32]);

impl SemanticProgramDigest {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Reference-only metadata for the sole current canonical body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticGeneration {
    pub materialization: MaterializationRef,
    pub created_at: Timestamp,
    pub members: BTreeSet<IntentId>,
    pub source_revision: SourceRevision,
    pub program_digest: SemanticProgramDigest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticCurrentState {
    pub source_revision: SourceRevision,
    pub program_digest: SemanticProgramDigest,
    pub generation: Option<SemanticGeneration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredSemanticResource {
    pub coordinate: Coordinate,
    pub operations: Vec<SemanticOperation>,
    pub source: Option<StoredEvent>,
    pub source_policy: SemanticSourcePolicy,
    pub current: SemanticCurrentState,
}

/// A complete generation candidate produced outside store locks. Routing and
/// signature state exist only beside real event bytes; a bodyless operation
/// cannot accidentally enter the signer or delivery lanes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializationCandidate {
    pub event: UnsignedEvent,
    pub routing: String,
    pub sig_state: PendingMaterializationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingMaterializationState {
    AwaitingSigner,
    Pending,
}

impl PendingMaterializationState {
    pub(crate) fn intent_sig_state(self) -> IntentSigState {
        match self {
            Self::AwaitingSigner => IntentSigState::AwaitingSigner,
            Self::Pending => IntentSigState::Pending,
        }
    }
}

/// Bodyless or body-ready semantic payload accepted by the ordinary door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticAccept {
    pub coordinate: Coordinate,
    pub program: ReplayProgramId,
    pub format: ReplayFormatId,
    pub expected_source_revision: Option<SourceRevision>,
    pub expected_program_digest: Option<SemanticProgramDigest>,
    pub expected_current_materialization: Option<MaterializationId>,
    pub starting_source: StartingSourceRequirement,
    pub source: SourceEvidence,
    pub source_policy: SemanticSourcePolicy,
    /// The complete verified source named by `source`, including its relay
    /// provenance. Event-qualified acceptance must carry it; absent and
    /// unresolved acceptance must not.
    pub source_event: Option<StoredEvent>,
    pub plan: SemanticPlan,
    pub materialized: Option<MaterializationCandidate>,
    pub contributing_operations: Vec<IntentId>,
    pub resolved_operations: Vec<ResolvedOperation>,
}

/// Candidate produced outside store/reducer locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticRematerialize {
    pub coordinate: Coordinate,
    pub expected_source_revision: SourceRevision,
    pub expected_program_digest: SemanticProgramDigest,
    pub expected_current_materialization: Option<MaterializationId>,
    pub source: SourceEvidence,
    pub evaluated_at: Timestamp,
    pub materialized: Option<MaterializationCandidate>,
    pub contributing_operations: Vec<IntentId>,
    pub resolved_operations: Vec<ResolvedOperation>,
}

/// One verified relay source and the complete successor prepared from it.
/// The store adopts both under the same CAS transaction; the source can never
/// become an intermediate effective canonical value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticSourceInstall {
    pub source: StoredEvent,
    pub successor: SemanticRematerialize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticInstallOutcome {
    Installed {
        current: SemanticCurrentState,
        installed: Box<StoredEvent>,
        predecessor: Option<Box<StoredEvent>>,
    },
    Waiting(SemanticCurrentState),
    Resolved,
    Stale,
    Refused(SemanticRefusal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticRefusal {
    InvalidCoordinate,
    InvalidPlanVersion,
    PlanTooLarge,
    IncompatibleReplayProgram,
    SourceUnresolved,
    InvalidSourceRevision,
    EmptySourceRound,
    IncompatibleSourcePolicy,
    SourceRevisionExhausted,
    NonCanonicalOperationOrder,
    DuplicateOperation(IntentId),
    UnknownOperation(IntentId),
    TooManyContributingOperations,
    ResolutionReasonTooLarge,
    MaterializationDoesNotMatchCoordinate,
    MaterializationMembershipMismatch,
    MaterializationTimestampMismatch,
    MaterializationTimestampOverflow,
    MaterializationExpired,
    MaterializationIdExhausted,
    MaterializationEventIdCollision,
    MaterializationTombstoned,
}

impl std::fmt::Display for SemanticRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SemanticRefusal {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticResourceState {
    pub(crate) coordinate: Coordinate,
    pub(crate) source_revision: SourceRevision,
    pub(crate) operations: Vec<SemanticOperation>,
    pub(crate) source: Option<StoredEvent>,
    pub(crate) source_policy: SemanticSourcePolicy,
    pub(crate) last_materialization_id: Option<MaterializationId>,
    pub(crate) generation: Option<SemanticGeneration>,
}

impl SemanticResourceState {
    pub(crate) fn current(&self) -> SemanticCurrentState {
        SemanticCurrentState {
            source_revision: self.source_revision.clone(),
            program_digest: semantic_program_digest(&self.operations),
            generation: self.generation.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticReceiptUpdate {
    pub(crate) intent_id: IntentId,
    pub(crate) resolution: OperationResolution,
    pub(crate) current: Option<MaterializationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticTransitionPlan {
    pub(crate) previous: Option<SemanticResourceState>,
    pub(crate) next: Option<SemanticResourceState>,
    pub(crate) receipt_updates: Vec<SemanticReceiptUpdate>,
    pub(crate) materialization_high_water: Option<MaterializationId>,
    pub(crate) candidate: Option<MaterializationCandidate>,
    pub(crate) removed_generation: Option<SemanticGeneration>,
}

pub(crate) fn plan_accept(
    previous: Option<SemanticResourceState>,
    materialization_high_water: Option<MaterializationId>,
    intent_id: IntentId,
    accepted_at: Timestamp,
    accept: SemanticAccept,
) -> Result<SemanticTransitionPlan, SemanticRefusal> {
    validate_coordinate(&accept.coordinate)?;
    if accept.starting_source.plan != accept.source.plan
        || accept.starting_source.access != accept.source.access
    {
        return Err(SemanticRefusal::IncompatibleReplayProgram);
    }
    validate_source_event(&accept.source, accept.source_event.as_ref())?;
    if let Some(previous) = &previous {
        validate_resource_state(previous).map_err(|_| SemanticRefusal::InvalidSourceRevision)?;
    }
    if accept.expected_source_revision.as_ref()
        != previous.as_ref().map(|state| &state.source_revision)
        || accept.expected_program_digest
            != previous
                .as_ref()
                .map(|state| semantic_program_digest(&state.operations))
        || accept.expected_current_materialization
            != previous
                .as_ref()
                .and_then(|state| state.generation.as_ref())
                .map(|generation| generation.materialization.materialization_id)
    {
        return Err(SemanticRefusal::InvalidSourceRevision);
    }
    if previous
        .as_ref()
        .is_some_and(|state| state.last_materialization_id != materialization_high_water)
    {
        return Err(SemanticRefusal::InvalidSourceRevision);
    }

    let mut operations = previous
        .as_ref()
        .map(|state| state.operations.clone())
        .unwrap_or_default();
    if let Some(existing) = operations.first() {
        let requirement = source_requirement(existing);
        if existing.program != accept.program
            || existing.format != accept.format
            || requirement.plan != accept.starting_source.plan
            || requirement.access != accept.starting_source.access
        {
            return Err(SemanticRefusal::IncompatibleReplayProgram);
        }
    }
    if previous
        .as_ref()
        .is_some_and(|state| state.source_policy != accept.source_policy)
    {
        return Err(SemanticRefusal::IncompatibleSourcePolicy);
    }

    let source_revision = match previous.as_ref() {
        None => SourceRevision::initial(accept.source.clone()),
        Some(state) => SourceRevision::reconcile(&state.source_revision, accept.source.clone())?,
    };
    let resolved = resolved_operations(accept.resolved_operations)?;
    validate_resolution_partition(
        &operations,
        &accept.contributing_operations,
        &resolved.keys().copied().collect(),
    )?;

    operations.retain(|operation| !resolved.contains_key(&operation.intent_id));
    let source_requirement = if source_qualifies(&accept.starting_source, &accept.source) {
        OperationSourceRequirement::Qualified(accept.starting_source)
    } else {
        OperationSourceRequirement::Awaiting(accept.starting_source)
    };
    operations.push(SemanticOperation {
        intent_id,
        program: accept.program,
        format: accept.format,
        source_requirement,
        accepted_at,
        plan: accept.plan,
    });
    qualify_operations(&mut operations, &accept.source);
    if operations.len() > MAX_CONTRIBUTING_OPERATIONS {
        return Err(SemanticRefusal::TooManyContributingOperations);
    }

    let all_members = operations
        .iter()
        .map(|operation| operation.intent_id)
        .collect::<BTreeSet<_>>();
    let proposed = accept
        .contributing_operations
        .iter()
        .copied()
        .chain(std::iter::once(intent_id))
        .collect::<BTreeSet<_>>();
    let previous_generation = previous.as_ref().and_then(|state| state.generation.clone());
    let current_digest = semantic_program_digest(&operations);
    let (generation, candidate) = match accept.materialized {
        Some(mut candidate) => {
            if candidate
                .event
                .tags
                .expiration()
                .is_some_and(|expires_at| expires_at <= &accepted_at)
            {
                return Err(SemanticRefusal::MaterializationExpired);
            }
            if proposed != all_members {
                return Err(SemanticRefusal::MaterializationMembershipMismatch);
            }
            ensure_all_qualified(&operations)?;
            validate_materialized(&accept.coordinate, &mut candidate.event)?;
            validate_exact_timestamp(
                candidate.event.created_at,
                &source_revision,
                previous_generation.as_ref(),
                &operations,
            )?;
            let materialization_id = next_materialization_id(materialization_high_water)?;
            let materialization = MaterializationRef {
                materialization_id,
                event_id: candidate
                    .event
                    .id
                    .expect("validate_materialized ensures id"),
            };
            (
                Some(SemanticGeneration {
                    materialization,
                    created_at: candidate.event.created_at,
                    members: all_members.clone(),
                    source_revision: source_revision.clone(),
                    program_digest: current_digest,
                }),
                Some(candidate),
            )
        }
        None => {
            let retained = previous_generation.filter(|generation| {
                generation
                    .members
                    .iter()
                    .all(|member| all_members.contains(member))
            });
            (retained, None)
        }
    };

    let current_ref = generation
        .as_ref()
        .map(|generation| generation.materialization);
    let receipt_updates = operations
        .iter()
        .map(|operation| SemanticReceiptUpdate {
            intent_id: operation.intent_id,
            resolution: OperationResolution::Contributing,
            current: generation.as_ref().and_then(|generation| {
                generation
                    .members
                    .contains(&operation.intent_id)
                    .then_some(generation.materialization)
            }),
        })
        .chain(
            resolved
                .into_iter()
                .map(|(intent_id, resolution)| SemanticReceiptUpdate {
                    intent_id,
                    resolution,
                    current: None,
                }),
        )
        .collect();
    let last_materialization_id = current_ref
        .map(|current| current.materialization_id)
        .or(materialization_high_water);
    let removed_generation = previous
        .as_ref()
        .and_then(|state| state.generation.clone())
        .filter(|old| generation.as_ref() != Some(old));
    let retained_source = previous
        .as_ref()
        .and_then(|state| state.source.clone())
        .into_iter()
        .chain(accept.source_event)
        .find(|source| validate_source_event(source_revision.evidence(), Some(source)).is_ok());
    Ok(SemanticTransitionPlan {
        previous,
        next: Some(SemanticResourceState {
            coordinate: accept.coordinate,
            source_revision,
            operations,
            source: retained_source,
            source_policy: accept.source_policy,
            last_materialization_id,
            generation,
        }),
        receipt_updates,
        materialization_high_water: last_materialization_id,
        candidate,
        removed_generation,
    })
}

pub(crate) fn plan_rematerialize(
    previous: SemanticResourceState,
    rematerialize: SemanticRematerialize,
) -> Result<SemanticTransitionPlan, SemanticRefusal> {
    validate_coordinate(&rematerialize.coordinate)?;
    if previous.coordinate != rematerialize.coordinate
        || previous.source_revision != rematerialize.expected_source_revision
        || semantic_program_digest(&previous.operations) != rematerialize.expected_program_digest
        || previous
            .generation
            .as_ref()
            .map(|generation| generation.materialization.materialization_id)
            != rematerialize.expected_current_materialization
    {
        return Err(SemanticRefusal::InvalidSourceRevision);
    }
    if previous.operations.first().is_some_and(|operation| {
        let requirement = source_requirement(operation);
        requirement.plan != rematerialize.source.plan
            || requirement.access != rematerialize.source.access
    }) {
        return Err(SemanticRefusal::IncompatibleReplayProgram);
    }
    let source_revision =
        SourceRevision::reconcile(&previous.source_revision, rematerialize.source.clone())?;
    let resolved = resolved_operations(rematerialize.resolved_operations)?;
    validate_resolution_partition(
        &previous.operations,
        &rematerialize.contributing_operations,
        &resolved.keys().copied().collect(),
    )?;
    let mut operations = previous
        .operations
        .iter()
        .filter(|operation| !resolved.contains_key(&operation.intent_id))
        .cloned()
        .collect::<Vec<_>>();
    qualify_operations(&mut operations, &rematerialize.source);
    let all_members = operations
        .iter()
        .map(|operation| operation.intent_id)
        .collect::<BTreeSet<_>>();
    let proposed = rematerialize
        .contributing_operations
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if !operations.is_empty() && proposed != all_members {
        return Err(SemanticRefusal::MaterializationMembershipMismatch);
    }
    let current_digest = semantic_program_digest(&operations);
    let (generation, candidate) = match (operations.is_empty(), rematerialize.materialized) {
        (true, None) => (None, None),
        (false, None) => {
            let retained = previous.generation.clone().filter(|generation| {
                generation
                    .members
                    .iter()
                    .all(|member| all_members.contains(member))
            });
            (retained, None)
        }
        (false, Some(mut candidate)) => {
            if candidate
                .event
                .tags
                .expiration()
                .is_some_and(|expires_at| expires_at <= &rematerialize.evaluated_at)
            {
                return Err(SemanticRefusal::MaterializationExpired);
            }
            if matches!(rematerialize.source.qualified, QualifiedSource::Unresolved) {
                return Err(SemanticRefusal::SourceUnresolved);
            }
            ensure_all_qualified(&operations)?;
            validate_materialized(&rematerialize.coordinate, &mut candidate.event)?;
            validate_exact_timestamp(
                candidate.event.created_at,
                &source_revision,
                previous.generation.as_ref(),
                &operations,
            )?;
            let materialization_id = next_materialization_id(previous.last_materialization_id)?;
            let materialization = MaterializationRef {
                materialization_id,
                event_id: candidate
                    .event
                    .id
                    .expect("validate_materialized ensures id"),
            };
            (
                Some(SemanticGeneration {
                    materialization,
                    created_at: candidate.event.created_at,
                    members: all_members.clone(),
                    source_revision: source_revision.clone(),
                    program_digest: current_digest,
                }),
                Some(candidate),
            )
        }
        (true, Some(_)) => return Err(SemanticRefusal::MaterializationMembershipMismatch),
    };
    let receipt_updates = operations
        .iter()
        .map(|operation| SemanticReceiptUpdate {
            intent_id: operation.intent_id,
            resolution: OperationResolution::Contributing,
            current: generation.as_ref().and_then(|generation| {
                generation
                    .members
                    .contains(&operation.intent_id)
                    .then_some(generation.materialization)
            }),
        })
        .chain(
            resolved
                .into_iter()
                .map(|(intent_id, resolution)| SemanticReceiptUpdate {
                    intent_id,
                    resolution,
                    current: None,
                }),
        )
        .collect();
    let removed_generation = previous
        .generation
        .clone()
        .filter(|old| generation.as_ref() != Some(old));
    let last_materialization_id = generation
        .as_ref()
        .map(|generation| generation.materialization.materialization_id)
        .or(previous.last_materialization_id);
    let retained_source = previous.source.clone();
    let source_policy = previous.source_policy.clone();
    Ok(SemanticTransitionPlan {
        previous: Some(previous),
        next: (!operations.is_empty()).then_some(SemanticResourceState {
            coordinate: rematerialize.coordinate,
            source_revision,
            operations,
            source: retained_source,
            source_policy,
            last_materialization_id,
            generation,
        }),
        receipt_updates,
        materialization_high_water: last_materialization_id,
        candidate,
        removed_generation,
    })
}

pub(crate) fn plan_source_install(
    previous: SemanticResourceState,
    install: SemanticSourceInstall,
) -> Result<SemanticTransitionPlan, SemanticRefusal> {
    if matches!(
        previous.source_policy,
        SemanticSourcePolicy::Finite(ref round) if round.is_closed()
    ) {
        return Err(SemanticRefusal::InvalidSourceRevision);
    }
    validate_source_event(&install.successor.source, Some(&install.source))?;
    let coordinate = &install.successor.coordinate;
    let source_matches_coordinate = install.source.event.pubkey == coordinate.public_key
        && install.source.event.kind == coordinate.kind
        && (!(30_000..=39_999).contains(&coordinate.kind.as_u16())
            || install.source.event.tags.identifier().unwrap_or("") == coordinate.identifier);
    let source_matches_evidence = matches!(
        install.successor.source.qualified,
        QualifiedSource::Event { event_id, created_at }
            if event_id == install.source.event.id && created_at == install.source.event.created_at
    );
    if !source_matches_coordinate || !source_matches_evidence {
        return Err(SemanticRefusal::InvalidSourceRevision);
    }
    let advances = match previous.source_revision.evidence().qualified {
        QualifiedSource::Event {
            event_id,
            created_at,
        } => {
            install.source.event.created_at > created_at
                || (install.source.event.created_at == created_at
                    && install.source.event.id < event_id)
        }
        QualifiedSource::Absent | QualifiedSource::Unresolved => true,
    };
    if !advances {
        return Err(SemanticRefusal::InvalidSourceRevision);
    }
    let source = install.source;
    let mut plan = plan_rematerialize(previous, install.successor)?;
    if let Some(next) = &mut plan.next {
        next.source = Some(source);
    }
    Ok(plan)
}

pub(crate) fn advance_source_round(
    state: &mut SemanticResourceState,
    fact: SemanticSourceRoundFact,
) -> SemanticSourceRoundOutcome {
    let SemanticSourcePolicy::Finite(round) = &mut state.source_policy else {
        return SemanticSourceRoundOutcome::Stale;
    };
    let (request, settled) = match fact {
        SemanticSourceRoundFact::RequestOpened(request) => (request, None),
        SemanticSourceRoundFact::RequestSettled { request, terminal } => (request, Some(terminal)),
    };
    if request.round != round.id {
        return SemanticSourceRoundOutcome::Stale;
    }
    let Some(current) = round.sources.get_mut(&request.source) else {
        return SemanticSourceRoundOutcome::Stale;
    };
    match (current.clone(), settled) {
        (SemanticSourceMemberState::Pending, None) => {
            *current = SemanticSourceMemberState::Open(request);
            SemanticSourceRoundOutcome::Advanced
        }
        (SemanticSourceMemberState::Open(existing), None) if existing == request => {
            SemanticSourceRoundOutcome::AlreadyApplied
        }
        // An open request is process/transport-generation ownership, not a
        // durable promise that its callback can still arrive. Recovery opens
        // a fresh router request for the unfinished member and atomically
        // replaces the old identity; a later settlement carrying `existing`
        // then falls through to `Stale`.
        (SemanticSourceMemberState::Open(_), None) => {
            *current = SemanticSourceMemberState::Open(request);
            SemanticSourceRoundOutcome::Advanced
        }
        (SemanticSourceMemberState::Open(existing), Some(terminal)) if existing == request => {
            *current = SemanticSourceMemberState::Settled { request, terminal };
            SemanticSourceRoundOutcome::Advanced
        }
        (
            SemanticSourceMemberState::Settled {
                request: existing,
                terminal: existing_terminal,
            },
            Some(terminal),
        ) if existing == request && existing_terminal == terminal => {
            SemanticSourceRoundOutcome::AlreadyApplied
        }
        _ => SemanticSourceRoundOutcome::Stale,
    }
}

fn validate_source_event(
    evidence: &SourceEvidence,
    source: Option<&StoredEvent>,
) -> Result<(), SemanticRefusal> {
    match (evidence.qualified, source) {
        (
            QualifiedSource::Event {
                event_id,
                created_at,
            },
            Some(source),
        ) if source.event.id == event_id
            && source.event.created_at == created_at
            && source.event.verify().is_ok() =>
        {
            Ok(())
        }
        (QualifiedSource::Absent | QualifiedSource::Unresolved, None) => Ok(()),
        _ => Err(SemanticRefusal::InvalidSourceRevision),
    }
}

fn source_requirement(operation: &SemanticOperation) -> &StartingSourceRequirement {
    match &operation.source_requirement {
        OperationSourceRequirement::Awaiting(requirement)
        | OperationSourceRequirement::Qualified(requirement) => requirement,
    }
}

fn source_qualifies(required: &StartingSourceRequirement, evidence: &SourceEvidence) -> bool {
    if required.plan != evidence.plan || required.access != evidence.access {
        return false;
    }
    match (&required.source, evidence.qualified) {
        (StartingSource::Absent, QualifiedSource::Absent) => true,
        (StartingSource::Event(expected), QualifiedSource::Event { event_id, .. }) => {
            *expected == event_id
        }
        _ => false,
    }
}

fn qualify_operations(operations: &mut [SemanticOperation], evidence: &SourceEvidence) {
    for operation in operations {
        if let OperationSourceRequirement::Awaiting(requirement) = &operation.source_requirement {
            if source_qualifies(requirement, evidence) {
                operation.source_requirement =
                    OperationSourceRequirement::Qualified(requirement.clone());
            }
        }
    }
}

fn ensure_all_qualified(operations: &[SemanticOperation]) -> Result<(), SemanticRefusal> {
    if operations.iter().any(|operation| {
        matches!(
            operation.source_requirement,
            OperationSourceRequirement::Awaiting(_)
        )
    }) {
        Err(SemanticRefusal::SourceUnresolved)
    } else {
        Ok(())
    }
}

fn validate_coordinate(coordinate: &Coordinate) -> Result<(), SemanticRefusal> {
    if crate::address_key::address_key_for_coordinate(coordinate).is_none()
        || coordinate.identifier.len() > MAX_COORDINATE_IDENTIFIER_BYTES
    {
        return Err(SemanticRefusal::InvalidCoordinate);
    }
    Ok(())
}

fn validate_materialized(
    coordinate: &Coordinate,
    materialized: &mut UnsignedEvent,
) -> Result<(), SemanticRefusal> {
    validate_coordinate(coordinate)?;
    materialized.ensure_id();
    let matches = materialized.pubkey == coordinate.public_key
        && materialized.kind == coordinate.kind
        && (!(30_000..=39_999).contains(&coordinate.kind.as_u16())
            || materialized.tags.identifier().unwrap_or("") == coordinate.identifier)
        && materialized.verify_id().is_ok();
    matches
        .then_some(())
        .ok_or(SemanticRefusal::MaterializationDoesNotMatchCoordinate)
}

fn validate_exact_timestamp(
    candidate: Timestamp,
    source: &SourceRevision,
    previous: Option<&SemanticGeneration>,
    operations: &[SemanticOperation],
) -> Result<(), SemanticRefusal> {
    let operation_time = operations
        .iter()
        .map(|operation| operation.accepted_at.as_secs())
        .max()
        .unwrap_or(0);
    let source_time = source
        .created_at()
        .map(|created_at| {
            created_at
                .as_secs()
                .checked_add(1)
                .ok_or(SemanticRefusal::MaterializationTimestampOverflow)
        })
        .transpose()?
        .unwrap_or(0);
    let prior_time = previous
        .map(|generation| {
            generation
                .created_at
                .as_secs()
                .checked_add(1)
                .ok_or(SemanticRefusal::MaterializationTimestampOverflow)
        })
        .transpose()?
        .unwrap_or(0);
    let exact = operation_time.max(source_time).max(prior_time);
    if candidate.as_secs() != exact {
        return Err(SemanticRefusal::MaterializationTimestampMismatch);
    }
    Ok(())
}

fn validate_resolution_partition(
    operations: &[SemanticOperation],
    contributing: &[IntentId],
    resolved: &BTreeSet<IntentId>,
) -> Result<(), SemanticRefusal> {
    let existing = operations
        .iter()
        .map(|operation| operation.intent_id)
        .collect::<Vec<_>>();
    let contributing_set = contributing.iter().copied().collect::<BTreeSet<_>>();
    if contributing_set.len() != contributing.len() {
        return Err(SemanticRefusal::NonCanonicalOperationOrder);
    }
    for intent_id in &existing {
        match (
            contributing_set.contains(intent_id),
            resolved.contains(intent_id),
        ) {
            (true, false) | (false, true) => {}
            (true, true) => return Err(SemanticRefusal::DuplicateOperation(*intent_id)),
            (false, false) => return Err(SemanticRefusal::UnknownOperation(*intent_id)),
        }
    }
    if contributing_set
        .iter()
        .chain(resolved.iter())
        .any(|intent_id| !existing.contains(intent_id))
    {
        return Err(SemanticRefusal::UnknownOperation(
            *contributing_set
                .iter()
                .chain(resolved.iter())
                .find(|intent_id| !existing.contains(intent_id))
                .expect("checked"),
        ));
    }
    let ordered = operations
        .iter()
        .filter(|operation| contributing_set.contains(&operation.intent_id))
        .map(|operation| operation.intent_id)
        .collect::<Vec<_>>();
    if ordered != contributing {
        return Err(SemanticRefusal::NonCanonicalOperationOrder);
    }
    Ok(())
}

fn resolved_operations(
    resolved: Vec<ResolvedOperation>,
) -> Result<BTreeMap<IntentId, OperationResolution>, SemanticRefusal> {
    let mut by_id = BTreeMap::new();
    for resolved in resolved {
        if matches!(
            &resolved.resolution,
            OperationResolution::Refused(reason) if reason.len() > MAX_RESOLUTION_REASON_BYTES
        ) {
            return Err(SemanticRefusal::ResolutionReasonTooLarge);
        }
        if matches!(resolved.resolution, OperationResolution::Contributing) {
            return Err(SemanticRefusal::NonCanonicalOperationOrder);
        }
        if by_id
            .insert(resolved.intent_id, resolved.resolution)
            .is_some()
        {
            return Err(SemanticRefusal::DuplicateOperation(resolved.intent_id));
        }
    }
    Ok(by_id)
}

fn next_materialization_id(
    high_water: Option<MaterializationId>,
) -> Result<MaterializationId, SemanticRefusal> {
    Ok(MaterializationId(
        high_water
            .map(|materialization| materialization.0)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(SemanticRefusal::MaterializationIdExhausted)?,
    ))
}

pub(crate) fn semantic_program_digest(operations: &[SemanticOperation]) -> SemanticProgramDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"nmp-semantic-program-v2\0");
    hasher.update(&(operations.len() as u64).to_be_bytes());
    for operation in operations {
        hasher.update(&operation.intent_id.0.to_be_bytes());
        hasher.update(&operation.program.0);
        hasher.update(&operation.format.0);
        let (qualification, requirement) = match &operation.source_requirement {
            OperationSourceRequirement::Awaiting(requirement) => (0u8, requirement),
            OperationSourceRequirement::Qualified(requirement) => (1u8, requirement),
        };
        hasher.update(&[qualification]);
        hasher.update(&requirement.plan.0);
        hasher.update(&requirement.access.0);
        match requirement.source {
            StartingSource::Absent => {
                hasher.update(&[0]);
            }
            StartingSource::Event(event_id) => {
                hasher.update(&[1]);
                hasher.update(event_id.as_bytes());
            }
        }
        hasher.update(&operation.accepted_at.as_secs().to_be_bytes());
        hasher.update(&operation.plan.version().to_be_bytes());
        hasher.update(&(operation.plan.bytes().len() as u64).to_be_bytes());
        hasher.update(operation.plan.bytes());
    }
    SemanticProgramDigest(*hasher.finalize().as_bytes())
}

pub(crate) fn validate_resource_state(
    state: &SemanticResourceState,
) -> Result<(), PersistenceError> {
    let operation_ids = state
        .operations
        .iter()
        .map(|operation| operation.intent_id)
        .collect::<BTreeSet<_>>();
    let source_policy_valid = match &state.source_policy {
        SemanticSourcePolicy::Continuing => true,
        SemanticSourcePolicy::Finite(round) => {
            !round.sources.is_empty()
                && round.sources.iter().all(|(source, member)| match member {
                    SemanticSourceMemberState::Pending => true,
                    SemanticSourceMemberState::Open(request)
                    | SemanticSourceMemberState::Settled { request, .. } => {
                        request.round == round.id && request.source == *source
                    }
                })
        }
    };
    let valid = !state.operations.is_empty()
        && operation_ids.len() == state.operations.len()
        && state
            .operations
            .windows(2)
            .all(|pair| pair[0].intent_id < pair[1].intent_id)
        && state.generation.as_ref().is_none_or(|generation| {
            generation
                .members
                .iter()
                .all(|member| operation_ids.contains(member))
                && state.last_materialization_id
                    == Some(generation.materialization.materialization_id)
        })
        && validate_source_event(state.source_revision.evidence(), state.source.as_ref()).is_ok()
        && source_policy_valid;
    valid
        .then_some(())
        .ok_or_else(|| PersistenceError::invariant("invalid active semantic resource state"))
}

pub(crate) fn recovered(state: SemanticResourceState) -> RecoveredSemanticResource {
    let program_digest = semantic_program_digest(&state.operations);
    RecoveredSemanticResource {
        coordinate: state.coordinate,
        operations: state.operations,
        source: state.source,
        source_policy: state.source_policy,
        current: SemanticCurrentState {
            source_revision: state.source_revision,
            program_digest,
            generation: state.generation,
        },
    }
}
