//! Portable binary codec for publish-queue control records.
//!
//! Every value owns an eight-byte envelope (`magic | version | reserved`) and
//! every integer is big-endian. Lengths are explicit `u32`s with per-field
//! bounds checked before allocation. Relay URLs do not appear in these
//! values: delivery tables refer to the atomic relay dictionary by a stable
//! four-byte surrogate.

use std::collections::BTreeSet;

use nostr::{Event, EventId, PublicKey, RelayUrl, Timestamp};

use crate::binary_event::{self, StoredEventView};
use crate::{
    AuthDenial, AuthDenialSource, HandoffEvidence, IntentId, IntentSigState, PersistenceError,
    PublishQueueAttemptDetails, PublishQueueAttemptHandoff, PublishQueueAttemptOutcome,
    PublishQueueAttemptTransient, PublishQueueDeadlineKind, PublishQueueInFlightPhase,
    PublishQueueLaneState, PublishQueueTerminalOutcome, PublishQueueTransientCause, ReceiptState,
    RefuseReason, StoredEvent,
};

use super::publish_queue::{
    AddrClaimant, PublishQueueIntentRecord, PublishQueueReceiptRecord, SuppressClaimRecord,
};

pub(crate) type PublishQueueRelayId = u32;

pub(super) const PUBLISH_QUEUE_CODEC_VERSION: u64 = 5;
pub(super) const PUBLISH_QUEUE_CODEC_VERSION_KEY: &[u8] = b"codec_version";
pub(super) const NEXT_INTENT_ID_KEY: &[u8] = b"next_intent_id";
pub(super) const NEXT_RECEIPT_ID_KEY: &[u8] = b"next_receipt_id";
pub(super) const NEXT_RELAY_ID_KEY: &[u8] = b"next_relay_id";
pub(crate) const NEXT_TERMINAL_SEQUENCE_KEY: &[u8] = b"next_terminal_sequence";
pub(crate) const TERMINAL_RECEIPT_COUNT_KEY: &[u8] = b"terminal_receipt_count";
pub(crate) const TERMINAL_RECEIPT_BYTES_KEY: &[u8] = b"terminal_receipt_bytes";
pub(crate) const LAST_TERMINAL_AT_KEY: &[u8] = b"last_terminal_at";
pub(crate) const TERMINAL_RECEIPT_PREFIX: &[u8] = b"terminal_receipt/";

pub(super) const MAX_RELAY_BYTES: usize = 4_096;
pub(super) const MAX_TEXT_BYTES: usize = 65_536;
pub(super) const MAX_REASON_BYTES: usize = 4_096;
pub(super) const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_ROUTE_RELAYS: usize = 4_096;
pub(super) const MAX_SUPPRESSION_CLAIMS: usize = 65_536;

pub(crate) fn terminal_receipt_key(sequence: u64, receipt_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(TERMINAL_RECEIPT_PREFIX.len() + 16);
    key.extend_from_slice(TERMINAL_RECEIPT_PREFIX);
    key.extend_from_slice(&sequence.to_be_bytes());
    key.extend_from_slice(&receipt_id.to_be_bytes());
    key
}

pub(crate) fn terminal_receipt_range() -> (Vec<u8>, Vec<u8>) {
    let mut lower = Vec::with_capacity(TERMINAL_RECEIPT_PREFIX.len() + 16);
    lower.extend_from_slice(TERMINAL_RECEIPT_PREFIX);
    lower.extend_from_slice(&[0; 16]);
    let mut upper = Vec::with_capacity(TERMINAL_RECEIPT_PREFIX.len() + 16);
    upper.extend_from_slice(TERMINAL_RECEIPT_PREFIX);
    upper.extend_from_slice(&[u8::MAX; 16]);
    (lower, upper)
}

pub(crate) fn parse_terminal_receipt_key(key: &[u8]) -> Result<(u64, u64), PublishQueueCodecError> {
    let suffix =
        key.strip_prefix(TERMINAL_RECEIPT_PREFIX)
            .ok_or(PublishQueueCodecError::InvalidValue(
                "terminal receipt key prefix",
            ))?;
    if suffix.len() != 16 {
        return Err(PublishQueueCodecError::InvalidValue(
            "terminal receipt key width",
        ));
    }
    let (sequence, receipt_id) = suffix.split_at(8);
    let sequence: [u8; 8] = sequence
        .try_into()
        .map_err(|_| PublishQueueCodecError::InvalidValue("terminal receipt key width"))?;
    let receipt_id: [u8; 8] = receipt_id
        .try_into()
        .map_err(|_| PublishQueueCodecError::InvalidValue("terminal receipt key width"))?;
    Ok((u64::from_be_bytes(sequence), u64::from_be_bytes(receipt_id)))
}

const INTENT_MAGIC: [u8; 4] = *b"NMDI";
const RECEIPT_MAGIC: [u8; 4] = *b"NMDR";
const ROUTE_MAGIC: [u8; 4] = *b"NMDV";
const ATTEMPT_MAGIC: [u8; 4] = *b"NMDA";
const LANE_MAGIC: [u8; 4] = *b"NMDL";
const DEADLINE_MAGIC: [u8; 4] = *b"NMDD";
const DETAIL_MAGIC: [u8; 4] = *b"NMDT";
const CLAIMS_MAGIC: [u8; 4] = *b"NMDC";
const CLAIMANTS_MAGIC: [u8; 4] = *b"NMDQ";
const ADDR_CLAIMANTS_MAGIC: [u8; 4] = *b"NMDX";
const RELAY_MAGIC: [u8; 4] = *b"NMDU";
const VALUE_VERSION: u8 = 1;
const HEADER_LEN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublishQueueCodecError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u8),
    InvalidReserved,
    InvalidTag(&'static str, u8),
    InvalidUtf8,
    InvalidRelay,
    NonCanonicalRelay,
    InvalidPublicKey,
    InvalidEvent,
    InvalidValue(&'static str),
    TrailingBytes,
    LengthOverflow,
    BoundExceeded(&'static str),
}

impl std::fmt::Display for PublishQueueCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<PublishQueueCodecError> for PersistenceError {
    fn from(error: PublishQueueCodecError) -> Self {
        codec_error("record", error)
    }
}

pub(super) fn codec_error(what: &str, error: PublishQueueCodecError) -> PersistenceError {
    PersistenceError::invariant(format!("decode publish queue {what}: {error}"))
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new(magic: [u8; 4]) -> Self {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&magic);
        bytes.push(VALUE_VERSION);
        bytes.extend_from_slice(&[0; 3]);
        Self { bytes }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn fixed(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn bytes(
        &mut self,
        value: &[u8],
        bound: usize,
        name: &'static str,
    ) -> Result<(), PublishQueueCodecError> {
        if value.len() > bound {
            return Err(PublishQueueCodecError::BoundExceeded(name));
        }
        self.u32(u32::try_from(value.len()).map_err(|_| PublishQueueCodecError::LengthOverflow)?);
        self.fixed(value);
        Ok(())
    }

    fn text(
        &mut self,
        value: &str,
        bound: usize,
        name: &'static str,
    ) -> Result<(), PublishQueueCodecError> {
        self.bytes(value.as_bytes(), bound, name)
    }

    fn optional_text(
        &mut self,
        value: Option<&str>,
        bound: usize,
        name: &'static str,
    ) -> Result<(), PublishQueueCodecError> {
        match value {
            None => self.u8(0),
            Some(value) => {
                self.u8(1);
                self.text(value, bound, name)?;
            }
        }
        Ok(())
    }

    fn event(&mut self, event: &Event) -> Result<(), PublishQueueCodecError> {
        let event =
            binary_event::encode_event(event).map_err(|_| PublishQueueCodecError::InvalidEvent)?;
        self.bytes(&event, MAX_EVENT_BYTES, "event")
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], magic: [u8; 4]) -> Result<Self, PublishQueueCodecError> {
        if bytes.len() < HEADER_LEN {
            return Err(PublishQueueCodecError::Truncated);
        }
        if bytes[..4] != magic {
            return Err(PublishQueueCodecError::BadMagic);
        }
        if bytes[4] != VALUE_VERSION {
            return Err(PublishQueueCodecError::UnsupportedVersion(bytes[4]));
        }
        if bytes[5..8] != [0, 0, 0] {
            return Err(PublishQueueCodecError::InvalidReserved);
        }
        Ok(Self {
            bytes,
            cursor: HEADER_LEN,
        })
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], PublishQueueCodecError> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or(PublishQueueCodecError::LengthOverflow)?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or(PublishQueueCodecError::Truncated)?;
        self.cursor = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, PublishQueueCodecError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, PublishQueueCodecError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("length checked"),
        ))
    }

    fn u32(&mut self) -> Result<u32, PublishQueueCodecError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("length checked"),
        ))
    }

    fn u64(&mut self) -> Result<u64, PublishQueueCodecError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("length checked"),
        ))
    }

    fn bytes(
        &mut self,
        bound: usize,
        name: &'static str,
    ) -> Result<&'a [u8], PublishQueueCodecError> {
        let len = self.u32()? as usize;
        if len > bound {
            return Err(PublishQueueCodecError::BoundExceeded(name));
        }
        self.take(len)
    }

    fn text(&mut self, bound: usize, name: &'static str) -> Result<String, PublishQueueCodecError> {
        std::str::from_utf8(self.bytes(bound, name)?)
            .map(str::to_owned)
            .map_err(|_| PublishQueueCodecError::InvalidUtf8)
    }

    fn optional_text(
        &mut self,
        bound: usize,
        name: &'static str,
    ) -> Result<Option<String>, PublishQueueCodecError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.text(bound, name)?)),
            other => Err(PublishQueueCodecError::InvalidTag("optional text", other)),
        }
    }

    fn event(&mut self) -> Result<Event, PublishQueueCodecError> {
        StoredEventView::parse(self.bytes(MAX_EVENT_BYTES, "event")?)
            .and_then(|view| view.materialize_event())
            .map_err(|_| PublishQueueCodecError::InvalidEvent)
    }

    fn public_key(&mut self) -> Result<PublicKey, PublishQueueCodecError> {
        PublicKey::from_slice(self.take(32)?).map_err(|_| PublishQueueCodecError::InvalidPublicKey)
    }

    fn event_id(&mut self) -> Result<EventId, PublishQueueCodecError> {
        Ok(EventId::from_byte_array(
            self.take(32)?.try_into().expect("length checked"),
        ))
    }

    fn finish(self) -> Result<(), PublishQueueCodecError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(PublishQueueCodecError::TrailingBytes)
        }
    }
}

pub(super) fn intent_key(id: IntentId) -> [u8; 8] {
    id.0.to_be_bytes()
}

pub(super) fn parse_intent_key(key: &[u8]) -> Result<IntentId, PublishQueueCodecError> {
    let raw: [u8; 8] = key
        .try_into()
        .map_err(|_| PublishQueueCodecError::InvalidValue("intent key length"))?;
    Ok(IntentId(u64::from_be_bytes(raw)))
}

pub(super) fn receipt_key(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}

pub(super) fn relay_key(id: PublishQueueRelayId) -> [u8; 4] {
    id.to_be_bytes()
}

pub(super) fn lane_key(intent_id: IntentId, relay_id: PublishQueueRelayId) -> [u8; 12] {
    let mut key = [0; 12];
    key[..8].copy_from_slice(&intent_id.0.to_be_bytes());
    key[8..].copy_from_slice(&relay_id.to_be_bytes());
    key
}

pub(super) fn parse_lane_key(
    key: &[u8],
) -> Result<(IntentId, PublishQueueRelayId), PublishQueueCodecError> {
    if key.len() != 12 {
        return Err(PublishQueueCodecError::InvalidValue("lane key length"));
    }
    Ok((
        IntentId(u64::from_be_bytes(
            key[..8].try_into().expect("length checked"),
        )),
        u32::from_be_bytes(key[8..].try_into().expect("length checked")),
    ))
}

pub(super) fn lane_range(intent_id: IntentId) -> ([u8; 12], [u8; 12]) {
    (
        lane_key(intent_id, 0),
        lane_key(intent_id, PublishQueueRelayId::MAX),
    )
}

pub(super) fn attempt_key(
    intent_id: IntentId,
    relay_id: PublishQueueRelayId,
    ordinal: u64,
) -> [u8; 20] {
    let mut key = [0; 20];
    key[..12].copy_from_slice(&lane_key(intent_id, relay_id));
    key[12..].copy_from_slice(&ordinal.to_be_bytes());
    key
}

pub(super) fn parse_attempt_key(
    key: &[u8],
) -> Result<(IntentId, PublishQueueRelayId, u64), PublishQueueCodecError> {
    if key.len() != 20 {
        return Err(PublishQueueCodecError::InvalidValue("attempt key length"));
    }
    let (intent_id, relay_id) = parse_lane_key(&key[..12])?;
    Ok((
        intent_id,
        relay_id,
        u64::from_be_bytes(key[12..].try_into().expect("length checked")),
    ))
}

pub(super) fn attempt_range(intent_id: IntentId) -> ([u8; 20], [u8; 20]) {
    (
        attempt_key(intent_id, 0, 0),
        attempt_key(intent_id, PublishQueueRelayId::MAX, u64::MAX),
    )
}

pub(super) fn route_revision_key(intent_id: IntentId, ordinal: u64) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&intent_id.0.to_be_bytes());
    key[8..].copy_from_slice(&ordinal.to_be_bytes());
    key
}

pub(super) fn parse_route_revision_key(
    key: &[u8],
) -> Result<(IntentId, u64), PublishQueueCodecError> {
    if key.len() != 16 {
        return Err(PublishQueueCodecError::InvalidValue(
            "route revision key length",
        ));
    }
    Ok((
        IntentId(u64::from_be_bytes(
            key[..8].try_into().expect("length checked"),
        )),
        u64::from_be_bytes(key[8..].try_into().expect("length checked")),
    ))
}

pub(super) fn route_revision_range(intent_id: IntentId) -> ([u8; 16], [u8; 16]) {
    (
        route_revision_key(intent_id, 0),
        route_revision_key(intent_id, u64::MAX),
    )
}

pub(super) fn deadline_key(
    at: Timestamp,
    intent_id: IntentId,
    relay_id: PublishQueueRelayId,
) -> [u8; 20] {
    let mut key = [0; 20];
    key[..8].copy_from_slice(&at.as_secs().to_be_bytes());
    key[8..16].copy_from_slice(&intent_id.0.to_be_bytes());
    key[16..].copy_from_slice(&relay_id.to_be_bytes());
    key
}

pub(super) fn deadline_by_intent_key(
    intent_id: IntentId,
    at: Timestamp,
    relay_id: PublishQueueRelayId,
) -> [u8; 20] {
    let mut key = [0; 20];
    key[..8].copy_from_slice(&intent_id.0.to_be_bytes());
    key[8..16].copy_from_slice(&at.as_secs().to_be_bytes());
    key[16..].copy_from_slice(&relay_id.to_be_bytes());
    key
}

pub(super) fn parse_deadline_key(
    key: &[u8],
) -> Result<(Timestamp, IntentId, PublishQueueRelayId), PublishQueueCodecError> {
    if key.len() != 20 {
        return Err(PublishQueueCodecError::InvalidValue("deadline key length"));
    }
    Ok((
        Timestamp::from(u64::from_be_bytes(
            key[..8].try_into().expect("length checked"),
        )),
        IntentId(u64::from_be_bytes(
            key[8..16].try_into().expect("length checked"),
        )),
        u32::from_be_bytes(key[16..].try_into().expect("length checked")),
    ))
}

pub(super) fn parse_deadline_by_intent_key(
    key: &[u8],
) -> Result<(IntentId, Timestamp, PublishQueueRelayId), PublishQueueCodecError> {
    if key.len() != 20 {
        return Err(PublishQueueCodecError::InvalidValue(
            "deadline-by-intent key length",
        ));
    }
    Ok((
        IntentId(u64::from_be_bytes(
            key[..8].try_into().expect("length checked"),
        )),
        Timestamp::from(u64::from_be_bytes(
            key[8..16].try_into().expect("length checked"),
        )),
        u32::from_be_bytes(key[16..].try_into().expect("length checked")),
    ))
}

pub(super) fn deadline_due_range(now: Timestamp) -> ([u8; 20], [u8; 20]) {
    (
        deadline_key(Timestamp::from(0), IntentId(0), 0),
        deadline_key(now, IntentId(u64::MAX), PublishQueueRelayId::MAX),
    )
}

pub(super) fn deadline_intent_range(intent_id: IntentId) -> ([u8; 20], [u8; 20]) {
    (
        deadline_by_intent_key(intent_id, Timestamp::from(0), 0),
        deadline_by_intent_key(
            intent_id,
            Timestamp::from(u64::MAX),
            PublishQueueRelayId::MAX,
        ),
    )
}

pub(super) fn id_claim_key(id: &EventId, author: &PublicKey) -> [u8; 64] {
    let mut key = [0; 64];
    key[..32].copy_from_slice(id.as_bytes());
    key[32..].copy_from_slice(author.as_bytes());
    key
}

pub(super) fn encode_meta_u64(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

pub(super) fn decode_meta_u64(
    bytes: &[u8],
    what: &'static str,
) -> Result<u64, PublishQueueCodecError> {
    let raw: [u8; 8] = bytes
        .try_into()
        .map_err(|_| PublishQueueCodecError::InvalidValue(what))?;
    Ok(u64::from_be_bytes(raw))
}

pub(super) fn encode_relay(relay: &RelayUrl) -> Result<Vec<u8>, PublishQueueCodecError> {
    let mut encoder = Encoder::new(RELAY_MAGIC);
    encoder.text(relay.as_str(), MAX_RELAY_BYTES, "relay URL")?;
    Ok(encoder.finish())
}

pub(super) fn decode_relay(bytes: &[u8]) -> Result<RelayUrl, PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, RELAY_MAGIC)?;
    let encoded = decoder.text(MAX_RELAY_BYTES, "relay URL")?;
    decoder.finish()?;
    let relay = RelayUrl::parse(&encoded).map_err(|_| PublishQueueCodecError::InvalidRelay)?;
    if relay.as_str() != encoded {
        return Err(PublishQueueCodecError::NonCanonicalRelay);
    }
    Ok(relay)
}

fn encode_sig_state(encoder: &mut Encoder, value: IntentSigState) {
    encoder.u8(match value {
        IntentSigState::AwaitingSigner => 0,
        IntentSigState::Pending => 1,
        IntentSigState::Signed => 2,
    });
}

fn decode_sig_state(decoder: &mut Decoder<'_>) -> Result<IntentSigState, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(IntentSigState::AwaitingSigner),
        1 => Ok(IntentSigState::Pending),
        2 => Ok(IntentSigState::Signed),
        other => Err(PublishQueueCodecError::InvalidTag(
            "intent signature state",
            other,
        )),
    }
}

pub(super) fn encode_intent(
    record: &PublishQueueIntentRecord,
) -> Result<Vec<u8>, PublishQueueCodecError> {
    let mut encoder = Encoder::new(INTENT_MAGIC);
    encoder.u64(record.receipt_id);
    match &record.work {
        super::publish_queue::PublishQueueIntentRecordWork::Event {
            frozen,
            routing,
            sig_state,
        } => {
            encoder.u8(0);
            encoder.event(frozen)?;
            encoder.text(routing, MAX_TEXT_BYTES, "routing strategy")?;
            encode_sig_state(&mut encoder, *sig_state);
        }
        super::publish_queue::PublishQueueIntentRecordWork::ReplaceableOperation {
            coordinate,
            materialization,
        } => {
            encoder.u8(1);
            encode_coordinate(&mut encoder, coordinate)?;
            match materialization {
                None => encoder.u8(0),
                Some(materialization) => {
                    encoder.u8(1);
                    encoder.u64(materialization.current.materialization_id.0);
                    encoder.fixed(materialization.current.event_id.as_bytes());
                    encoder.text(
                        &materialization.routing,
                        MAX_TEXT_BYTES,
                        "materialization routing strategy",
                    )?;
                    encode_sig_state(&mut encoder, materialization.sig_state);
                }
            }
        }
    }
    encoder.fixed(record.expected_pubkey.as_bytes());
    encoder.text(
        &record.signing_identity_ref,
        MAX_TEXT_BYTES,
        "signing identity reference",
    )?;
    encoder.u64(record.accepted_at.as_secs());
    Ok(encoder.finish())
}

pub(super) fn decode_intent(
    bytes: &[u8],
) -> Result<PublishQueueIntentRecord, PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, INTENT_MAGIC)?;
    let receipt_id = decoder.u64()?;
    let work = match decoder.u8()? {
        0 => {
            let frozen = decoder.event()?;
            let routing = decoder.text(MAX_TEXT_BYTES, "routing strategy")?;
            let sig_state = decode_sig_state(&mut decoder)?;
            super::publish_queue::PublishQueueIntentRecordWork::Event {
                frozen,
                routing,
                sig_state,
            }
        }
        1 => {
            let coordinate = decode_coordinate(&mut decoder)?;
            let materialization = match decoder.u8()? {
                0 => None,
                1 => Some(super::publish_queue::PublishQueueMaterializationRecord {
                    current: crate::MaterializationRef {
                        materialization_id: crate::MaterializationId(decoder.u64()?),
                        event_id: decoder.event_id()?,
                    },
                    routing: decoder.text(MAX_TEXT_BYTES, "materialization routing strategy")?,
                    sig_state: decode_sig_state(&mut decoder)?,
                }),
                other => {
                    return Err(PublishQueueCodecError::InvalidTag(
                        "optional materialization",
                        other,
                    ))
                }
            };
            super::publish_queue::PublishQueueIntentRecordWork::ReplaceableOperation {
                coordinate,
                materialization,
            }
        }
        other => return Err(PublishQueueCodecError::InvalidTag("intent work", other)),
    };
    let expected_pubkey = decoder.public_key()?;
    let signing_identity_ref = decoder.text(MAX_TEXT_BYTES, "signing identity reference")?;
    let accepted_at = Timestamp::from(decoder.u64()?);
    decoder.finish()?;
    Ok(PublishQueueIntentRecord {
        receipt_id,
        work,
        expected_pubkey,
        signing_identity_ref,
        accepted_at,
    })
}

fn encode_coordinate(
    encoder: &mut Encoder,
    coordinate: &nostr::nips::nip01::Coordinate,
) -> Result<(), PublishQueueCodecError> {
    if crate::address_key::address_key_for_coordinate(coordinate).is_none() {
        return Err(PublishQueueCodecError::InvalidValue(
            "operation coordinate is not replaceable/addressable",
        ));
    }
    encoder.u16(coordinate.kind.as_u16());
    encoder.fixed(coordinate.public_key.as_bytes());
    encoder.text(
        &coordinate.identifier,
        crate::semantic_edit::MAX_COORDINATE_IDENTIFIER_BYTES,
        "operation coordinate identifier",
    )
}

fn decode_coordinate(
    decoder: &mut Decoder<'_>,
) -> Result<nostr::nips::nip01::Coordinate, PublishQueueCodecError> {
    let coordinate = nostr::nips::nip01::Coordinate {
        kind: nostr::Kind::from(decoder.u16()?),
        public_key: decoder.public_key()?,
        identifier: decoder.text(
            crate::semantic_edit::MAX_COORDINATE_IDENTIFIER_BYTES,
            "operation coordinate identifier",
        )?,
    };
    if crate::address_key::address_key_for_coordinate(&coordinate).is_none() {
        return Err(PublishQueueCodecError::InvalidValue(
            "operation coordinate is not replaceable/addressable",
        ));
    }
    Ok(coordinate)
}

fn encode_optional_event_id(encoder: &mut Encoder, value: Option<EventId>) {
    match value {
        None => encoder.u8(0),
        Some(id) => {
            encoder.u8(1);
            encoder.fixed(id.as_bytes());
        }
    }
}

fn decode_optional_event_id(
    decoder: &mut Decoder<'_>,
) -> Result<Option<EventId>, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decoder.event_id()?)),
        other => Err(PublishQueueCodecError::InvalidTag(
            "optional event id",
            other,
        )),
    }
}

fn encode_refuse_reason(encoder: &mut Encoder, reason: RefuseReason) {
    match reason {
        RefuseReason::AlreadyExpired => encoder.u8(0),
        RefuseReason::Tombstoned => encoder.u8(1),
        RefuseReason::ReplaceableBaseOnRegularEvent => encoder.u8(2),
        // The two ids are the whole point of retaining this reason: they let
        // an app fetch what is actually at the coordinate, reapply the
        // user's change and resubmit without troubling them.
        RefuseReason::ReplaceableBaseChanged { expected, actual } => {
            encoder.u8(3);
            encode_optional_event_id(encoder, expected);
            encode_optional_event_id(encoder, actual);
        }
    }
}

fn decode_refuse_reason(decoder: &mut Decoder<'_>) -> Result<RefuseReason, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(RefuseReason::AlreadyExpired),
        1 => Ok(RefuseReason::Tombstoned),
        2 => Ok(RefuseReason::ReplaceableBaseOnRegularEvent),
        3 => {
            let expected = decode_optional_event_id(decoder)?;
            let actual = decode_optional_event_id(decoder)?;
            Ok(RefuseReason::ReplaceableBaseChanged { expected, actual })
        }
        other => Err(PublishQueueCodecError::InvalidTag("refuse reason", other)),
    }
}

fn encode_receipt_state(encoder: &mut Encoder, state: ReceiptState) {
    match state {
        ReceiptState::Accepted => encoder.u8(0),
        ReceiptState::Signed => encoder.u8(1),
        ReceiptState::Compensated => encoder.u8(2),
        ReceiptState::Cancelled => encoder.u8(3),
        ReceiptState::Superseded => encoder.u8(4),
        ReceiptState::NoDestination => encoder.u8(6),
        ReceiptState::Refused(reason) => {
            encoder.u8(5);
            encode_refuse_reason(encoder, reason);
        }
    }
}

fn decode_receipt_state(decoder: &mut Decoder<'_>) -> Result<ReceiptState, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(ReceiptState::Accepted),
        1 => Ok(ReceiptState::Signed),
        2 => Ok(ReceiptState::Compensated),
        3 => Ok(ReceiptState::Cancelled),
        4 => Ok(ReceiptState::Superseded),
        6 => Ok(ReceiptState::NoDestination),
        5 => Ok(ReceiptState::Refused(decode_refuse_reason(decoder)?)),
        other => Err(PublishQueueCodecError::InvalidTag("receipt state", other)),
    }
}

pub(crate) fn encode_receipt(record: &PublishQueueReceiptRecord) -> Vec<u8> {
    let mut encoder = Encoder::new(RECEIPT_MAGIC);
    match record.intent_id {
        None => encoder.u8(0),
        Some(intent_id) => {
            encoder.u8(1);
            encoder.u64(intent_id.0);
        }
    }
    encoder.fixed(record.expected_pubkey.as_bytes());
    match record.accepted_at {
        None => encoder.u8(0),
        Some(accepted_at) => {
            encoder.u8(1);
            encoder.u64(accepted_at.as_secs());
        }
    }
    match &record.payload {
        crate::PublishQueueReceiptPayload::Event { event_id, state } => {
            encoder.u8(0);
            encoder.fixed(event_id.as_bytes());
            encode_receipt_state(&mut encoder, *state);
        }
        crate::PublishQueueReceiptPayload::ReplaceableOperation {
            coordinate,
            acceptance,
            state,
        } => {
            encoder.u8(1);
            encode_coordinate(&mut encoder, coordinate)
                .expect("validated operation receipt coordinate");
            match acceptance {
                crate::ReplaceableOperationAcceptance::Bodyless => encoder.u8(0),
                crate::ReplaceableOperationAcceptance::BodyComplete(event_id) => {
                    encoder.u8(1);
                    encoder.fixed(event_id.as_bytes());
                }
            }
            match state {
                crate::ReplaceableOperationReceiptState::Contributing { current } => {
                    encoder.u8(0);
                    match current {
                        None => encoder.u8(0),
                        Some(current) => {
                            encoder.u8(1);
                            encoder.u64(current.materialization.materialization_id.0);
                            encoder.fixed(current.materialization.event_id.as_bytes());
                            encode_sig_state(&mut encoder, current.sig_state);
                        }
                    }
                }
                crate::ReplaceableOperationReceiptState::Resolved => encoder.u8(1),
                crate::ReplaceableOperationReceiptState::Cancelled => encoder.u8(2),
                crate::ReplaceableOperationReceiptState::Refused(reason) => {
                    encoder.u8(3);
                    encoder
                        .text(reason, MAX_TEXT_BYTES, "operation refusal")
                        .expect("bounded operation refusal");
                }
            }
        }
    }
    encoder
        .optional_text(
            record.correlation.as_deref(),
            MAX_TEXT_BYTES,
            "correlation token",
        )
        .expect("validated correlation token");
    match (
        record.terminal_sequence,
        record.terminal_at,
        record.terminal_bytes,
    ) {
        (None, None, None) => encoder.u8(0),
        (Some(sequence), Some(terminal_at), Some(terminal_bytes)) => {
            encoder.u8(1);
            encoder.u64(sequence);
            encoder.u64(terminal_at.as_secs());
            encoder.u64(terminal_bytes);
        }
        _ => unreachable!("terminal receipt metadata is all-or-none"),
    }
    encoder.finish()
}

pub(crate) fn decode_receipt(
    bytes: &[u8],
) -> Result<PublishQueueReceiptRecord, PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, RECEIPT_MAGIC)?;
    let intent_id = match decoder.u8()? {
        0 => None,
        1 => Some(IntentId(decoder.u64()?)),
        other => return Err(PublishQueueCodecError::InvalidTag("receipt intent", other)),
    };
    let expected_pubkey = decoder.public_key()?;
    let accepted_at = match decoder.u8()? {
        0 => None,
        1 => Some(Timestamp::from(decoder.u64()?)),
        other => {
            return Err(PublishQueueCodecError::InvalidTag(
                "receipt acceptance",
                other,
            ))
        }
    };
    let payload = match decoder.u8()? {
        0 => crate::PublishQueueReceiptPayload::Event {
            event_id: decoder.event_id()?,
            state: decode_receipt_state(&mut decoder)?,
        },
        1 => {
            let coordinate = decode_coordinate(&mut decoder)?;
            let acceptance = match decoder.u8()? {
                0 => crate::ReplaceableOperationAcceptance::Bodyless,
                1 => crate::ReplaceableOperationAcceptance::BodyComplete(decoder.event_id()?),
                other => {
                    return Err(PublishQueueCodecError::InvalidTag(
                        "operation acceptance",
                        other,
                    ))
                }
            };
            let state = match decoder.u8()? {
                0 => {
                    let current = match decoder.u8()? {
                        0 => None,
                        1 => Some(crate::MaterializationReceipt {
                            materialization: crate::MaterializationRef {
                                materialization_id: crate::MaterializationId(decoder.u64()?),
                                event_id: decoder.event_id()?,
                            },
                            sig_state: decode_sig_state(&mut decoder)?,
                        }),
                        other => {
                            return Err(PublishQueueCodecError::InvalidTag(
                                "optional receipt materialization",
                                other,
                            ))
                        }
                    };
                    crate::ReplaceableOperationReceiptState::Contributing { current }
                }
                1 => crate::ReplaceableOperationReceiptState::Resolved,
                2 => crate::ReplaceableOperationReceiptState::Cancelled,
                3 => crate::ReplaceableOperationReceiptState::Refused(
                    decoder.text(MAX_TEXT_BYTES, "operation refusal")?,
                ),
                other => {
                    return Err(PublishQueueCodecError::InvalidTag(
                        "operation receipt state",
                        other,
                    ))
                }
            };
            crate::PublishQueueReceiptPayload::ReplaceableOperation {
                coordinate,
                acceptance,
                state,
            }
        }
        other => return Err(PublishQueueCodecError::InvalidTag("receipt payload", other)),
    };
    let correlation = decoder.optional_text(MAX_TEXT_BYTES, "correlation token")?;
    let (terminal_sequence, terminal_at, terminal_bytes) = match decoder.u8()? {
        0 => (None, None, None),
        1 => (
            Some(decoder.u64()?),
            Some(Timestamp::from(decoder.u64()?)),
            Some(decoder.u64()?),
        ),
        other => {
            return Err(PublishQueueCodecError::InvalidTag(
                "terminal receipt metadata",
                other,
            ))
        }
    };
    decoder.finish()?;
    Ok(PublishQueueReceiptRecord {
        intent_id,
        expected_pubkey,
        accepted_at,
        payload,
        correlation,
        terminal_sequence,
        terminal_at,
        terminal_bytes,
    })
}

pub(crate) fn encode_route(
    relays: &[PublishQueueRelayId],
) -> Result<Vec<u8>, PublishQueueCodecError> {
    if relays.len() > MAX_ROUTE_RELAYS {
        return Err(PublishQueueCodecError::BoundExceeded("route relay count"));
    }
    if relays.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(PublishQueueCodecError::InvalidValue(
            "route relay ids must be strictly ordered",
        ));
    }
    let mut encoder = Encoder::new(ROUTE_MAGIC);
    encoder.u32(relays.len() as u32);
    for relay in relays {
        encoder.u32(*relay);
    }
    Ok(encoder.finish())
}

pub(super) fn decode_route(
    bytes: &[u8],
) -> Result<Vec<PublishQueueRelayId>, PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, ROUTE_MAGIC)?;
    let count = decoder.u32()? as usize;
    if count > MAX_ROUTE_RELAYS {
        return Err(PublishQueueCodecError::BoundExceeded("route relay count"));
    }
    let mut relays = Vec::with_capacity(count);
    for _ in 0..count {
        relays.push(decoder.u32()?);
    }
    decoder.finish()?;
    if relays.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(PublishQueueCodecError::InvalidValue(
            "route relay ids must be strictly ordered",
        ));
    }
    Ok(relays)
}

fn encode_attempt_outcome(
    encoder: &mut Encoder,
    outcome: &PublishQueueAttemptOutcome,
) -> Result<(), PublishQueueCodecError> {
    match outcome {
        PublishQueueAttemptOutcome::Started => encoder.u8(0),
        PublishQueueAttemptOutcome::Acked => encoder.u8(1),
        PublishQueueAttemptOutcome::Rejected(reason) => {
            encoder.u8(2);
            encoder.text(reason, MAX_REASON_BYTES, "rejection reason")?;
        }
        PublishQueueAttemptOutcome::GaveUp => encoder.u8(3),
    }
    Ok(())
}

fn decode_attempt_outcome(
    decoder: &mut Decoder<'_>,
) -> Result<PublishQueueAttemptOutcome, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(PublishQueueAttemptOutcome::Started),
        1 => Ok(PublishQueueAttemptOutcome::Acked),
        2 => Ok(PublishQueueAttemptOutcome::Rejected(
            decoder.text(MAX_REASON_BYTES, "rejection reason")?,
        )),
        3 => Ok(PublishQueueAttemptOutcome::GaveUp),
        other => Err(PublishQueueCodecError::InvalidTag("attempt outcome", other)),
    }
}

pub(crate) fn encode_attempt(
    event: &Event,
    outcome: &PublishQueueAttemptOutcome,
) -> Result<Vec<u8>, PublishQueueCodecError> {
    let mut encoder = Encoder::new(ATTEMPT_MAGIC);
    encoder.event(event)?;
    encode_attempt_outcome(&mut encoder, outcome)?;
    Ok(encoder.finish())
}

pub(super) fn decode_attempt(
    bytes: &[u8],
) -> Result<(Event, PublishQueueAttemptOutcome), PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, ATTEMPT_MAGIC)?;
    let event = decoder.event()?;
    let outcome = decode_attempt_outcome(&mut decoder)?;
    decoder.finish()?;
    event
        .verify()
        .map_err(|_| PublishQueueCodecError::InvalidEvent)?;
    Ok((event, outcome))
}

fn encode_auth_source(encoder: &mut Encoder, source: AuthDenialSource) {
    encoder.u8(match source {
        AuthDenialSource::Policy => 0,
        AuthDenialSource::Signer => 1,
        AuthDenialSource::Relay => 2,
    });
}

fn decode_auth_source(
    decoder: &mut Decoder<'_>,
) -> Result<AuthDenialSource, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(AuthDenialSource::Policy),
        1 => Ok(AuthDenialSource::Signer),
        2 => Ok(AuthDenialSource::Relay),
        other => Err(PublishQueueCodecError::InvalidTag(
            "auth denial source",
            other,
        )),
    }
}

fn encode_transient_cause(encoder: &mut Encoder, cause: PublishQueueTransientCause) {
    encoder.u8(match cause {
        PublishQueueTransientCause::Interrupted => 0,
        PublishQueueTransientCause::AckTimeout => 1,
        PublishQueueTransientCause::ConnectionLost => 2,
        PublishQueueTransientCause::RelayRateLimited => 3,
        PublishQueueTransientCause::RelayError => 4,
        PublishQueueTransientCause::AuthRequired => 5,
    });
}

fn decode_transient_cause(
    decoder: &mut Decoder<'_>,
) -> Result<PublishQueueTransientCause, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(PublishQueueTransientCause::Interrupted),
        1 => Ok(PublishQueueTransientCause::AckTimeout),
        2 => Ok(PublishQueueTransientCause::ConnectionLost),
        3 => Ok(PublishQueueTransientCause::RelayRateLimited),
        4 => Ok(PublishQueueTransientCause::RelayError),
        5 => Ok(PublishQueueTransientCause::AuthRequired),
        other => Err(PublishQueueCodecError::InvalidTag("transient cause", other)),
    }
}

fn encode_terminal(
    encoder: &mut Encoder,
    outcome: &PublishQueueTerminalOutcome,
) -> Result<(), PublishQueueCodecError> {
    match outcome {
        PublishQueueTerminalOutcome::Acked => encoder.u8(0),
        PublishQueueTerminalOutcome::Rejected(reason) => {
            encoder.u8(1);
            encoder.text(reason, MAX_REASON_BYTES, "terminal rejection reason")?;
        }
        PublishQueueTerminalOutcome::GaveUp => encoder.u8(2),
        PublishQueueTerminalOutcome::AuthDenied(denial) => {
            encoder.u8(4);
            encode_auth_source(encoder, denial.source);
            encoder.text(
                &denial.reason,
                MAX_REASON_BYTES,
                "authentication denial reason",
            )?;
        }
    }
    Ok(())
}

fn decode_terminal(
    decoder: &mut Decoder<'_>,
) -> Result<PublishQueueTerminalOutcome, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(PublishQueueTerminalOutcome::Acked),
        1 => Ok(PublishQueueTerminalOutcome::Rejected(
            decoder.text(MAX_REASON_BYTES, "terminal rejection reason")?,
        )),
        2 => Ok(PublishQueueTerminalOutcome::GaveUp),
        4 => Ok(PublishQueueTerminalOutcome::AuthDenied(AuthDenial {
            source: decode_auth_source(decoder)?,
            reason: decoder.text(MAX_REASON_BYTES, "authentication denial reason")?,
        })),
        other => Err(PublishQueueCodecError::InvalidTag(
            "lane terminal outcome",
            other,
        )),
    }
}

fn encode_lane_state(
    encoder: &mut Encoder,
    state: &PublishQueueLaneState,
) -> Result<(), PublishQueueCodecError> {
    match state {
        PublishQueueLaneState::WaitingConnection => encoder.u8(0),
        PublishQueueLaneState::WaitingAuth => encoder.u8(1),
        PublishQueueLaneState::Eligible { since } => {
            encoder.u8(2);
            encoder.u64(since.as_secs());
        }
        PublishQueueLaneState::InFlight { ordinal, phase } => {
            encoder.u8(3);
            encoder.u64(*ordinal);
            match phase {
                PublishQueueInFlightPhase::AwaitingHandoff => encoder.u8(0),
                PublishQueueInFlightPhase::AwaitingAck { deadline } => {
                    encoder.u8(1);
                    encoder.u64(deadline.as_secs());
                }
            }
        }
        PublishQueueLaneState::Transient {
            ordinal,
            eligible_at,
            cause,
            raw_reason,
        } => {
            encoder.u8(4);
            encoder.u64(*ordinal);
            encoder.u64(eligible_at.as_secs());
            encode_transient_cause(encoder, *cause);
            encoder.optional_text(
                raw_reason.as_deref(),
                MAX_REASON_BYTES,
                "transient raw reason",
            )?;
        }
        PublishQueueLaneState::Terminal { ordinal, outcome } => {
            encoder.u8(5);
            encoder.u64(*ordinal);
            encode_terminal(encoder, outcome)?;
        }
    }
    Ok(())
}

fn decode_lane_state(
    decoder: &mut Decoder<'_>,
) -> Result<PublishQueueLaneState, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(PublishQueueLaneState::WaitingConnection),
        1 => Ok(PublishQueueLaneState::WaitingAuth),
        2 => Ok(PublishQueueLaneState::Eligible {
            since: Timestamp::from(decoder.u64()?),
        }),
        3 => {
            let ordinal = decoder.u64()?;
            let phase = match decoder.u8()? {
                0 => PublishQueueInFlightPhase::AwaitingHandoff,
                1 => PublishQueueInFlightPhase::AwaitingAck {
                    deadline: Timestamp::from(decoder.u64()?),
                },
                other => return Err(PublishQueueCodecError::InvalidTag("in-flight phase", other)),
            };
            Ok(PublishQueueLaneState::InFlight { ordinal, phase })
        }
        4 => Ok(PublishQueueLaneState::Transient {
            ordinal: decoder.u64()?,
            eligible_at: Timestamp::from(decoder.u64()?),
            cause: decode_transient_cause(decoder)?,
            raw_reason: decoder.optional_text(MAX_REASON_BYTES, "transient raw reason")?,
        }),
        5 => Ok(PublishQueueLaneState::Terminal {
            ordinal: decoder.u64()?,
            outcome: decode_terminal(decoder)?,
        }),
        other => Err(PublishQueueCodecError::InvalidTag("lane state", other)),
    }
}

pub(crate) fn encode_lane(
    revision: u64,
    last_ordinal: u64,
    state: &PublishQueueLaneState,
) -> Result<Vec<u8>, PublishQueueCodecError> {
    if revision == 0 {
        return Err(PublishQueueCodecError::InvalidValue(
            "lane revision must be non-zero",
        ));
    }
    let mut encoder = Encoder::new(LANE_MAGIC);
    encoder.u64(revision);
    encoder.u64(last_ordinal);
    encode_lane_state(&mut encoder, state)?;
    Ok(encoder.finish())
}

pub(super) fn decode_lane(
    bytes: &[u8],
) -> Result<(u64, u64, PublishQueueLaneState), PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, LANE_MAGIC)?;
    let revision = decoder.u64()?;
    let last_ordinal = decoder.u64()?;
    let state = decode_lane_state(&mut decoder)?;
    decoder.finish()?;
    if revision == 0 {
        return Err(PublishQueueCodecError::InvalidValue(
            "lane revision must be non-zero",
        ));
    }
    let state_ordinal = match &state {
        PublishQueueLaneState::InFlight { ordinal, .. }
        | PublishQueueLaneState::Transient { ordinal, .. }
        | PublishQueueLaneState::Terminal { ordinal, .. } => Some(*ordinal),
        _ => None,
    };
    if state_ordinal.is_some_and(|ordinal| ordinal != last_ordinal) {
        return Err(PublishQueueCodecError::InvalidValue(
            "lane state ordinal disagrees with cursor",
        ));
    }
    Ok((revision, last_ordinal, state))
}

pub(super) fn encode_deadline(lane_revision: u64, kind: PublishQueueDeadlineKind) -> Vec<u8> {
    let mut encoder = Encoder::new(DEADLINE_MAGIC);
    encoder.u64(lane_revision);
    encoder.u8(match kind {
        PublishQueueDeadlineKind::RetryEligible => 0,
        PublishQueueDeadlineKind::AckTimeout => 1,
    });
    encoder.finish()
}

pub(super) fn decode_deadline(
    bytes: &[u8],
) -> Result<(u64, PublishQueueDeadlineKind), PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, DEADLINE_MAGIC)?;
    let lane_revision = decoder.u64()?;
    let kind = match decoder.u8()? {
        0 => PublishQueueDeadlineKind::RetryEligible,
        1 => PublishQueueDeadlineKind::AckTimeout,
        other => return Err(PublishQueueCodecError::InvalidTag("deadline kind", other)),
    };
    decoder.finish()?;
    Ok((lane_revision, kind))
}

fn encode_handoff_result(encoder: &mut Encoder, result: HandoffEvidence) {
    encoder.u8(match result {
        HandoffEvidence::NotHandedOff => 0,
        HandoffEvidence::Written => 1,
        HandoffEvidence::Ambiguous => 2,
    });
}

fn decode_handoff_result(
    decoder: &mut Decoder<'_>,
) -> Result<HandoffEvidence, PublishQueueCodecError> {
    match decoder.u8()? {
        0 => Ok(HandoffEvidence::NotHandedOff),
        1 => Ok(HandoffEvidence::Written),
        2 => Ok(HandoffEvidence::Ambiguous),
        other => Err(PublishQueueCodecError::InvalidTag(
            "handoff evidence",
            other,
        )),
    }
}

pub(crate) fn encode_attempt_details(
    details: &PublishQueueAttemptDetails,
) -> Result<Vec<u8>, PublishQueueCodecError> {
    let mut encoder = Encoder::new(DETAIL_MAGIC);
    match details.started_at {
        None => encoder.u8(0),
        Some(at) => {
            encoder.u8(1);
            encoder.u64(at.as_secs());
        }
    }
    match &details.handoff {
        None => encoder.u8(0),
        Some(handoff) => {
            encoder.u8(1);
            encoder.u64(handoff.at.as_secs());
            encode_handoff_result(&mut encoder, handoff.result);
        }
    }
    match &details.transient {
        None => encoder.u8(0),
        Some(transient) => {
            encoder.u8(1);
            encoder.u64(transient.eligible_at.as_secs());
            encode_transient_cause(&mut encoder, transient.cause);
            encoder.optional_text(
                transient.raw_reason.as_deref(),
                MAX_REASON_BYTES,
                "transient raw reason",
            )?;
        }
    }
    match details.finished_at {
        None => encoder.u8(0),
        Some(at) => {
            encoder.u8(1);
            encoder.u64(at.as_secs());
        }
    }
    match &details.terminal {
        None => encoder.u8(0),
        Some(outcome) => {
            encoder.u8(1);
            encode_attempt_outcome(&mut encoder, outcome)?;
        }
    }
    Ok(encoder.finish())
}

fn decode_attempt_detail_prefix(
    decoder: &mut Decoder<'_>,
) -> Result<(Option<Timestamp>, Option<PublishQueueAttemptHandoff>), PublishQueueCodecError> {
    let started_at = match decoder.u8()? {
        0 => None,
        1 => Some(Timestamp::from(decoder.u64()?)),
        other => return Err(PublishQueueCodecError::InvalidTag("attempt start", other)),
    };
    let handoff = match decoder.u8()? {
        0 => None,
        1 => Some(PublishQueueAttemptHandoff {
            at: Timestamp::from(decoder.u64()?),
            result: decode_handoff_result(decoder)?,
        }),
        other => return Err(PublishQueueCodecError::InvalidTag("attempt handoff", other)),
    };
    Ok((started_at, handoff))
}

type DecodedAttemptDetail = (
    Option<Timestamp>,
    Option<PublishQueueAttemptHandoff>,
    Option<PublishQueueAttemptTransient>,
    Option<Timestamp>,
    Option<PublishQueueAttemptOutcome>,
);

fn decode_attempt_detail(bytes: &[u8]) -> Result<DecodedAttemptDetail, PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, DETAIL_MAGIC)?;
    let (started_at, handoff) = decode_attempt_detail_prefix(&mut decoder)?;
    let transient = match decoder.u8()? {
        0 => None,
        1 => Some(PublishQueueAttemptTransient {
            eligible_at: Timestamp::from(decoder.u64()?),
            cause: decode_transient_cause(&mut decoder)?,
            raw_reason: decoder.optional_text(MAX_REASON_BYTES, "transient raw reason")?,
        }),
        other => {
            return Err(PublishQueueCodecError::InvalidTag(
                "attempt transient",
                other,
            ))
        }
    };
    let finished_at = match decoder.u8()? {
        0 => None,
        1 => Some(Timestamp::from(decoder.u64()?)),
        other => return Err(PublishQueueCodecError::InvalidTag("attempt finish", other)),
    };
    let terminal = match decoder.u8()? {
        0 => None,
        1 => Some(decode_attempt_outcome(&mut decoder)?),
        other => {
            return Err(PublishQueueCodecError::InvalidTag(
                "attempt terminal",
                other,
            ))
        }
    };
    decoder.finish()?;
    if terminal == Some(PublishQueueAttemptOutcome::Started) {
        return Err(PublishQueueCodecError::InvalidValue(
            "attempt terminal cannot contain Started",
        ));
    }
    if finished_at.is_some() != terminal.is_some() {
        return Err(PublishQueueCodecError::InvalidValue(
            "attempt finish and terminal must coexist",
        ));
    }
    Ok((started_at, handoff, transient, finished_at, terminal))
}

pub(super) fn decode_attempt_handoff(
    bytes: &[u8],
) -> Result<Option<PublishQueueAttemptHandoff>, PublishQueueCodecError> {
    let (_, handoff, _, _, _) = decode_attempt_detail(bytes)?;
    Ok(handoff)
}

pub(super) fn decode_attempt_details(
    bytes: &[u8],
    intent_id: IntentId,
    relay: RelayUrl,
    ordinal: u64,
) -> Result<PublishQueueAttemptDetails, PublishQueueCodecError> {
    let (started_at, handoff, transient, finished_at, terminal) = decode_attempt_detail(bytes)?;
    Ok(PublishQueueAttemptDetails {
        version: 1,
        intent_id,
        relay,
        ordinal,
        started_at,
        handoff,
        transient,
        finished_at,
        terminal,
    })
}

pub(super) fn encode_claims(
    claims: &[SuppressClaimRecord],
) -> Result<Vec<u8>, PublishQueueCodecError> {
    if claims.len() > MAX_SUPPRESSION_CLAIMS {
        return Err(PublishQueueCodecError::BoundExceeded(
            "suppression claim count",
        ));
    }
    let mut encoder = Encoder::new(CLAIMS_MAGIC);
    encoder.u32(claims.len() as u32);
    for claim in claims {
        match claim {
            SuppressClaimRecord::Id {
                target,
                claiming_author,
            } => {
                encoder.u8(0);
                encoder.fixed(target.as_bytes());
                encoder.fixed(claiming_author.as_bytes());
            }
            SuppressClaimRecord::Addr {
                key,
                ceiling,
                deleting_author,
            } => {
                encoder.u8(1);
                encoder.bytes(key, MAX_TEXT_BYTES, "address suppression key")?;
                encoder.u64(*ceiling);
                encoder.fixed(deleting_author.as_bytes());
            }
        }
    }
    Ok(encoder.finish())
}

pub(super) fn decode_claims(
    bytes: &[u8],
) -> Result<Vec<SuppressClaimRecord>, PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, CLAIMS_MAGIC)?;
    let count = decoder.u32()? as usize;
    if count > MAX_SUPPRESSION_CLAIMS {
        return Err(PublishQueueCodecError::BoundExceeded(
            "suppression claim count",
        ));
    }
    let mut claims = Vec::with_capacity(count);
    for _ in 0..count {
        claims.push(match decoder.u8()? {
            0 => SuppressClaimRecord::Id {
                target: decoder.event_id()?,
                claiming_author: decoder.public_key()?,
            },
            1 => SuppressClaimRecord::Addr {
                key: decoder
                    .bytes(MAX_TEXT_BYTES, "address suppression key")?
                    .to_vec(),
                ceiling: decoder.u64()?,
                deleting_author: decoder.public_key()?,
            },
            other => {
                return Err(PublishQueueCodecError::InvalidTag(
                    "suppression claim",
                    other,
                ))
            }
        });
    }
    decoder.finish()?;
    Ok(claims)
}

pub(super) fn encode_claimants(claimants: &[u64]) -> Result<Vec<u8>, PublishQueueCodecError> {
    if claimants.len() > MAX_SUPPRESSION_CLAIMS {
        return Err(PublishQueueCodecError::BoundExceeded("claimant count"));
    }
    let mut canonical = claimants.to_vec();
    canonical.sort_unstable();
    canonical.dedup();
    let mut encoder = Encoder::new(CLAIMANTS_MAGIC);
    encoder.u32(canonical.len() as u32);
    for claimant in canonical {
        encoder.u64(claimant);
    }
    Ok(encoder.finish())
}

pub(super) fn decode_claimants(bytes: &[u8]) -> Result<Vec<u64>, PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, CLAIMANTS_MAGIC)?;
    let count = decoder.u32()? as usize;
    if count > MAX_SUPPRESSION_CLAIMS {
        return Err(PublishQueueCodecError::BoundExceeded("claimant count"));
    }
    let mut claimants = Vec::with_capacity(count);
    for _ in 0..count {
        claimants.push(decoder.u64()?);
    }
    decoder.finish()?;
    if claimants.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(PublishQueueCodecError::InvalidValue(
            "claimants must be strictly ordered",
        ));
    }
    Ok(claimants)
}

pub(super) fn encode_addr_claimants(
    claimants: &[AddrClaimant],
) -> Result<Vec<u8>, PublishQueueCodecError> {
    if claimants.len() > MAX_SUPPRESSION_CLAIMS {
        return Err(PublishQueueCodecError::BoundExceeded(
            "address claimant count",
        ));
    }
    let mut canonical = claimants.to_vec();
    canonical.sort_by_key(|claimant| claimant.intent_id);
    if canonical
        .windows(2)
        .any(|pair| pair[0].intent_id == pair[1].intent_id)
    {
        return Err(PublishQueueCodecError::InvalidValue(
            "duplicate address claimant",
        ));
    }
    let mut encoder = Encoder::new(ADDR_CLAIMANTS_MAGIC);
    encoder.u32(canonical.len() as u32);
    for claimant in canonical {
        encoder.u64(claimant.intent_id);
        encoder.u64(claimant.ceiling);
    }
    Ok(encoder.finish())
}

pub(super) fn decode_addr_claimants(
    bytes: &[u8],
) -> Result<Vec<AddrClaimant>, PublishQueueCodecError> {
    let mut decoder = Decoder::new(bytes, ADDR_CLAIMANTS_MAGIC)?;
    let count = decoder.u32()? as usize;
    if count > MAX_SUPPRESSION_CLAIMS {
        return Err(PublishQueueCodecError::BoundExceeded(
            "address claimant count",
        ));
    }
    let mut claimants = Vec::with_capacity(count);
    for _ in 0..count {
        claimants.push(AddrClaimant {
            intent_id: decoder.u64()?,
            ceiling: decoder.u64()?,
        });
    }
    decoder.finish()?;
    if claimants
        .windows(2)
        .any(|pair| pair[0].intent_id >= pair[1].intent_id)
    {
        return Err(PublishQueueCodecError::InvalidValue(
            "address claimants must be strictly ordered",
        ));
    }
    Ok(claimants)
}

pub(super) fn encode_displaced(stored: &StoredEvent) -> Result<Vec<u8>, PublishQueueCodecError> {
    let encoded = binary_event::encode(stored).map_err(|_| PublishQueueCodecError::InvalidEvent)?;
    if encoded.len() > MAX_EVENT_BYTES {
        return Err(PublishQueueCodecError::BoundExceeded("displaced event"));
    }
    Ok(encoded)
}

pub(super) fn decode_displaced(bytes: &[u8]) -> Result<StoredEvent, PublishQueueCodecError> {
    if bytes.len() > MAX_EVENT_BYTES {
        return Err(PublishQueueCodecError::BoundExceeded("displaced event"));
    }
    binary_event::decode(bytes).map_err(|_| PublishQueueCodecError::InvalidEvent)
}

pub(super) fn canonical_route_ids(
    relays: impl IntoIterator<Item = PublishQueueRelayId>,
) -> Vec<PublishQueueRelayId> {
    relays
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind};

    #[test]
    fn relay_vector_is_human_explained_and_canonical() {
        let relay = RelayUrl::parse("wss://relay.example/").unwrap();
        let encoded = encode_relay(&relay).unwrap();
        let mut expected = b"NMDU\x01\0\0\0".to_vec();
        expected.extend_from_slice(&20u32.to_be_bytes());
        expected.extend_from_slice(b"wss://relay.example/");
        assert_eq!(encoded, expected);
        assert_eq!(decode_relay(&expected).unwrap(), relay);
    }

    #[test]
    fn key_vectors_are_big_endian_and_prefix_sortable() {
        assert_eq!(
            lane_key(IntentId(0x0102_0304_0506_0708), 0x1112_1314),
            [1, 2, 3, 4, 5, 6, 7, 8, 0x11, 0x12, 0x13, 0x14,]
        );
        assert!(attempt_key(IntentId(7), 3, 8) < attempt_key(IntentId(7), 3, 9));
        assert!(
            deadline_key(Timestamp::from(8), IntentId(9), 10)
                < deadline_key(Timestamp::from(9), IntentId(0), 0)
        );
    }

    #[test]
    fn malformed_lengths_versions_and_trailing_bytes_fail_closed() {
        let relay = RelayUrl::parse("wss://relay.example/").unwrap();
        let encoded = encode_relay(&relay).unwrap();
        for length in 0..encoded.len() {
            assert_eq!(
                decode_relay(&encoded[..length]),
                Err(PublishQueueCodecError::Truncated)
            );
        }
        let mut unknown = encoded.clone();
        unknown[4] = 9;
        assert_eq!(
            decode_relay(&unknown),
            Err(PublishQueueCodecError::UnsupportedVersion(9))
        );
        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(
            decode_relay(&trailing),
            Err(PublishQueueCodecError::TrailingBytes)
        );
    }

    #[test]
    fn overlong_noncanonical_and_invalid_utf8_values_fail_before_adoption() {
        let overlong = RelayUrl::parse(&format!(
            "wss://relay.example/{}",
            "x".repeat(MAX_RELAY_BYTES)
        ))
        .unwrap();
        assert_eq!(
            encode_relay(&overlong),
            Err(PublishQueueCodecError::BoundExceeded("relay URL"))
        );

        let noncanonical_text = b"WSS://RELAY.EXAMPLE";
        let mut noncanonical = b"NMDU\x01\0\0\0".to_vec();
        noncanonical.extend_from_slice(&(noncanonical_text.len() as u32).to_be_bytes());
        noncanonical.extend_from_slice(noncanonical_text);
        assert_eq!(
            decode_relay(&noncanonical),
            Err(PublishQueueCodecError::NonCanonicalRelay)
        );

        let mut invalid_utf8 = b"NMDU\x01\0\0\0".to_vec();
        invalid_utf8.extend_from_slice(&1u32.to_be_bytes());
        invalid_utf8.push(0xff);
        assert_eq!(
            decode_relay(&invalid_utf8),
            Err(PublishQueueCodecError::InvalidUtf8)
        );

        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "bounded reason")
            .sign_with_keys(&keys)
            .unwrap();
        assert_eq!(
            encode_attempt(
                &event,
                &PublishQueueAttemptOutcome::Rejected("x".repeat(MAX_REASON_BYTES + 1))
            ),
            Err(PublishQueueCodecError::BoundExceeded("rejection reason"))
        );
    }

    #[test]
    fn attempt_event_round_trip_is_exact() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "binary delivery")
            .sign_with_keys(&keys)
            .unwrap();
        let encoded = encode_attempt(
            &event,
            &PublishQueueAttemptOutcome::Rejected("blocked".to_owned()),
        )
        .unwrap();
        let decoded = decode_attempt(&encoded).unwrap();
        assert_eq!(
            decoded,
            (
                event,
                PublishQueueAttemptOutcome::Rejected("blocked".to_owned())
            )
        );
    }
}
