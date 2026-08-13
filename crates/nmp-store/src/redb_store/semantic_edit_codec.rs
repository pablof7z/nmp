//! Current-epoch binary codecs for opaque replaceable-operation state.

use std::collections::BTreeSet;

use nostr::nips::nip01::Coordinate;
use nostr::{EventId, Timestamp};

use crate::semantic_edit::{
    AccessContextId, MaterializationId, OperationSourceRequirement, QualifiedSource,
    ReplayFormatId, ReplayProgramId, SemanticGeneration, SemanticOperation, SemanticPlan,
    SemanticProgramDigest, SemanticResourceState, SourceEvidence, SourcePlanId, SourceRevision,
    StartingSource, StartingSourceRequirement, MAX_CONTRIBUTING_OPERATIONS,
    MAX_COORDINATE_IDENTIFIER_BYTES, MAX_PROGRAM_BYTES,
};
use crate::{IntentId, MaterializationRef, PersistenceError};

const RESOURCE_MAGIC: &[u8; 4] = b"NMSR";
const OPERATION_MAGIC: &[u8; 4] = b"NMSO";
const VERSION: u8 = 3;
const HEADER: usize = 8;

struct Encoder(Vec<u8>);

impl Encoder {
    fn new(magic: &[u8; 4]) -> Self {
        let mut bytes = Vec::with_capacity(128);
        bytes.extend_from_slice(magic);
        bytes.push(VERSION);
        bytes.extend_from_slice(&[0; 3]);
        Self(bytes)
    }

    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: usize) -> Result<(), PersistenceError> {
        self.0.extend_from_slice(
            &u32::try_from(value)
                .map_err(|_| invariant("semantic length overflow"))?
                .to_be_bytes(),
        );
        Ok(())
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn fixed(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }

    fn bytes(&mut self, value: &[u8], max: usize) -> Result<(), PersistenceError> {
        if value.len() > max {
            return Err(invariant("semantic field exceeds bound"));
        }
        self.u32(value.len())?;
        self.fixed(value);
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.0
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], magic: &[u8; 4]) -> Result<Self, PersistenceError> {
        if bytes.len() < HEADER
            || bytes.get(..4) != Some(magic)
            || bytes[4] != VERSION
            || bytes[5..8] != [0, 0, 0]
        {
            return Err(invariant("semantic codec envelope"));
        }
        Ok(Self {
            bytes,
            cursor: HEADER,
        })
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], PersistenceError> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or_else(|| invariant("semantic length overflow"))?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or_else(|| invariant("semantic codec truncated"))?;
        self.cursor = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, PersistenceError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, PersistenceError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().expect("width")))
    }

    fn u32(&mut self) -> Result<usize, PersistenceError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().expect("width")) as usize)
    }

    fn u64(&mut self) -> Result<u64, PersistenceError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().expect("width")))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], PersistenceError> {
        Ok(self.take(N)?.try_into().expect("width"))
    }

    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, PersistenceError> {
        let len = self.u32()?;
        if len > max {
            return Err(invariant("semantic field exceeds bound"));
        }
        Ok(self.take(len)?.to_vec())
    }

    fn finish(self) -> Result<(), PersistenceError> {
        if self.cursor != self.bytes.len() {
            return Err(invariant("semantic codec trailing bytes"));
        }
        Ok(())
    }
}

fn invariant(message: impl Into<String>) -> PersistenceError {
    PersistenceError::invariant(message)
}

pub(super) fn coordinate_key(coordinate: &Coordinate) -> Result<Vec<u8>, PersistenceError> {
    if crate::address_key::address_key_for_coordinate(coordinate).is_none()
        || coordinate.identifier.len() > MAX_COORDINATE_IDENTIFIER_BYTES
    {
        return Err(invariant("invalid replaceable-operation coordinate"));
    }
    let mut key = Vec::with_capacity(39 + coordinate.identifier.len());
    key.push(1);
    key.extend_from_slice(&coordinate.kind.as_u16().to_be_bytes());
    key.extend_from_slice(coordinate.public_key.as_bytes());
    key.extend_from_slice(&(coordinate.identifier.len() as u32).to_be_bytes());
    key.extend_from_slice(coordinate.identifier.as_bytes());
    Ok(key)
}

pub(super) fn operation_key(
    coordinate: &Coordinate,
    intent_id: IntentId,
) -> Result<Vec<u8>, PersistenceError> {
    let mut key = coordinate_key(coordinate)?;
    key.extend_from_slice(&intent_id.0.to_be_bytes());
    Ok(key)
}

pub(super) fn operation_range(
    coordinate: &Coordinate,
) -> Result<(Vec<u8>, Vec<u8>), PersistenceError> {
    Ok((
        operation_key(coordinate, IntentId(0))?,
        operation_key(coordinate, IntentId(u64::MAX))?,
    ))
}

fn encode_starting_source(encoder: &mut Encoder, source: StartingSource) {
    match source {
        StartingSource::Absent => encoder.u8(0),
        StartingSource::Event(event_id) => {
            encoder.u8(1);
            encoder.fixed(event_id.as_bytes());
        }
    }
}

fn decode_starting_source(decoder: &mut Decoder<'_>) -> Result<StartingSource, PersistenceError> {
    match decoder.u8()? {
        0 => Ok(StartingSource::Absent),
        1 => Ok(StartingSource::Event(
            EventId::from_slice(&decoder.array::<32>()?)
                .map_err(|_| invariant("invalid starting event id"))?,
        )),
        _ => Err(invariant("invalid starting-source tag")),
    }
}

fn encode_evidence(encoder: &mut Encoder, evidence: &SourceEvidence) {
    encoder.fixed(&evidence.plan.0);
    encoder.fixed(&evidence.access.0);
    match evidence.qualified {
        QualifiedSource::Unresolved => encoder.u8(0),
        QualifiedSource::Absent => encoder.u8(1),
        QualifiedSource::Event {
            event_id,
            created_at,
        } => {
            encoder.u8(2);
            encoder.fixed(event_id.as_bytes());
            encoder.u64(created_at.as_secs());
        }
    }
}

fn decode_evidence(decoder: &mut Decoder<'_>) -> Result<SourceEvidence, PersistenceError> {
    let plan = SourcePlanId(decoder.array()?);
    let access = AccessContextId(decoder.array()?);
    let qualified = match decoder.u8()? {
        0 => QualifiedSource::Unresolved,
        1 => QualifiedSource::Absent,
        2 => QualifiedSource::Event {
            event_id: EventId::from_slice(&decoder.array::<32>()?)
                .map_err(|_| invariant("invalid qualified event id"))?,
            created_at: Timestamp::from(decoder.u64()?),
        },
        _ => return Err(invariant("invalid qualified-source tag")),
    };
    Ok(SourceEvidence {
        plan,
        access,
        qualified,
    })
}

fn encode_revision(encoder: &mut Encoder, revision: &SourceRevision) {
    encoder.u64(revision.ordinal());
    encode_evidence(encoder, revision.evidence());
}

fn decode_revision(decoder: &mut Decoder<'_>) -> Result<SourceRevision, PersistenceError> {
    SourceRevision::from_parts(decoder.u64()?, decode_evidence(decoder)?)
        .map_err(|error| invariant(format!("invalid source revision: {error}")))
}

pub(super) fn encode_operation(operation: &SemanticOperation) -> Result<Vec<u8>, PersistenceError> {
    let mut encoder = Encoder::new(OPERATION_MAGIC);
    encoder.u64(operation.intent_id.0);
    encoder.fixed(&operation.program.0);
    encoder.fixed(&operation.format.0);
    let (tag, requirement) = match &operation.source_requirement {
        OperationSourceRequirement::Awaiting(requirement) => (0, requirement),
        OperationSourceRequirement::Qualified(requirement) => (1, requirement),
    };
    encoder.u8(tag);
    encoder.fixed(&requirement.plan.0);
    encoder.fixed(&requirement.access.0);
    encode_starting_source(&mut encoder, requirement.source);
    encoder.u64(operation.accepted_at.as_secs());
    encoder.u16(operation.plan.version());
    encoder.bytes(operation.plan.bytes(), MAX_PROGRAM_BYTES)?;
    Ok(encoder.finish())
}

pub(super) fn decode_operation(bytes: &[u8]) -> Result<SemanticOperation, PersistenceError> {
    let mut decoder = Decoder::new(bytes, OPERATION_MAGIC)?;
    let intent_id = IntentId(decoder.u64()?);
    let program = ReplayProgramId(decoder.array()?);
    let format = ReplayFormatId(decoder.array()?);
    let qualification = decoder.u8()?;
    let requirement = StartingSourceRequirement {
        plan: SourcePlanId(decoder.array()?),
        access: AccessContextId(decoder.array()?),
        source: decode_starting_source(&mut decoder)?,
    };
    let source_requirement = match qualification {
        0 => OperationSourceRequirement::Awaiting(requirement),
        1 => OperationSourceRequirement::Qualified(requirement),
        _ => return Err(invariant("invalid operation qualification tag")),
    };
    let accepted_at = Timestamp::from(decoder.u64()?);
    let plan = SemanticPlan::new(decoder.u16()?, decoder.bytes(MAX_PROGRAM_BYTES)?)
        .map_err(|error| invariant(format!("invalid opaque replay plan: {error}")))?;
    decoder.finish()?;
    Ok(SemanticOperation {
        intent_id,
        program,
        format,
        source_requirement,
        accepted_at,
        plan,
    })
}

pub(super) fn encode_resource(state: &SemanticResourceState) -> Result<Vec<u8>, PersistenceError> {
    let mut encoder = Encoder::new(RESOURCE_MAGIC);
    encode_revision(&mut encoder, &state.source_revision);
    match &state.source {
        None => encoder.u8(0),
        Some(source) => {
            encoder.u8(1);
            let encoded = crate::binary_event::encode(source)
                .map_err(|error| invariant(format!("encode semantic source: {error:?}")))?;
            encoder.bytes(&encoded, usize::MAX)?;
        }
    }
    match state.last_materialization_id {
        None => encoder.u8(0),
        Some(id) => {
            encoder.u8(1);
            encoder.u64(id.0);
        }
    }
    match &state.generation {
        None => encoder.u8(0),
        Some(generation) => {
            encoder.u8(1);
            encoder.u64(generation.materialization.materialization_id.0);
            encoder.fixed(generation.materialization.event_id.as_bytes());
            encoder.u64(generation.created_at.as_secs());
            encode_revision(&mut encoder, &generation.source_revision);
            encoder.fixed(generation.program_digest.as_bytes());
            encoder.u32(generation.members.len())?;
            for member in &generation.members {
                encoder.u64(member.0);
            }
        }
    }
    Ok(encoder.finish())
}

pub(super) fn decode_resource(
    coordinate: Coordinate,
    bytes: &[u8],
) -> Result<SemanticResourceState, PersistenceError> {
    let mut decoder = Decoder::new(bytes, RESOURCE_MAGIC)?;
    let source_revision = decode_revision(&mut decoder)?;
    let source = match decoder.u8()? {
        0 => None,
        1 => Some(
            crate::binary_event::decode(&decoder.bytes(usize::MAX)?)
                .map_err(|error| invariant(format!("decode semantic source: {error:?}")))?,
        ),
        _ => return Err(invariant("invalid optional semantic source tag")),
    };
    let last_materialization_id = match decoder.u8()? {
        0 => None,
        1 => Some(MaterializationId(decoder.u64()?)),
        _ => return Err(invariant("invalid materialization high-water tag")),
    };
    let generation = match decoder.u8()? {
        0 => None,
        1 => {
            let materialization_id = MaterializationId(decoder.u64()?);
            let event_id = EventId::from_slice(&decoder.array::<32>()?)
                .map_err(|_| invariant("invalid materialization event id"))?;
            let created_at = Timestamp::from(decoder.u64()?);
            let source_revision = decode_revision(&mut decoder)?;
            let program_digest = SemanticProgramDigest(decoder.array()?);
            let member_count = decoder.u32()?;
            if member_count > MAX_CONTRIBUTING_OPERATIONS {
                return Err(invariant("too many materialization members"));
            }
            let mut members = BTreeSet::new();
            for _ in 0..member_count {
                if !members.insert(IntentId(decoder.u64()?)) {
                    return Err(invariant("duplicate materialization member"));
                }
            }
            Some(SemanticGeneration {
                materialization: MaterializationRef {
                    materialization_id,
                    event_id,
                },
                created_at,
                members,
                source_revision,
                program_digest,
            })
        }
        _ => return Err(invariant("invalid optional generation tag")),
    };
    decoder.finish()?;
    Ok(SemanticResourceState {
        coordinate,
        source_revision,
        operations: Vec::new(),
        source,
        last_materialization_id,
        generation,
    })
}
