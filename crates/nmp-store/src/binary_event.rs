//! Portable binary codecs for canonical persisted event state.
//!
//! Immutable signed NIP-01 event bytes, optional canonical local state, and
//! self-contained displaced-row provenance deliberately have distinct
//! envelopes. Canonical relay observations are fixed-width redb rows outside
//! this codec, while queries borrow fixed fields, tags, and content directly
//! from the immutable event value.
//! Fixed-width integers are big-endian, arena string lengths use canonical
//! unsigned LEB128, and every parser uses checked slices; unlike nostrdb's
//! packed native struct, this is alignment-safe and endian-stable on every
//! Rust target, including wasm.

use std::collections::{BTreeMap, BTreeSet};

use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event, EventId, Filter, Kind, PublicKey, RelayUrl, SingleLetterTag, Tag, Tags, Timestamp,
};

use crate::{IntentId, LocalOrigin, Provenance, SigState, StoredEvent};

const EVENT_MAGIC: &[u8; 4] = b"NMPE";
const LOCAL_MAGIC: &[u8; 4] = b"NMPL";
const PROVENANCE_MAGIC: &[u8; 4] = b"NMPP";
const COMPOSITE_MAGIC: &[u8; 4] = b"NMPC";
const EVENT_VERSION: u8 = 4;
const SIDECAR_VERSION: u8 = 3;
const COMPOSITE_VERSION: u8 = 4;
const FLAG_LOCAL: u8 = 1;

// Event v4:
// fixed header = magic + version + reserved + id + pubkey + sig + created_at
// + kind + tag_count + tag_section_len + content_len
// tag section = cumulative tag ends[u32] + atom descriptors[u32] + dense arena
// content = direct UTF-8 bytes after the tag section.
const EVENT_HEADER_LEN: usize = 158;
const ID_OFFSET: usize = 8;
const PUBKEY_OFFSET: usize = ID_OFFSET + 32;
const SIG_OFFSET: usize = PUBKEY_OFFSET + 32;
const CREATED_AT_OFFSET: usize = SIG_OFFSET + 64;
const KIND_OFFSET: usize = CREATED_AT_OFFSET + 8;
const TAG_COUNT_OFFSET: usize = KIND_OFFSET + 2;
const TAG_SECTION_LEN_OFFSET: usize = TAG_COUNT_OFFSET + 4;
const CONTENT_LEN_OFFSET: usize = TAG_SECTION_LEN_OFFSET + 4;

const ATOM_KIND_MASK: u32 = 0xc000_0000;
const ATOM_OFFSET_MASK: u32 = 0x3fff_ffff;
const ATOM_INLINE: u32 = 0;
const ATOM_UTF8: u32 = 0x4000_0000;
const ATOM_RAW32: u32 = 0x8000_0000;

// Atom descriptor high bits: 00 inline (len in bits 29..28, 3 payload bytes),
// 01 canonical-LEB-length UTF-8 arena cell, 10 raw 32-byte identity cell,
// 11 reserved. Arena descriptors carry a 30-bit arena-relative offset.

// magic + version + sig_state + reserved + owner_count
const LOCAL_HEADER_LEN: usize = 12;
const LOCAL_OWNER_COUNT_OFFSET: usize = 8;

// magic + version + flags + reserved + seen_count + owner_count
const PROVENANCE_HEADER_LEN: usize = 16;
const PROVENANCE_SEEN_COUNT_OFFSET: usize = 8;
const PROVENANCE_OWNER_COUNT_OFFSET: usize = 12;

// magic + version + reserved + event_len + provenance_len
const COMPOSITE_HEADER_LEN: usize = 16;
const COMPOSITE_EVENT_LEN_OFFSET: usize = 8;
const COMPOSITE_PROVENANCE_LEN_OFFSET: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u8),
    InvalidFlags(u8),
    InvalidReserved,
    InvalidUtf8,
    EmptyTag,
    InvalidTag,
    InvalidRelay,
    DuplicateRelay,
    DuplicateOwner,
    InvalidOwnerOrder,
    InvalidSignature,
    InvalidLocalState(u8),
    TrailingBytes,
    LengthOverflow,
    InvalidAtom,
    NonCanonicalLength,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: usize) -> Result<(), DecodeError> {
    let value = u32::try_from(value).map_err(|_| DecodeError::LengthOverflow)?;
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, DecodeError> {
    let raw: [u8; 2] = bytes
        .get(offset..offset + 2)
        .ok_or(DecodeError::Truncated)?
        .try_into()
        .expect("length checked");
    Ok(u16::from_be_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, DecodeError> {
    let raw: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or(DecodeError::Truncated)?
        .try_into()
        .expect("length checked");
    Ok(u32::from_be_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, DecodeError> {
    let raw: [u8; 8] = bytes
        .get(offset..offset + 8)
        .ok_or(DecodeError::Truncated)?
        .try_into()
        .expect("length checked");
    Ok(u64::from_be_bytes(raw))
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], DecodeError> {
    let end = cursor.checked_add(len).ok_or(DecodeError::LengthOverflow)?;
    let value = bytes.get(*cursor..end).ok_or(DecodeError::Truncated)?;
    *cursor = end;
    Ok(value)
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, DecodeError> {
    let value = read_u32(bytes, *cursor)?;
    *cursor = cursor.checked_add(4).ok_or(DecodeError::LengthOverflow)?;
    Ok(value)
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, DecodeError> {
    let value = read_u64(bytes, *cursor)?;
    *cursor = cursor.checked_add(8).ok_or(DecodeError::LengthOverflow)?;
    Ok(value)
}

fn check_magic_and_version(
    bytes: &[u8],
    magic: &[u8; 4],
    expected_version: u8,
) -> Result<(), DecodeError> {
    if bytes.get(..4) != Some(magic) {
        return Err(if bytes.len() < 4 {
            DecodeError::Truncated
        } else {
            DecodeError::BadMagic
        });
    }
    let version = *bytes.get(4).ok_or(DecodeError::Truncated)?;
    if version != expected_version {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    Ok(())
}

fn checked_capacity(base: usize, first: usize, second: usize) -> Result<usize, DecodeError> {
    base.checked_add(first)
        .and_then(|value| value.checked_add(second))
        .ok_or(DecodeError::LengthOverflow)
}

fn checked_add_len(total: &mut usize, value: usize) -> Result<(), DecodeError> {
    *total = total
        .checked_add(value)
        .ok_or(DecodeError::LengthOverflow)?;
    Ok(())
}

pub(crate) fn decode_hex_32(value: &str) -> Option<[u8; 32]> {
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut raw = [0u8; 32];
    for (index, byte) in raw.iter_mut().enumerate() {
        let pair = index * 2;
        *byte = (nibble(bytes[pair])? << 4) | nibble(bytes[pair + 1])?;
    }
    Some(raw)
}

fn encode_hex_32(raw: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = vec![0u8; 64];
    for (index, byte) in raw.iter().copied().enumerate() {
        out[index * 2] = HEX[(byte >> 4) as usize];
        out[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    String::from_utf8(out).expect("lowercase hex is utf8")
}

fn push_leb128_u32(out: &mut Vec<u8>, value: usize) -> Result<(), DecodeError> {
    let mut value = u32::try_from(value).map_err(|_| DecodeError::LengthOverflow)?;
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return Ok(());
        }
    }
}

fn leb128_u32_len(value: usize) -> Result<usize, DecodeError> {
    let value = u32::try_from(value).map_err(|_| DecodeError::LengthOverflow)?;
    Ok(match value {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        0x4000..=0x1f_ffff => 3,
        0x20_0000..=0x0fff_ffff => 4,
        _ => 5,
    })
}

fn take_leb128_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, DecodeError> {
    let mut value = 0u32;
    for index in 0..5u32 {
        let byte = *take(bytes, cursor, 1)?.first().expect("one byte was taken");
        let payload = u32::from(byte & 0x7f);
        if index == 4 && payload > 0x0f {
            return Err(DecodeError::LengthOverflow);
        }
        value |= payload << (index * 7);
        if byte & 0x80 == 0 {
            if index > 0 && payload == 0 {
                return Err(DecodeError::NonCanonicalLength);
            }
            return Ok(value);
        }
    }
    Err(DecodeError::LengthOverflow)
}

fn arena_offset_descriptor(kind: u32, arena_len: usize) -> Result<u32, DecodeError> {
    let offset = u32::try_from(arena_len).map_err(|_| DecodeError::LengthOverflow)?;
    if offset > ATOM_OFFSET_MASK {
        return Err(DecodeError::LengthOverflow);
    }
    Ok(kind | offset)
}

fn inline_atom_descriptor(bytes: &[u8]) -> u32 {
    debug_assert!(bytes.len() <= 3);
    let mut payload = [0u8; 4];
    payload[1..1 + bytes.len()].copy_from_slice(bytes);
    ((bytes.len() as u32) << 28) | u32::from_be_bytes(payload)
}

fn encoded_tag_shape(tags: &[Tag]) -> Result<(usize, usize), DecodeError> {
    let mut atom_count = 0usize;
    let mut arena_len = 0usize;
    for tag in tags {
        if tag.as_slice().is_empty() {
            return Err(DecodeError::EmptyTag);
        }
        for element in tag.as_slice() {
            atom_count = atom_count
                .checked_add(1)
                .ok_or(DecodeError::LengthOverflow)?;
            let len = element.len();
            if len <= 3 {
                continue;
            }
            let cell_len = if decode_hex_32(element).is_some() {
                32
            } else {
                leb128_u32_len(len)?
                    .checked_add(len)
                    .ok_or(DecodeError::LengthOverflow)?
            };
            arena_len = arena_len
                .checked_add(cell_len)
                .ok_or(DecodeError::LengthOverflow)?;
            if arena_len > ATOM_OFFSET_MASK as usize {
                return Err(DecodeError::LengthOverflow);
            }
        }
    }
    u32::try_from(atom_count).map_err(|_| DecodeError::LengthOverflow)?;
    Ok((atom_count, arena_len))
}

fn encoded_provenance_body_len(provenance: &Provenance) -> Result<usize, DecodeError> {
    let mut total = 0usize;
    for relay in provenance.seen.keys() {
        checked_add_len(&mut total, 4)?;
        checked_add_len(&mut total, relay.as_str().len())?;
        checked_add_len(&mut total, 8)?;
    }
    if let Some(local) = &provenance.local {
        checked_add_len(&mut total, 1)?;
        let owners_len = local
            .owners
            .len()
            .checked_mul(8)
            .ok_or(DecodeError::LengthOverflow)?;
        checked_add_len(&mut total, owners_len)?;
    }
    Ok(total)
}

fn checked_local_len(owner_count: usize) -> Result<usize, DecodeError> {
    u32::try_from(owner_count).map_err(|_| DecodeError::LengthOverflow)?;
    let owners_len = owner_count
        .checked_mul(8)
        .ok_or(DecodeError::LengthOverflow)?;
    LOCAL_HEADER_LEN
        .checked_add(owners_len)
        .ok_or(DecodeError::LengthOverflow)
}

/// Encode only immutable, signed NIP-01 event data.
pub(crate) fn encode_event(event: &Event) -> Result<Vec<u8>, DecodeError> {
    let tags = event.tags.as_slice();
    let (atom_count, arena_len) = encoded_tag_shape(tags)?;
    let directory_len = tags
        .len()
        .checked_add(atom_count)
        .and_then(|count| count.checked_mul(4))
        .ok_or(DecodeError::LengthOverflow)?;
    let tag_section_len = directory_len
        .checked_add(arena_len)
        .ok_or(DecodeError::LengthOverflow)?;
    u32::try_from(tags.len()).map_err(|_| DecodeError::LengthOverflow)?;
    u32::try_from(tag_section_len).map_err(|_| DecodeError::LengthOverflow)?;
    u32::try_from(event.content.len()).map_err(|_| DecodeError::LengthOverflow)?;
    let capacity = EVENT_HEADER_LEN
        .checked_add(tag_section_len)
        .and_then(|value| value.checked_add(event.content.len()))
        .ok_or(DecodeError::LengthOverflow)?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(EVENT_MAGIC);
    out.push(EVENT_VERSION);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(event.id.as_bytes());
    out.extend_from_slice(event.pubkey.as_bytes());
    out.extend_from_slice(event.sig.as_ref());
    push_u64(&mut out, event.created_at.as_secs());
    push_u16(&mut out, event.kind.as_u16());
    push_u32(&mut out, tags.len())?;
    push_u32(&mut out, tag_section_len)?;
    push_u32(&mut out, event.content.len())?;
    debug_assert_eq!(out.len(), EVENT_HEADER_LEN);

    let atom_refs_start = EVENT_HEADER_LEN + tags.len() * 4;
    let arena_start = atom_refs_start + atom_count * 4;
    out.resize(arena_start, 0);
    let mut atom_index = 0usize;
    for (tag_index, tag) in tags.iter().enumerate() {
        for element in tag.as_slice() {
            let bytes = element.as_bytes();
            let arena_offset = out.len() - arena_start;
            let descriptor = if bytes.len() <= 3 {
                inline_atom_descriptor(bytes)
            } else if let Some(raw) = decode_hex_32(element) {
                let descriptor = arena_offset_descriptor(ATOM_RAW32, arena_offset)?;
                out.extend_from_slice(&raw);
                descriptor
            } else {
                let descriptor = arena_offset_descriptor(ATOM_UTF8, arena_offset)?;
                push_leb128_u32(&mut out, bytes.len())?;
                out.extend_from_slice(bytes);
                descriptor
            };
            let descriptor_offset = atom_refs_start + atom_index * 4;
            out[descriptor_offset..descriptor_offset + 4]
                .copy_from_slice(&descriptor.to_be_bytes());
            atom_index += 1;
        }
        out[EVENT_HEADER_LEN + tag_index * 4..EVENT_HEADER_LEN + (tag_index + 1) * 4]
            .copy_from_slice(
                &u32::try_from(atom_index)
                    .map_err(|_| DecodeError::LengthOverflow)?
                    .to_be_bytes(),
            );
    }
    debug_assert_eq!(out.len(), EVENT_HEADER_LEN + tag_section_len);
    out.extend_from_slice(event.content.as_bytes());
    debug_assert_eq!(out.len(), capacity);
    Ok(out)
}

/// Encode the canonical local-intent ownership/signature state independently
/// from relay observations. Table absence represents `Provenance::local ==
/// None`; this envelope therefore always represents `Some(LocalOrigin)`, even
/// when its owner set is empty.
pub(crate) fn encode_local(local: &LocalOrigin) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(checked_local_len(local.owners.len())?);
    out.extend_from_slice(LOCAL_MAGIC);
    out.push(SIDECAR_VERSION);
    out.push(match local.sig_state {
        SigState::Pending => 0,
        SigState::Signed => 1,
    });
    out.extend_from_slice(&[0; 2]);
    push_u32(&mut out, local.owners.len())?;
    debug_assert_eq!(out.len(), LOCAL_HEADER_LEN);
    for owner in &local.owners {
        push_u64(&mut out, owner.0);
    }
    Ok(out)
}

/// Decode one canonical local-state envelope. Owner ids must already be in
/// canonical set form: repeated ids are rejected rather than silently folded.
pub(crate) fn decode_local(bytes: &[u8]) -> Result<LocalOrigin, DecodeError> {
    if bytes.len() < LOCAL_HEADER_LEN {
        return Err(DecodeError::Truncated);
    }
    check_magic_and_version(bytes, LOCAL_MAGIC, SIDECAR_VERSION)?;
    let sig_state = match bytes[5] {
        0 => SigState::Pending,
        1 => SigState::Signed,
        other => return Err(DecodeError::InvalidLocalState(other)),
    };
    if bytes[6..8] != [0, 0] {
        return Err(DecodeError::InvalidReserved);
    }
    let owner_count = read_u32(bytes, LOCAL_OWNER_COUNT_OFFSET)? as usize;
    let expected_len = checked_local_len(owner_count)?;
    if expected_len > bytes.len() {
        return Err(DecodeError::Truncated);
    }
    if expected_len < bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }

    let mut cursor = LOCAL_HEADER_LEN;
    let mut owners = BTreeSet::new();
    let mut previous_owner = None;
    for _ in 0..owner_count {
        let owner = IntentId(take_u64(bytes, &mut cursor)?);
        if let Some(previous) = previous_owner {
            if owner.0 == previous {
                return Err(DecodeError::DuplicateOwner);
            }
            if owner.0 < previous {
                return Err(DecodeError::InvalidOwnerOrder);
            }
        }
        if !owners.insert(owner) {
            return Err(DecodeError::DuplicateOwner);
        }
        previous_owner = Some(owner.0);
    }
    debug_assert_eq!(cursor, expected_len);
    Ok(LocalOrigin { owners, sig_state })
}

/// Encode only mutable relay provenance and local-intent ownership state.
pub(crate) fn encode_provenance(provenance: &Provenance) -> Result<Vec<u8>, DecodeError> {
    let local = provenance.local.as_ref();
    let flags = if local.is_some() { FLAG_LOCAL } else { 0 };
    let owner_count = local.map(|value| value.owners.len()).unwrap_or(0);

    let body_len = encoded_provenance_body_len(provenance)?;
    let capacity = checked_capacity(PROVENANCE_HEADER_LEN, body_len, 0)?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(PROVENANCE_MAGIC);
    out.push(SIDECAR_VERSION);
    out.push(flags);
    out.extend_from_slice(&[0; 2]);
    push_u32(&mut out, provenance.seen.len())?;
    push_u32(&mut out, owner_count)?;
    debug_assert_eq!(out.len(), PROVENANCE_HEADER_LEN);

    for (relay, at) in &provenance.seen {
        push_u32(&mut out, relay.as_str().len())?;
        out.extend_from_slice(relay.as_str().as_bytes());
        push_u64(&mut out, at.as_secs());
    }
    if let Some(local) = local {
        out.push(match local.sig_state {
            SigState::Pending => 0,
            SigState::Signed => 1,
        });
        for owner in &local.owners {
            push_u64(&mut out, owner.0);
        }
    }
    Ok(out)
}

/// Decode a self-contained mutable provenance sidecar.
pub(crate) fn decode_provenance(bytes: &[u8]) -> Result<Provenance, DecodeError> {
    if bytes.len() < PROVENANCE_HEADER_LEN {
        return Err(DecodeError::Truncated);
    }
    check_magic_and_version(bytes, PROVENANCE_MAGIC, SIDECAR_VERSION)?;
    let flags = bytes[5];
    if flags & !FLAG_LOCAL != 0 {
        return Err(DecodeError::InvalidFlags(flags));
    }
    if bytes[6..8] != [0, 0] {
        return Err(DecodeError::InvalidReserved);
    }
    let seen_count = read_u32(bytes, PROVENANCE_SEEN_COUNT_OFFSET)?;
    let owner_count = read_u32(bytes, PROVENANCE_OWNER_COUNT_OFFSET)?;
    if flags & FLAG_LOCAL == 0 && owner_count != 0 {
        return Err(DecodeError::InvalidFlags(flags));
    }

    let mut cursor = PROVENANCE_HEADER_LEN;
    let mut seen = BTreeMap::new();
    for _ in 0..seen_count {
        let relay_len = take_u32(bytes, &mut cursor)? as usize;
        let relay = std::str::from_utf8(take(bytes, &mut cursor, relay_len)?)
            .map_err(|_| DecodeError::InvalidUtf8)?;
        let relay = RelayUrl::parse(relay).map_err(|_| DecodeError::InvalidRelay)?;
        let at = Timestamp::from(take_u64(bytes, &mut cursor)?);
        if seen.insert(relay, at).is_some() {
            return Err(DecodeError::DuplicateRelay);
        }
    }

    let local = if flags & FLAG_LOCAL != 0 {
        let state = *take(bytes, &mut cursor, 1)?
            .first()
            .expect("one byte requested");
        let sig_state = match state {
            0 => SigState::Pending,
            1 => SigState::Signed,
            other => return Err(DecodeError::InvalidLocalState(other)),
        };
        let mut owners = BTreeSet::new();
        for _ in 0..owner_count {
            let owner = IntentId(take_u64(bytes, &mut cursor)?);
            if !owners.insert(owner) {
                return Err(DecodeError::DuplicateOwner);
            }
        }
        Some(LocalOrigin { owners, sig_state })
    } else {
        None
    };

    if cursor != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(Provenance { seen, local })
}

/// Preserve the self-contained encoding used by displaced outbox rows while
/// composing it from the same event and provenance codecs as canonical rows.
pub(crate) fn encode(stored: &StoredEvent) -> Result<Vec<u8>, DecodeError> {
    let event = encode_event(&stored.event)?;
    let provenance = encode_provenance(&stored.provenance)?;
    let capacity = checked_capacity(COMPOSITE_HEADER_LEN, event.len(), provenance.len())?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(COMPOSITE_MAGIC);
    out.push(COMPOSITE_VERSION);
    out.extend_from_slice(&[0; 3]);
    push_u32(&mut out, event.len())?;
    push_u32(&mut out, provenance.len())?;
    debug_assert_eq!(out.len(), COMPOSITE_HEADER_LEN);
    out.extend_from_slice(&event);
    out.extend_from_slice(&provenance);
    Ok(out)
}

/// Decode the self-contained composite used by displaced outbox rows.
pub(crate) fn decode(bytes: &[u8]) -> Result<StoredEvent, DecodeError> {
    if bytes.len() < COMPOSITE_HEADER_LEN {
        return Err(DecodeError::Truncated);
    }
    check_magic_and_version(bytes, COMPOSITE_MAGIC, COMPOSITE_VERSION)?;
    if bytes[5..8] != [0, 0, 0] {
        return Err(DecodeError::InvalidReserved);
    }
    let event_len = read_u32(bytes, COMPOSITE_EVENT_LEN_OFFSET)? as usize;
    let provenance_len = read_u32(bytes, COMPOSITE_PROVENANCE_LEN_OFFSET)? as usize;
    let expected_len = checked_capacity(COMPOSITE_HEADER_LEN, event_len, provenance_len)?;
    if expected_len > bytes.len() {
        return Err(DecodeError::Truncated);
    }
    if expected_len < bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }

    let event_start = COMPOSITE_HEADER_LEN;
    let provenance_start = event_start
        .checked_add(event_len)
        .ok_or(DecodeError::LengthOverflow)?;
    let event =
        StoredEventView::parse(&bytes[event_start..provenance_start])?.materialize_event()?;
    let provenance = decode_provenance(&bytes[provenance_start..expected_len])?;
    Ok(StoredEvent { event, provenance })
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StoredEventView<'a> {
    bytes: &'a [u8],
    tag_count: u32,
    atom_count: u32,
    atom_refs_start: usize,
    arena_start: usize,
    arena_len: usize,
    content_start: usize,
    content_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomRef<'a> {
    Text(&'a str),
    Raw32(&'a [u8; 32]),
}

impl AtomRef<'_> {
    fn is_single_ascii(self, wanted: u8) -> bool {
        matches!(self, Self::Text(text) if text.as_bytes() == [wanted])
    }

    fn to_owned_text(self) -> String {
        match self {
            Self::Text(text) => text.to_owned(),
            Self::Raw32(raw) => encode_hex_32(raw),
        }
    }
}

fn decode_atom<'a>(
    descriptor: &'a [u8; 4],
    arena: &'a [u8],
) -> Result<(AtomRef<'a>, Option<(usize, usize)>), DecodeError> {
    let word = u32::from_be_bytes(*descriptor);
    match word & ATOM_KIND_MASK {
        ATOM_INLINE => {
            let len = ((word >> 28) & 0x03) as usize;
            if word & 0x0f00_0000 != 0 || descriptor[1 + len..].iter().any(|byte| *byte != 0) {
                return Err(DecodeError::InvalidAtom);
            }
            let text = std::str::from_utf8(&descriptor[1..1 + len])
                .map_err(|_| DecodeError::InvalidUtf8)?;
            Ok((AtomRef::Text(text), None))
        }
        ATOM_UTF8 => {
            let offset = (word & ATOM_OFFSET_MASK) as usize;
            let mut cursor = offset;
            let len = take_leb128_u32(arena, &mut cursor)? as usize;
            let raw = take(arena, &mut cursor, len)?;
            let text = std::str::from_utf8(raw).map_err(|_| DecodeError::InvalidUtf8)?;
            if raw.len() <= 3 || decode_hex_32(text).is_some() {
                return Err(DecodeError::InvalidAtom);
            }
            Ok((AtomRef::Text(text), Some((offset, cursor))))
        }
        ATOM_RAW32 => {
            let offset = (word & ATOM_OFFSET_MASK) as usize;
            let mut cursor = offset;
            let raw: &[u8; 32] = take(arena, &mut cursor, 32)?
                .try_into()
                .expect("32 raw identity bytes were taken");
            Ok((AtomRef::Raw32(raw), Some((offset, cursor))))
        }
        _ => Err(DecodeError::InvalidAtom),
    }
}

/// Predicate(s) already proven by the ordered index that yielded a row.
/// nostrdb uses the same matched-field mask so cheap post-filtering does not
/// rescan the selected tag or recompare selected fixed fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexedMatch {
    None,
    Author,
    Kind,
    AuthorKind,
    Tag(SingleLetterTag),
}

struct PreparedTagValues<'a> {
    text: &'a BTreeSet<String>,
    raw32: Vec<[u8; 32]>,
}

impl PreparedTagValues<'_> {
    fn matches(&self, atom: AtomRef<'_>) -> bool {
        match atom {
            AtomRef::Text(actual) => self.text.contains(actual),
            AtomRef::Raw32(actual) => self.raw32.binary_search(actual).is_ok(),
        }
    }
}

pub(crate) struct PreparedFilter<'a> {
    filter: &'a Filter,
    generic_tags: Vec<(SingleLetterTag, PreparedTagValues<'a>)>,
}

impl<'a> PreparedFilter<'a> {
    pub(crate) fn new(filter: &'a Filter) -> Self {
        let generic_tags = filter
            .generic_tags
            .iter()
            .map(|(name, values)| {
                let mut raw32: Vec<_> = values
                    .iter()
                    .filter_map(|value| decode_hex_32(value))
                    .collect();
                raw32.sort_unstable();
                (
                    *name,
                    PreparedTagValues {
                        text: values,
                        raw32,
                    },
                )
            })
            .collect();
        Self {
            filter,
            generic_tags,
        }
    }

    pub(crate) fn needs_event_value_after_index(&self, indexed: IndexedMatch) -> bool {
        let filter = self.filter;
        filter.ids.as_ref().is_some_and(|ids| !ids.is_empty())
            || (!matches!(indexed, IndexedMatch::Author | IndexedMatch::AuthorKind)
                && filter
                    .authors
                    .as_ref()
                    .is_some_and(|authors| !authors.is_empty()))
            || (!matches!(indexed, IndexedMatch::Kind | IndexedMatch::AuthorKind)
                && filter
                    .kinds
                    .as_ref()
                    .is_some_and(|kinds| !kinds.is_empty()))
            || self.generic_tags.iter().any(|(name, _)| {
                !matches!(indexed, IndexedMatch::Tag(indexed_name) if indexed_name == *name)
            })
            || filter.search.as_ref().is_some_and(|query| !query.is_empty())
    }
}

impl<'a> StoredEventView<'a> {
    /// Fully validate an immutable event envelope before exposing borrowed
    /// fields and iterators.
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        let view = Self::from_trusted(bytes)?;
        let mut previous_end = 0u32;
        for index in 0..view.tag_count as usize {
            let end = read_u32(bytes, EVENT_HEADER_LEN + index * 4)?;
            if end <= previous_end {
                return Err(DecodeError::EmptyTag);
            }
            if end > view.atom_count {
                return Err(DecodeError::LengthOverflow);
            }
            previous_end = end;
        }
        if previous_end != view.atom_count {
            return Err(DecodeError::LengthOverflow);
        }
        let arena = &bytes[view.arena_start..view.arena_start + view.arena_len];
        let mut arena_cursor = 0usize;
        for index in 0..view.atom_count as usize {
            let descriptor_offset = view.atom_refs_start + index * 4;
            let descriptor: &[u8; 4] = bytes[descriptor_offset..descriptor_offset + 4]
                .try_into()
                .expect("atom directory bounds checked");
            let (_atom, cell) = decode_atom(descriptor, arena)?;
            if let Some((start, end)) = cell {
                if start != arena_cursor {
                    return Err(DecodeError::InvalidAtom);
                }
                arena_cursor = end;
            }
        }
        if arena_cursor != arena.len() {
            return Err(DecodeError::InvalidAtom);
        }
        std::str::from_utf8(
            bytes
                .get(view.content_start..view.content_start + view.content_len)
                .ok_or(DecodeError::Truncated)?,
        )
        .map_err(|_| DecodeError::InvalidUtf8)?;
        Ok(view)
    }

    /// Build a borrowed view after checking the fixed header and direct
    /// section bounds. Canonical rows were fully validated before insertion;
    /// query hot paths use this door so a fixed-field rejection does not walk
    /// tags or content first.
    pub(crate) fn from_trusted(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        if bytes.len() < EVENT_HEADER_LEN {
            return Err(DecodeError::Truncated);
        }
        check_magic_and_version(bytes, EVENT_MAGIC, EVENT_VERSION)?;
        if bytes[5..8] != [0, 0, 0] {
            return Err(DecodeError::InvalidReserved);
        }
        let tag_count = read_u32(bytes, TAG_COUNT_OFFSET)?;
        let tag_section_len = read_u32(bytes, TAG_SECTION_LEN_OFFSET)? as usize;
        let content_len = read_u32(bytes, CONTENT_LEN_OFFSET)? as usize;
        let content_start = EVENT_HEADER_LEN
            .checked_add(tag_section_len)
            .ok_or(DecodeError::LengthOverflow)?;
        let expected_len = content_start
            .checked_add(content_len)
            .ok_or(DecodeError::LengthOverflow)?;
        if expected_len > bytes.len() {
            return Err(DecodeError::Truncated);
        }
        if expected_len < bytes.len() {
            return Err(DecodeError::TrailingBytes);
        }
        let tag_ends_len = (tag_count as usize)
            .checked_mul(4)
            .ok_or(DecodeError::LengthOverflow)?;
        if tag_ends_len > tag_section_len {
            return Err(DecodeError::LengthOverflow);
        }
        let atom_count = if tag_count == 0 {
            0
        } else {
            read_u32(bytes, EVENT_HEADER_LEN + tag_ends_len - 4)?
        };
        let atom_refs_len = (atom_count as usize)
            .checked_mul(4)
            .ok_or(DecodeError::LengthOverflow)?;
        let atom_refs_start = EVENT_HEADER_LEN
            .checked_add(tag_ends_len)
            .ok_or(DecodeError::LengthOverflow)?;
        let arena_start = atom_refs_start
            .checked_add(atom_refs_len)
            .ok_or(DecodeError::LengthOverflow)?;
        if arena_start > content_start {
            return Err(DecodeError::LengthOverflow);
        }
        let arena_len = content_start - arena_start;

        Ok(Self {
            bytes,
            tag_count,
            atom_count,
            atom_refs_start,
            arena_start,
            arena_len,
            content_start,
            content_len,
        })
    }

    pub(crate) fn id_bytes(&self) -> &'a [u8; 32] {
        self.bytes[ID_OFFSET..ID_OFFSET + 32]
            .try_into()
            .expect("validated fixed header")
    }

    pub(crate) fn pubkey_bytes(&self) -> &'a [u8; 32] {
        self.bytes[PUBKEY_OFFSET..PUBKEY_OFFSET + 32]
            .try_into()
            .expect("validated fixed header")
    }

    pub(crate) fn signature_bytes(&self) -> &'a [u8; 64] {
        self.bytes[SIG_OFFSET..SIG_OFFSET + 64]
            .try_into()
            .expect("validated fixed header")
    }

    pub(crate) fn created_at_secs(&self) -> u64 {
        read_u64(self.bytes, CREATED_AT_OFFSET).expect("validated fixed header")
    }

    pub(crate) fn kind_u16(&self) -> u16 {
        read_u16(self.bytes, KIND_OFFSET).expect("validated fixed header")
    }

    pub(crate) fn content(&self) -> &'a str {
        std::str::from_utf8(&self.bytes[self.content_start..self.content_start + self.content_len])
            .expect("canonical event content is valid utf8")
    }

    pub(crate) fn tags(&self) -> TagsIter<'a> {
        TagsIter {
            bytes: self.bytes,
            tag_index: 0,
            tag_count: self.tag_count,
            previous_end: 0,
            atom_refs_start: self.atom_refs_start,
            arena_start: self.arena_start,
            arena_len: self.arena_len,
        }
    }

    #[cfg(test)]
    pub(crate) fn matches_filter(&self, filter: &Filter) -> bool {
        self.matches_prepared_filter_after_index(&PreparedFilter::new(filter), IndexedMatch::None)
    }

    pub(crate) fn matches_prepared_filter_after_index(
        &self,
        prepared: &PreparedFilter<'_>,
        indexed: IndexedMatch,
    ) -> bool {
        let filter = prepared.filter;
        if filter.ids.as_ref().is_some_and(|ids| {
            !ids.is_empty() && !ids.iter().any(|id| id.as_bytes() == self.id_bytes())
        }) {
            return false;
        }
        if !matches!(indexed, IndexedMatch::Author | IndexedMatch::AuthorKind)
            && filter.authors.as_ref().is_some_and(|authors| {
                !authors.is_empty()
                    && !authors
                        .iter()
                        .any(|author| author.as_bytes() == self.pubkey_bytes())
            })
        {
            return false;
        }
        if !matches!(indexed, IndexedMatch::Kind | IndexedMatch::AuthorKind)
            && filter.kinds.as_ref().is_some_and(|kinds| {
                !kinds.is_empty() && !kinds.iter().any(|kind| kind.as_u16() == self.kind_u16())
            })
        {
            return false;
        }
        let created_at = self.created_at_secs();
        if filter
            .since
            .is_some_and(|since| created_at < since.as_secs())
            || filter
                .until
                .is_some_and(|until| created_at > until.as_secs())
        {
            return false;
        }
        for (name, wanted) in &prepared.generic_tags {
            if matches!(indexed, IndexedMatch::Tag(indexed_name) if indexed_name == *name) {
                continue;
            }
            if !self.tags().any(|tag| {
                let mut elements = tag.elements();
                let Some(tag_name) = elements.next() else {
                    return false;
                };
                let Some(value) = elements.next() else {
                    return false;
                };
                tag_name.is_single_ascii(name.as_char() as u8) && wanted.matches(value)
            }) {
                return false;
            }
        }
        if let Some(query) = &filter.search {
            let query = query.as_bytes();
            if !query.is_empty()
                && !self
                    .content()
                    .as_bytes()
                    .windows(query.len())
                    .any(|window| window.eq_ignore_ascii_case(query))
            {
                return false;
            }
        }
        true
    }

    pub(crate) fn materialize_event(&self) -> Result<Event, DecodeError> {
        let mut tags = Vec::with_capacity(self.tag_count as usize);
        for tag in self.tags() {
            let elements: Vec<String> = tag.elements().map(AtomRef::to_owned_text).collect();
            tags.push(Tag::parse(elements).map_err(|_| DecodeError::InvalidTag)?);
        }
        Ok(Event::new(
            EventId::from_byte_array(*self.id_bytes()),
            PublicKey::from_byte_array(*self.pubkey_bytes()),
            Timestamp::from(self.created_at_secs()),
            Kind::from(self.kind_u16()),
            Tags::from_list(tags),
            self.content(),
            Signature::from_slice(self.signature_bytes())
                .map_err(|_| DecodeError::InvalidSignature)?,
        ))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TagRef<'a> {
    bytes: &'a [u8],
    atom_start: u32,
    atom_end: u32,
    atom_refs_start: usize,
    arena_start: usize,
    arena_len: usize,
}

impl<'a> TagRef<'a> {
    pub(crate) fn elements(&self) -> TagElementsIter<'a> {
        TagElementsIter {
            bytes: self.bytes,
            atom_index: self.atom_start,
            atom_end: self.atom_end,
            atom_refs_start: self.atom_refs_start,
            arena_start: self.arena_start,
            arena_len: self.arena_len,
        }
    }
}

pub(crate) struct TagsIter<'a> {
    bytes: &'a [u8],
    tag_index: u32,
    tag_count: u32,
    previous_end: u32,
    atom_refs_start: usize,
    arena_start: usize,
    arena_len: usize,
}

impl<'a> Iterator for TagsIter<'a> {
    type Item = TagRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.tag_index == self.tag_count {
            return None;
        }
        let end = read_u32(self.bytes, EVENT_HEADER_LEN + self.tag_index as usize * 4)
            .expect("validated tag directory");
        let tag = TagRef {
            bytes: self.bytes,
            atom_start: self.previous_end,
            atom_end: end,
            atom_refs_start: self.atom_refs_start,
            arena_start: self.arena_start,
            arena_len: self.arena_len,
        };
        self.previous_end = end;
        self.tag_index += 1;
        Some(tag)
    }
}

pub(crate) struct TagElementsIter<'a> {
    bytes: &'a [u8],
    atom_index: u32,
    atom_end: u32,
    atom_refs_start: usize,
    arena_start: usize,
    arena_len: usize,
}

impl<'a> Iterator for TagElementsIter<'a> {
    type Item = AtomRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.atom_index == self.atom_end {
            return None;
        }
        let offset = self.atom_refs_start + self.atom_index as usize * 4;
        let descriptor: &[u8; 4] = self.bytes[offset..offset + 4]
            .try_into()
            .expect("validated atom directory");
        let arena = &self.bytes[self.arena_start..self.arena_start + self.arena_len];
        let (atom, _cell) = decode_atom(descriptor, arena).expect("validated atom descriptor");
        self.atom_index += 1;
        Some(atom)
    }
}

#[cfg(test)]
mod tests {
    use nostr::filter::MatchEventOptions;
    use nostr::{Alphabet, EventBuilder, Keys, SingleLetterTag};

    use super::*;

    fn fixture() -> StoredEvent {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::from(9u16), "Hello Binary World")
            .tags([
                Tag::parse(["h", "room-a"]).unwrap(),
                Tag::parse(["p", &keys.public_key().to_hex(), "wss://hint.example"]).unwrap(),
            ])
            .custom_created_at(Timestamp::from(123u64))
            .sign_with_keys(&keys)
            .unwrap();
        StoredEvent {
            event,
            provenance: Provenance {
                seen: BTreeMap::from([
                    (
                        RelayUrl::parse("wss://a.example").unwrap(),
                        Timestamp::from(200u64),
                    ),
                    (
                        RelayUrl::parse("wss://b.example/path").unwrap(),
                        Timestamp::from(201u64),
                    ),
                ]),
                local: Some(LocalOrigin {
                    owners: BTreeSet::from([IntentId(3), IntentId(8)]),
                    sig_state: SigState::Signed,
                }),
            },
        }
    }

    #[test]
    fn split_event_and_provenance_round_trips_are_exact() {
        let expected = fixture();
        let event_bytes = encode_event(&expected.event).unwrap();
        let provenance_bytes = encode_provenance(&expected.provenance).unwrap();

        assert_eq!(&event_bytes[..4], EVENT_MAGIC);
        assert_eq!(&provenance_bytes[..4], PROVENANCE_MAGIC);
        assert_eq!(event_bytes[4], EVENT_VERSION);
        assert_eq!(provenance_bytes[4], SIDECAR_VERSION);
        assert_eq!(
            StoredEventView::parse(&event_bytes)
                .unwrap()
                .materialize_event()
                .unwrap(),
            expected.event
        );
        assert_eq!(
            decode_provenance(&provenance_bytes).unwrap(),
            expected.provenance
        );
    }

    #[test]
    fn packed_atom_kinds_preserve_exact_tag_text() {
        let keys = Keys::generate();
        let identity = keys.public_key().to_hex();
        let uppercase_identity = identity.to_uppercase();
        let long = "z".repeat(128);
        let event = EventBuilder::new(Kind::from(9u16), "")
            .tag(
                Tag::parse(vec![
                    "x".to_owned(),
                    String::new(),
                    "ab".to_owned(),
                    "€".to_owned(),
                    identity.clone(),
                    uppercase_identity.clone(),
                    long.clone(),
                ])
                .unwrap(),
            )
            .sign_with_keys(&keys)
            .unwrap();
        let encoded = encode_event(&event).unwrap();
        let view = StoredEventView::parse(&encoded).unwrap();
        let atoms: Vec<_> = view.tags().next().unwrap().elements().collect();

        assert_eq!(atoms[0], AtomRef::Text("x"));
        assert_eq!(atoms[1], AtomRef::Text(""));
        assert_eq!(atoms[2], AtomRef::Text("ab"));
        assert_eq!(atoms[3], AtomRef::Text("€"));
        assert_eq!(atoms[4], AtomRef::Raw32(&decode_hex_32(&identity).unwrap()));
        assert_eq!(atoms[5], AtomRef::Text(&uppercase_identity));
        assert_eq!(atoms[6], AtomRef::Text(&long));
        assert_eq!(view.materialize_event().unwrap(), event);

        let descriptor_words: Vec<_> = (0..atoms.len())
            .map(|index| read_u32(&encoded, view.atom_refs_start + index * 4).unwrap())
            .collect();
        assert!(descriptor_words[..4]
            .iter()
            .all(|word| word & ATOM_KIND_MASK == ATOM_INLINE));
        assert_eq!(descriptor_words[4] & ATOM_KIND_MASK, ATOM_RAW32);
        assert!(descriptor_words[5..]
            .iter()
            .all(|word| word & ATOM_KIND_MASK == ATOM_UTF8));
    }

    #[test]
    fn packed_atom_parser_rejects_aliases_and_noncanonical_directories() {
        let keys = Keys::generate();
        let ordinary = EventBuilder::new(Kind::from(9u16), "")
            .tag(Tag::parse(["h", "abcd"]).unwrap())
            .sign_with_keys(&keys)
            .unwrap();

        let mut short_utf8_alias = encode_event(&ordinary).unwrap();
        let arena_start = StoredEventView::parse(&short_utf8_alias)
            .unwrap()
            .arena_start;
        short_utf8_alias[arena_start] = 3;
        short_utf8_alias.pop();
        let section_len = read_u32(&short_utf8_alias, TAG_SECTION_LEN_OFFSET).unwrap() - 1;
        short_utf8_alias[TAG_SECTION_LEN_OFFSET..TAG_SECTION_LEN_OFFSET + 4]
            .copy_from_slice(&section_len.to_be_bytes());
        assert_eq!(
            StoredEventView::parse(&short_utf8_alias).unwrap_err(),
            DecodeError::InvalidAtom
        );

        let mut noncanonical_leb = encode_event(&ordinary).unwrap();
        let arena_start = StoredEventView::parse(&noncanonical_leb)
            .unwrap()
            .arena_start;
        noncanonical_leb.insert(arena_start, 0x84);
        noncanonical_leb[arena_start + 1] = 0;
        let section_len = read_u32(&noncanonical_leb, TAG_SECTION_LEN_OFFSET).unwrap() + 1;
        noncanonical_leb[TAG_SECTION_LEN_OFFSET..TAG_SECTION_LEN_OFFSET + 4]
            .copy_from_slice(&section_len.to_be_bytes());
        assert_eq!(
            StoredEventView::parse(&noncanonical_leb).unwrap_err(),
            DecodeError::NonCanonicalLength
        );

        let identity = keys.public_key().to_hex();
        let uppercase_identity = identity.to_uppercase();
        let uppercase = EventBuilder::new(Kind::from(9u16), "")
            .tag(Tag::parse(["p", uppercase_identity.as_str()]).unwrap())
            .sign_with_keys(&keys)
            .unwrap();
        let mut raw32_alias = encode_event(&uppercase).unwrap();
        let arena_start = StoredEventView::parse(&raw32_alias).unwrap().arena_start;
        raw32_alias[arena_start + 1..arena_start + 65].copy_from_slice(identity.as_bytes());
        assert_eq!(
            StoredEventView::parse(&raw32_alias).unwrap_err(),
            DecodeError::InvalidAtom
        );

        let mut reserved_inline = encode_event(&ordinary).unwrap();
        let atom_refs_start = StoredEventView::parse(&reserved_inline)
            .unwrap()
            .atom_refs_start;
        reserved_inline[atom_refs_start] |= 1;
        assert_eq!(
            StoredEventView::parse(&reserved_inline).unwrap_err(),
            DecodeError::InvalidAtom
        );

        let mut empty_tag = encode_event(&ordinary).unwrap();
        empty_tag[EVENT_HEADER_LEN..EVENT_HEADER_LEN + 4].fill(0);
        assert_eq!(
            StoredEventView::parse(&empty_tag).unwrap_err(),
            DecodeError::EmptyTag
        );

        let mut unused_arena_tail = encode_event(&ordinary).unwrap();
        let content_start = StoredEventView::parse(&unused_arena_tail)
            .unwrap()
            .content_start;
        unused_arena_tail.insert(content_start, 0);
        let section_len = read_u32(&unused_arena_tail, TAG_SECTION_LEN_OFFSET).unwrap() + 1;
        unused_arena_tail[TAG_SECTION_LEN_OFFSET..TAG_SECTION_LEN_OFFSET + 4]
            .copy_from_slice(&section_len.to_be_bytes());
        assert_eq!(
            StoredEventView::parse(&unused_arena_tail).unwrap_err(),
            DecodeError::InvalidAtom
        );

        let two_tags = EventBuilder::new(Kind::from(9u16), "")
            .tags([
                Tag::parse(["h", "abcd"]).unwrap(),
                Tag::parse(["p", "efgh"]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .unwrap();
        let mut nonmonotone_ends = encode_event(&two_tags).unwrap();
        nonmonotone_ends[EVENT_HEADER_LEN..EVENT_HEADER_LEN + 4]
            .copy_from_slice(&5u32.to_be_bytes());
        assert_eq!(
            StoredEventView::parse(&nonmonotone_ends).unwrap_err(),
            DecodeError::LengthOverflow
        );

        let mut reserved_kind = encode_event(&ordinary).unwrap();
        let atom_refs_start = StoredEventView::parse(&reserved_kind)
            .unwrap()
            .atom_refs_start;
        reserved_kind[atom_refs_start] |= 0xc0;
        assert_eq!(
            StoredEventView::parse(&reserved_kind).unwrap_err(),
            DecodeError::InvalidAtom
        );

        let with_content = EventBuilder::new(Kind::from(9u16), "x")
            .sign_with_keys(&keys)
            .unwrap();
        let mut invalid_content = encode_event(&with_content).unwrap();
        let content_start = StoredEventView::parse(&invalid_content)
            .unwrap()
            .content_start;
        invalid_content[content_start] = 0xff;
        assert_eq!(
            StoredEventView::parse(&invalid_content).unwrap_err(),
            DecodeError::InvalidUtf8
        );
    }

    #[test]
    fn tag_heavy_nip29_event_is_more_than_thirty_percent_smaller_than_v3() {
        let keys = Keys::generate();
        let mut tags = vec![Tag::parse(["h", "room-123456789012345"]).unwrap()];
        for _ in 0..8 {
            let raw = Keys::generate().public_key().to_hex();
            tags.push(Tag::parse(["p", raw.as_str()]).unwrap());
            tags.push(Tag::parse(["e", raw.as_str()]).unwrap());
        }
        let event = EventBuilder::new(Kind::from(9u16), "x".repeat(64))
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap();
        let v3_len = EVENT_HEADER_LEN
            + event
                .tags
                .iter()
                .map(|tag| {
                    4 + tag
                        .as_slice()
                        .iter()
                        .map(|element| 4 + element.len())
                        .sum::<usize>()
                })
                .sum::<usize>()
            + event.content.len();
        let encoded = encode_event(&event).unwrap();
        assert_eq!(v3_len, 1_487);
        assert_eq!(encoded.len(), 959);
        assert!(encoded.len() * 100 < v3_len * 70);
        assert_eq!(
            StoredEventView::parse(&encoded)
                .unwrap()
                .materialize_event()
                .unwrap(),
            event
        );
    }

    #[test]
    fn packed_arena_property_roundtrips_and_mutations_never_panic() {
        fn assert_canonical_if_materializable(bytes: &[u8]) {
            let Ok(view) = StoredEventView::parse(bytes) else {
                return;
            };
            let Ok(event) = view.materialize_event() else {
                return;
            };
            assert_eq!(encode_event(&event).unwrap(), bytes);
        }

        fn next(state: &mut u64) -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        }

        fn ascii(state: &mut u64, len: usize) -> String {
            (0..len)
                .map(|_| (b'a' + (next(state) % 26) as u8) as char)
                .collect()
        }

        fn raw32(state: &mut u64) -> String {
            let mut raw = [0u8; 32];
            for byte in &mut raw {
                *byte = next(state) as u8;
            }
            encode_hex_32(&raw)
        }

        let keys = Keys::generate();
        let mut state = 0x4e4d_502d_4152_454e;
        for case in 0..200u64 {
            let tag_count = (next(&mut state) % 9) as usize;
            let mut tags = Vec::with_capacity(tag_count);
            for tag_index in 0..tag_count {
                let mut elements = vec!["x".to_owned(), format!("case-{case}-tag-{tag_index}")];
                for _ in 0..next(&mut state) % 6 {
                    let element = match next(&mut state) % 8 {
                        0 => String::new(),
                        1 => ascii(&mut state, 1),
                        2 => ascii(&mut state, 2),
                        3 => "€".to_owned(),
                        4 => raw32(&mut state),
                        5 => raw32(&mut state).to_uppercase(),
                        6 => {
                            let len = 4 + (next(&mut state) % 60) as usize;
                            ascii(&mut state, len)
                        }
                        _ => {
                            let len = 128 + (next(&mut state) % 40) as usize;
                            ascii(&mut state, len)
                        }
                    };
                    elements.push(element);
                }
                tags.push(Tag::parse(elements).unwrap());
            }
            let content_len = (next(&mut state) % 96) as usize;
            let event = EventBuilder::new(Kind::from(9u16), ascii(&mut state, content_len))
                .tags(tags)
                .custom_created_at(Timestamp::from(10_000 + case))
                .sign_with_keys(&keys)
                .unwrap();
            let encoded = encode_event(&event).unwrap();
            let view = StoredEventView::parse(&encoded).unwrap();
            assert_eq!(view.materialize_event().unwrap(), event);
            assert_eq!(
                encode_event(&view.materialize_event().unwrap()).unwrap(),
                encoded
            );

            for _ in 0..16 {
                let mut mutated = encoded.clone();
                let index = (next(&mut state) as usize) % mutated.len();
                let bit = 1u8 << (next(&mut state) % 8);
                mutated[index] ^= bit;
                let outcome =
                    std::panic::catch_unwind(|| assert_canonical_if_materializable(&mutated));
                assert!(outcome.is_ok(), "mutation panicked at byte {index}");
            }
        }
    }

    #[test]
    #[ignore = "requires NMP_CORPUS real-event JSONL"]
    fn real_corpus_codec_cost_matrix() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        use nostr::JsonUtil;

        let path = std::env::var("NMP_CORPUS").expect("set NMP_CORPUS to event JSONL");
        let source = std::fs::read_to_string(path).unwrap();
        let events: Vec<Event> = source
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| Event::from_json(line).unwrap())
            .collect();
        let mut encoded = Vec::with_capacity(events.len());
        for event in &events {
            encoded.push(encode_event(event).unwrap());
        }

        let mut encode_elapsed = Duration::ZERO;
        let mut materialize_elapsed = Duration::ZERO;
        for _ in 0..50 {
            let started = Instant::now();
            for event in &events {
                black_box(encode_event(event).unwrap());
            }
            encode_elapsed += started.elapsed();

            let started = Instant::now();
            for bytes in &encoded {
                black_box(
                    StoredEventView::from_trusted(bytes)
                        .unwrap()
                        .materialize_event()
                        .unwrap(),
                );
            }
            materialize_elapsed += started.elapsed();
        }
        println!("events={}", events.len());
        println!(
            "encode_batch_mean_ms={:.3}",
            encode_elapsed.as_secs_f64() * 20.0
        );
        println!(
            "materialize_batch_mean_ms={:.3}",
            materialize_elapsed.as_secs_f64() * 20.0
        );
    }

    #[test]
    fn provenance_codec_preserves_absent_and_empty_local_state() {
        let without_local = Provenance {
            seen: BTreeMap::new(),
            local: None,
        };
        let empty_pending_local = Provenance {
            seen: BTreeMap::new(),
            local: Some(LocalOrigin {
                owners: BTreeSet::new(),
                sig_state: SigState::Pending,
            }),
        };

        let without_local_bytes = encode_provenance(&without_local).unwrap();
        let empty_pending_local_bytes = encode_provenance(&empty_pending_local).unwrap();
        assert_eq!(without_local_bytes.len(), PROVENANCE_HEADER_LEN);
        assert_eq!(without_local_bytes[5], 0);
        assert_eq!(empty_pending_local_bytes[5], FLAG_LOCAL);
        assert_eq!(
            decode_provenance(&without_local_bytes).unwrap(),
            without_local
        );
        assert_eq!(
            decode_provenance(&empty_pending_local_bytes).unwrap(),
            empty_pending_local
        );
    }

    #[test]
    fn local_codec_round_trips_both_states_with_sorted_unique_owners() {
        for sig_state in [SigState::Pending, SigState::Signed] {
            let expected = LocalOrigin {
                owners: BTreeSet::from([IntentId(8), IntentId(1), IntentId(3)]),
                sig_state,
            };
            let encoded = encode_local(&expected).unwrap();

            assert_eq!(&encoded[..4], LOCAL_MAGIC);
            assert_eq!(encoded[4], SIDECAR_VERSION);
            assert_eq!(encoded[6..8], [0, 0]);
            assert_eq!(read_u32(&encoded, LOCAL_OWNER_COUNT_OFFSET).unwrap(), 3);
            let encoded_owners: Vec<u64> = encoded[LOCAL_HEADER_LEN..]
                .chunks(8)
                .map(|bytes| u64::from_be_bytes(bytes.try_into().unwrap()))
                .collect();
            assert_eq!(encoded_owners, [1, 3, 8]);
            assert_eq!(decode_local(&encoded).unwrap(), expected);
        }
    }

    #[test]
    fn local_codec_preserves_some_with_an_empty_owner_set() {
        let expected = LocalOrigin {
            owners: BTreeSet::new(),
            sig_state: SigState::Pending,
        };
        let encoded = encode_local(&expected).unwrap();
        assert_eq!(encoded.len(), LOCAL_HEADER_LEN);
        assert_eq!(read_u32(&encoded, LOCAL_OWNER_COUNT_OFFSET).unwrap(), 0);
        assert_eq!(decode_local(&encoded).unwrap(), expected);
    }

    #[test]
    fn local_codec_rejects_malformed_and_noncanonical_values() {
        let expected = LocalOrigin {
            owners: BTreeSet::from([IntentId(3), IntentId(8)]),
            sig_state: SigState::Signed,
        };

        let mut invalid_magic = encode_local(&expected).unwrap();
        invalid_magic[0] = b'X';
        assert_eq!(
            decode_local(&invalid_magic).unwrap_err(),
            DecodeError::BadMagic
        );

        let mut invalid_version = encode_local(&expected).unwrap();
        invalid_version[4] = SIDECAR_VERSION.wrapping_add(1);
        assert_eq!(
            decode_local(&invalid_version).unwrap_err(),
            DecodeError::UnsupportedVersion(SIDECAR_VERSION.wrapping_add(1))
        );

        let mut invalid_state = encode_local(&expected).unwrap();
        invalid_state[5] = 2;
        assert_eq!(
            decode_local(&invalid_state).unwrap_err(),
            DecodeError::InvalidLocalState(2)
        );

        let mut invalid_reserved = encode_local(&expected).unwrap();
        invalid_reserved[6] = 1;
        assert_eq!(
            decode_local(&invalid_reserved).unwrap_err(),
            DecodeError::InvalidReserved
        );

        let mut duplicate = encode_local(&expected).unwrap();
        duplicate[LOCAL_HEADER_LEN + 8..LOCAL_HEADER_LEN + 16].copy_from_slice(&3u64.to_be_bytes());
        assert_eq!(
            decode_local(&duplicate).unwrap_err(),
            DecodeError::DuplicateOwner
        );

        let mut descending = encode_local(&expected).unwrap();
        descending[LOCAL_HEADER_LEN..LOCAL_HEADER_LEN + 8].copy_from_slice(&8u64.to_be_bytes());
        descending[LOCAL_HEADER_LEN + 8..LOCAL_HEADER_LEN + 16]
            .copy_from_slice(&3u64.to_be_bytes());
        assert_eq!(
            decode_local(&descending).unwrap_err(),
            DecodeError::InvalidOwnerOrder
        );

        let mut trailing = encode_local(&expected).unwrap();
        trailing.push(0);
        assert_eq!(
            decode_local(&trailing).unwrap_err(),
            DecodeError::TrailingBytes
        );

        let mut oversized_count = encode_local(&expected).unwrap();
        oversized_count[LOCAL_OWNER_COUNT_OFFSET..LOCAL_OWNER_COUNT_OFFSET + 4]
            .copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_local(&oversized_count).is_err());
        assert_eq!(
            checked_local_len(usize::MAX).unwrap_err(),
            DecodeError::LengthOverflow
        );
    }

    #[test]
    fn every_local_codec_prefix_is_rejected_as_truncated() {
        let local = LocalOrigin {
            owners: BTreeSet::from([IntentId(1), IntentId(2), IntentId(3)]),
            sig_state: SigState::Signed,
        };
        let encoded = encode_local(&local).unwrap();
        for len in 0..encoded.len() {
            assert_eq!(
                decode_local(&encoded[..len]).unwrap_err(),
                DecodeError::Truncated,
                "local-state prefix {len} unexpectedly parsed"
            );
        }
    }

    #[test]
    fn composite_displaced_row_round_trip_is_exact() {
        let expected = fixture();
        let encoded = encode(&expected).unwrap();
        assert_eq!(&encoded[..4], COMPOSITE_MAGIC);
        assert_eq!(encoded[4], COMPOSITE_VERSION);
        assert_eq!(decode(&encoded).unwrap(), expected);
    }

    #[test]
    fn changing_provenance_does_not_change_immutable_event_bytes() {
        let mut stored = fixture();
        let before = encode_event(&stored.event).unwrap();
        stored.provenance.seen.insert(
            RelayUrl::parse("wss://c.example").unwrap(),
            Timestamp::from(300u64),
        );
        stored
            .provenance
            .local
            .as_mut()
            .unwrap()
            .owners
            .insert(IntentId(13));
        assert_eq!(encode_event(&stored.event).unwrap(), before);
    }

    #[test]
    fn borrowed_filter_matching_reads_fields_tags_and_content_without_materializing() {
        let stored = fixture();
        let encoded = encode_event(&stored.event).unwrap();
        let view = StoredEventView::parse(&encoded).unwrap();
        let matching = Filter::new()
            .id(stored.event.id)
            .author(stored.event.pubkey)
            .kind(Kind::from(9u16))
            .since(Timestamp::from(100u64))
            .until(Timestamp::from(150u64))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "room-a")
            .search("binary");
        assert!(view.matches_filter(&matching));
        assert!(!view.matches_filter(
            &Filter::new().custom_tag(SingleLetterTag::lowercase(Alphabet::H), "different-room",)
        ));
        assert!(!view.matches_filter(&Filter::new().search("missing")));
    }

    #[test]
    fn borrowed_matching_is_equivalent_to_nostr_filter_matching() {
        let stored = fixture();
        let other = Keys::generate();
        let encoded = encode_event(&stored.event).unwrap();
        let view = StoredEventView::parse(&encoded).unwrap();
        let mut empty_sets = Filter::new();
        empty_sets.ids = Some(BTreeSet::new());
        empty_sets.authors = Some(BTreeSet::new());
        empty_sets.kinds = Some(BTreeSet::new());
        let filters = [
            Filter::new(),
            empty_sets,
            Filter::new().id(stored.event.id),
            Filter::new().author(stored.event.pubkey),
            Filter::new().author(other.public_key()),
            Filter::new().kind(Kind::from(9u16)),
            Filter::new().kind(Kind::TextNote),
            Filter::new().since(Timestamp::from(124u64)),
            Filter::new().until(Timestamp::from(122u64)),
            Filter::new().custom_tags(
                SingleLetterTag::lowercase(Alphabet::H),
                ["room-a", "room-b"],
            ),
            Filter::new()
                .custom_tag(SingleLetterTag::lowercase(Alphabet::H), "room-a")
                .custom_tag(
                    SingleLetterTag::lowercase(Alphabet::P),
                    stored.event.pubkey.to_hex(),
                ),
            Filter::new().search("BINARY world"),
            Filter::new().search("not present"),
        ];
        for filter in filters {
            assert_eq!(
                view.matches_filter(&filter),
                filter.match_event(&stored.event, MatchEventOptions::new()),
                "borrowed matcher diverged for {filter:?}"
            );
        }
    }

    #[test]
    fn malformed_truncation_fails_closed_for_every_codec() {
        let stored = fixture();
        let event = encode_event(&stored.event).unwrap();
        let provenance = encode_provenance(&stored.provenance).unwrap();
        let composite = encode(&stored).unwrap();

        for len in 0..event.len() {
            assert!(
                StoredEventView::parse(&event[..len]).is_err(),
                "event prefix {len} unexpectedly parsed"
            );
        }
        for len in 0..provenance.len() {
            assert!(
                decode_provenance(&provenance[..len]).is_err(),
                "provenance prefix {len} unexpectedly parsed"
            );
        }
        for len in 0..composite.len() {
            assert!(
                decode(&composite[..len]).is_err(),
                "composite prefix {len} unexpectedly parsed"
            );
        }
    }

    #[test]
    fn every_codec_rejects_trailing_bytes_and_reserved_bits() {
        let stored = fixture();
        let mut event = encode_event(&stored.event).unwrap();
        event.push(0);
        assert_eq!(
            StoredEventView::parse(&event).unwrap_err(),
            DecodeError::TrailingBytes
        );

        let mut provenance = encode_provenance(&stored.provenance).unwrap();
        provenance.push(0);
        assert_eq!(
            decode_provenance(&provenance).unwrap_err(),
            DecodeError::TrailingBytes
        );

        let mut composite = encode(&stored).unwrap();
        composite.push(0);
        assert_eq!(decode(&composite).unwrap_err(), DecodeError::TrailingBytes);

        let mut reserved = encode_event(&stored.event).unwrap();
        reserved[5] = 1;
        assert_eq!(
            StoredEventView::parse(&reserved).unwrap_err(),
            DecodeError::InvalidReserved
        );
    }
}
