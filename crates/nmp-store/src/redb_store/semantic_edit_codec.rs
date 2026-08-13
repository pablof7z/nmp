//! Exact current-epoch codecs for semantic-edit resources, operations, and receipts.

use std::collections::BTreeSet;

use nostr::{EventId, Kind, PublicKey, Timestamp};

use crate::semantic_edit::{
    MaterializationId, OperationId, OperationResolution, SemanticEditReceipt,
    SemanticGenerationRecord, SemanticMaterializationRecord, SemanticOperation, SemanticPlan,
    SemanticResourceState, SemanticStoreError, SourceRevision, MAX_CONTRIBUTING_OPERATIONS,
    MAX_COORDINATE_IDENTIFIER_BYTES, MAX_MATERIALIZATION_BYTES, MAX_PROGRAM_BYTES,
    MAX_RESOLUTION_REASON_BYTES,
};
use crate::PersistenceError;

const RESOURCE_MAGIC: &[u8; 4] = b"NMSR";
const OPERATION_MAGIC: &[u8; 4] = b"NMSO";
const RECEIPT_MAGIC: &[u8; 4] = b"NMSC";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 8;

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

    fn u32(&mut self, value: usize) -> Result<(), SemanticStoreError> {
        self.0.extend_from_slice(
            &u32::try_from(value)
                .map_err(|_| invariant("semantic codec length overflow"))?
                .to_be_bytes(),
        );
        Ok(())
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8], max: usize) -> Result<(), SemanticStoreError> {
        if value.len() > max {
            return Err(invariant("semantic codec field exceeds bound"));
        }
        self.u32(value.len())?;
        self.0.extend_from_slice(value);
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
    fn new(bytes: &'a [u8], magic: &[u8; 4]) -> Result<Self, SemanticStoreError> {
        if bytes.len() < HEADER_LEN || bytes.get(..4) != Some(magic) {
            return Err(invariant("semantic codec magic/truncation"));
        }
        if bytes[4] != VERSION || bytes[5..8] != [0, 0, 0] {
            return Err(invariant("semantic codec version/reserved bytes"));
        }
        Ok(Self {
            bytes,
            cursor: HEADER_LEN,
        })
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], SemanticStoreError> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or_else(|| invariant("semantic codec length overflow"))?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or_else(|| invariant("semantic codec truncated"))?;
        self.cursor = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, SemanticStoreError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, SemanticStoreError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("length checked"),
        ))
    }

    fn u32(&mut self) -> Result<usize, SemanticStoreError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().expect("length checked")) as usize)
    }

    fn u64(&mut self) -> Result<u64, SemanticStoreError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("length checked"),
        ))
    }

    fn bytes(&mut self, max: usize) -> Result<&'a [u8], SemanticStoreError> {
        let len = self.u32()?;
        if len > max {
            return Err(invariant("semantic codec field exceeds bound"));
        }
        self.take(len)
    }

    fn finish(self) -> Result<(), SemanticStoreError> {
        if self.cursor != self.bytes.len() {
            return Err(invariant("semantic codec trailing bytes"));
        }
        Ok(())
    }
}

fn invariant(message: impl Into<String>) -> SemanticStoreError {
    PersistenceError::invariant(message).into()
}

pub(super) fn coordinate_key(
    coordinate: &nostr::nips::nip01::Coordinate,
) -> Result<Vec<u8>, SemanticStoreError> {
    if crate::address_key::address_key_for_coordinate(coordinate).is_none()
        || coordinate.identifier.len() > MAX_COORDINATE_IDENTIFIER_BYTES
    {
        return Err(invariant("semantic coordinate identifier exceeds bound"));
    }
    let mut key = Vec::with_capacity(39 + coordinate.identifier.len());
    key.push(1);
    key.extend_from_slice(&coordinate.kind.as_u16().to_be_bytes());
    key.extend_from_slice(coordinate.public_key.as_bytes());
    key.extend_from_slice(
        &u32::try_from(coordinate.identifier.len())
            .map_err(|_| invariant("semantic coordinate length overflow"))?
            .to_be_bytes(),
    );
    key.extend_from_slice(coordinate.identifier.as_bytes());
    Ok(key)
}

pub(super) fn decode_coordinate_key(
    bytes: &[u8],
) -> Result<nostr::nips::nip01::Coordinate, SemanticStoreError> {
    if bytes.len() < 39 || bytes[0] != 1 {
        return Err(invariant("invalid semantic coordinate key"));
    }
    let kind = Kind::from(u16::from_be_bytes(
        bytes[1..3].try_into().expect("length checked"),
    ));
    let author = PublicKey::from_slice(&bytes[3..35])
        .map_err(|_| invariant("invalid semantic coordinate author"))?;
    let len = u32::from_be_bytes(bytes[35..39].try_into().expect("length checked")) as usize;
    if len > MAX_COORDINATE_IDENTIFIER_BYTES || bytes.len() != 39 + len {
        return Err(invariant("invalid semantic coordinate identifier length"));
    }
    let identifier = std::str::from_utf8(&bytes[39..])
        .map_err(|_| invariant("semantic coordinate identifier is not UTF-8"))?
        .to_owned();
    let coordinate = nostr::nips::nip01::Coordinate {
        public_key: author,
        kind,
        identifier,
    };
    if crate::address_key::address_key_for_coordinate(&coordinate).is_none() {
        return Err(invariant(
            "semantic coordinate is not replaceable/addressable",
        ));
    }
    Ok(coordinate)
}

pub(super) fn operation_key(
    coordinate: &nostr::nips::nip01::Coordinate,
    operation_id: OperationId,
) -> Result<Vec<u8>, SemanticStoreError> {
    let mut key = coordinate_key(coordinate)?;
    key.extend_from_slice(&operation_id.0.to_be_bytes());
    Ok(key)
}

pub(super) fn operation_range(
    coordinate: &nostr::nips::nip01::Coordinate,
) -> Result<(Vec<u8>, Vec<u8>), SemanticStoreError> {
    Ok((
        operation_key(coordinate, OperationId(0))?,
        operation_key(coordinate, OperationId(u64::MAX))?,
    ))
}

pub(super) fn operation_id_from_key(key: &[u8]) -> Result<OperationId, SemanticStoreError> {
    if key.len() < 8 {
        return Err(invariant("semantic operation key truncated"));
    }
    Ok(OperationId(u64::from_be_bytes(
        key[key.len() - 8..].try_into().expect("length checked"),
    )))
}

fn encode_source(encoder: &mut Encoder, source: &SourceRevision) {
    encoder.u64(source.ordinal());
    match source.identity() {
        None => encoder.u8(0),
        Some((event_id, created_at)) => {
            encoder.u8(1);
            encoder.0.extend_from_slice(event_id.as_bytes());
            encoder.u64(created_at.as_secs());
        }
    }
}

fn decode_source(decoder: &mut Decoder<'_>) -> Result<SourceRevision, SemanticStoreError> {
    let ordinal = decoder.u64()?;
    match decoder.u8()? {
        0 => SourceRevision::from_parts(ordinal, None, None)
            .map_err(|_| invariant("invalid semantic source revision tuple")),
        1 => SourceRevision::from_parts(
            ordinal,
            Some(EventId::from_byte_array(
                decoder.take(32)?.try_into().expect("length checked"),
            )),
            Some(Timestamp::from(decoder.u64()?)),
        )
        .map_err(|_| invariant("invalid semantic source revision tuple")),
        _ => Err(invariant("invalid semantic source tag")),
    }
}

pub(super) fn encode_resource(
    state: &SemanticResourceState,
) -> Result<Vec<u8>, SemanticStoreError> {
    let mut encoder = Encoder::new(RESOURCE_MAGIC);
    encode_source(&mut encoder, &state.source_revision);
    match state.last_materialization_id {
        None => encoder.u8(0),
        Some(materialization_id) => {
            encoder.u8(1);
            encoder.u64(materialization_id.0);
        }
    }
    match &state.generation {
        None => encoder.u8(0),
        Some(generation) => {
            if generation.members.len() > MAX_CONTRIBUTING_OPERATIONS
                || Some(generation.materialization_id) != state.last_materialization_id
            {
                return Err(invariant("invalid semantic generation metadata"));
            }
            encoder.u8(1);
            encoder.u64(generation.materialization_id.0);
            encoder.u32(generation.members.len())?;
            for member in &generation.members {
                encoder.u64(member.0);
            }
            match &generation.body {
                SemanticMaterializationRecord::Pending(json) => {
                    encoder.u8(0);
                    encoder.bytes(json.as_bytes(), MAX_MATERIALIZATION_BYTES)?;
                }
                SemanticMaterializationRecord::Signed(json) => {
                    encoder.u8(1);
                    encoder.bytes(json.as_bytes(), MAX_MATERIALIZATION_BYTES)?;
                }
            }
        }
    }
    Ok(encoder.finish())
}

pub(super) fn decode_resource(
    coordinate: nostr::nips::nip01::Coordinate,
    bytes: &[u8],
) -> Result<SemanticResourceState, SemanticStoreError> {
    let mut decoder = Decoder::new(bytes, RESOURCE_MAGIC)?;
    let source_revision = decode_source(&mut decoder)?;
    let last_materialization_id = match decoder.u8()? {
        0 => None,
        1 => Some(MaterializationId(decoder.u64()?)),
        _ => return Err(invariant("invalid semantic last-materialization tag")),
    };
    let generation = match decoder.u8()? {
        0 => None,
        1 => {
            let materialization_id = MaterializationId(decoder.u64()?);
            if Some(materialization_id) != last_materialization_id {
                return Err(invariant("semantic current generation is not the latest"));
            }
            let member_count = decoder.u32()?;
            if member_count > MAX_CONTRIBUTING_OPERATIONS {
                return Err(invariant("semantic generation member count exceeds bound"));
            }
            let mut members = BTreeSet::new();
            for _ in 0..member_count {
                if !members.insert(OperationId(decoder.u64()?)) {
                    return Err(invariant("duplicate semantic generation member"));
                }
            }
            let body = match decoder.u8()? {
                0 => SemanticMaterializationRecord::Pending(
                    std::str::from_utf8(decoder.bytes(MAX_MATERIALIZATION_BYTES)?)
                        .map_err(|_| invariant("semantic unsigned event is not UTF-8"))?
                        .to_owned(),
                ),
                1 => SemanticMaterializationRecord::Signed(
                    std::str::from_utf8(decoder.bytes(MAX_MATERIALIZATION_BYTES)?)
                        .map_err(|_| invariant("semantic signed event is not UTF-8"))?
                        .to_owned(),
                ),
                _ => return Err(invariant("invalid semantic signature-state tag")),
            };
            Some(SemanticGenerationRecord {
                materialization_id,
                members,
                body,
            })
        }
        _ => return Err(invariant("invalid semantic generation-presence tag")),
    };
    decoder.finish()?;
    if let Some(generation) = &generation {
        let materialized = generation.materialize()?;
        crate::semantic_edit::validate_materialized(&coordinate, &materialized.event.unsigned())
            .map_err(|_| invariant("semantic materialization does not match coordinate"))?;
    }
    Ok(SemanticResourceState {
        coordinate,
        source_revision,
        operations: Vec::new(),
        last_materialization_id,
        generation,
    })
}

pub(super) fn encode_operation(
    operation: &SemanticOperation,
) -> Result<Vec<u8>, SemanticStoreError> {
    let mut encoder = Encoder::new(OPERATION_MAGIC);
    encoder.u64(operation.accepted_at.as_secs());
    encoder.u16(operation.plan.version());
    encoder.bytes(operation.plan.bytes(), MAX_PROGRAM_BYTES)?;
    Ok(encoder.finish())
}

pub(super) fn decode_operation(
    operation_id: OperationId,
    bytes: &[u8],
) -> Result<SemanticOperation, SemanticStoreError> {
    let mut decoder = Decoder::new(bytes, OPERATION_MAGIC)?;
    let accepted_at = Timestamp::from(decoder.u64()?);
    let version = decoder.u16()?;
    let plan = SemanticPlan::new(version, decoder.bytes(MAX_PROGRAM_BYTES)?.to_vec())
        .map_err(|_| invariant("invalid persisted semantic plan"))?;
    decoder.finish()?;
    Ok(SemanticOperation {
        operation_id,
        accepted_at,
        plan,
    })
}

pub(super) fn receipt_key(operation_id: OperationId) -> [u8; 8] {
    operation_id.0.to_be_bytes()
}

pub(super) fn encode_receipt(receipt: &SemanticEditReceipt) -> Result<Vec<u8>, SemanticStoreError> {
    let mut encoder = Encoder::new(RECEIPT_MAGIC);
    encoder.bytes(
        &coordinate_key(&receipt.coordinate)?,
        MAX_COORDINATE_IDENTIFIER_BYTES + 39,
    )?;
    encoder.u64(receipt.accepted_at.as_secs());
    match &receipt.resolution {
        OperationResolution::Contributing => encoder.u8(0),
        OperationResolution::Resolved => encoder.u8(1),
        OperationResolution::Cancelled => encoder.u8(2),
        OperationResolution::Refused(reason) => {
            encoder.u8(3);
            encoder.bytes(reason.as_bytes(), MAX_RESOLUTION_REASON_BYTES)?;
        }
    }
    match receipt.current_materialization {
        None => encoder.u8(0),
        Some(materialization) => {
            encoder.u8(1);
            encoder.u64(materialization.0);
        }
    }
    Ok(encoder.finish())
}

pub(super) fn decode_receipt(
    operation_id: OperationId,
    bytes: &[u8],
) -> Result<SemanticEditReceipt, SemanticStoreError> {
    let mut decoder = Decoder::new(bytes, RECEIPT_MAGIC)?;
    let coordinate = decode_coordinate_key(decoder.bytes(MAX_COORDINATE_IDENTIFIER_BYTES + 39)?)?;
    let accepted_at = Timestamp::from(decoder.u64()?);
    let resolution = match decoder.u8()? {
        0 => OperationResolution::Contributing,
        1 => OperationResolution::Resolved,
        2 => OperationResolution::Cancelled,
        3 => OperationResolution::Refused(
            std::str::from_utf8(decoder.bytes(MAX_RESOLUTION_REASON_BYTES)?)
                .map_err(|_| invariant("semantic refusal reason is not UTF-8"))?
                .to_owned(),
        ),
        _ => return Err(invariant("invalid semantic receipt resolution")),
    };
    let current_materialization = match decoder.u8()? {
        0 => None,
        1 => Some(MaterializationId(decoder.u64()?)),
        _ => return Err(invariant("invalid semantic receipt materialization tag")),
    };
    decoder.finish()?;
    Ok(SemanticEditReceipt {
        operation_id,
        coordinate,
        accepted_at,
        resolution,
        current_materialization,
    })
}
