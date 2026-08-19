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

/// Decode a `LocalOrigin` owner set: `count` big-endian `u64` ids, which must
/// already be in canonical set form (the form both `encode_local` and
/// `encode_provenance` produce from a `BTreeSet`) — strictly ascending, no
/// repeats. This is the only decoder for that invariant; every envelope that
/// embeds an owner set, standalone or displaced, calls this rather than
/// re-checking order on its own.
fn decode_owners(
    bytes: &[u8],
    cursor: &mut usize,
    count: usize,
) -> Result<BTreeSet<IntentId>, DecodeError> {
    let mut owners = BTreeSet::new();
    let mut previous_owner = None;
    for _ in 0..count {
        let owner = IntentId(take_u64(bytes, cursor)?);
        if let Some(previous) = previous_owner {
            if owner.0 == previous {
                return Err(DecodeError::DuplicateOwner);
            }
            if owner.0 < previous {
                return Err(DecodeError::InvalidOwnerOrder);
            }
        }
        owners.insert(owner);
        previous_owner = Some(owner.0);
    }
    Ok(owners)
}

/// Decode one canonical local-state envelope. Owner ids must already be in
/// canonical set form: repeated or out-of-order ids are rejected rather than
/// silently folded or reordered.
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
    let owners = decode_owners(bytes, &mut cursor, owner_count)?;
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
        let owners = decode_owners(bytes, &mut cursor, owner_count as usize)?;
        Some(LocalOrigin { owners, sig_state })
    } else {
        None
    };

    if cursor != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(Provenance { seen, local })
}

/// Preserve the self-contained encoding used by displaced delivery rows while
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

/// Decode the self-contained composite used by displaced delivery rows.
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

    /// `from_trusted` only checks that the content byte range lies inside
    /// the row; it never decodes those bytes. A corrupt-but-length-consistent
    /// row can carry non-UTF-8 content here, so this door reports that as a
    /// typed error instead of trusting the byte range's validity.
    pub(crate) fn content(&self) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(&self.bytes[self.content_start..self.content_start + self.content_len])
            .map_err(|_| DecodeError::InvalidUtf8)
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

    /// `from_trusted` validates only the fixed header and the direct
    /// content/tag-section byte ranges; it never walks the per-tag directory
    /// or decodes atom descriptors. A corrupt-but-length-consistent row can
    /// therefore carry a tag-end that does not correspond to a real atom
    /// boundary, so every step here that walks tags or content is fallible
    /// and this returns `Result` rather than trusting the row.
    pub(crate) fn matches_prepared_filter_after_index(
        &self,
        prepared: &PreparedFilter<'_>,
        indexed: IndexedMatch,
    ) -> Result<bool, DecodeError> {
        let filter = prepared.filter;
        if filter.ids.as_ref().is_some_and(|ids| {
            !ids.is_empty() && !ids.iter().any(|id| id.as_bytes() == self.id_bytes())
        }) {
            return Ok(false);
        }
        if !matches!(indexed, IndexedMatch::Author)
            && filter.authors.as_ref().is_some_and(|authors| {
                !authors.is_empty()
                    && !authors
                        .iter()
                        .any(|author| author.as_bytes() == self.pubkey_bytes())
            })
        {
            return Ok(false);
        }
        if !matches!(indexed, IndexedMatch::Kind)
            && filter.kinds.as_ref().is_some_and(|kinds| {
                !kinds.is_empty() && !kinds.iter().any(|kind| kind.as_u16() == self.kind_u16())
            })
        {
            return Ok(false);
        }
        let created_at = self.created_at_secs();
        if filter
            .since
            .is_some_and(|since| created_at < since.as_secs())
            || filter
                .until
                .is_some_and(|until| created_at > until.as_secs())
        {
            return Ok(false);
        }
        for (name, wanted) in &prepared.generic_tags {
            if matches!(indexed, IndexedMatch::Tag(indexed_name) if indexed_name == *name) {
                continue;
            }
            let mut any_match = false;
            for tag in self.tags() {
                let mut elements = tag.elements();
                let Some(tag_name) = elements.next().transpose()? else {
                    continue;
                };
                let Some(value) = elements.next().transpose()? else {
                    continue;
                };
                if tag_name.is_single_ascii(name.as_char() as u8) && wanted.matches(value) {
                    any_match = true;
                    break;
                }
            }
            if !any_match {
                return Ok(false);
            }
        }
        if let Some(query) = &filter.search {
            let query = query.as_bytes();
            if !query.is_empty() {
                let content = self.content()?;
                if !content
                    .as_bytes()
                    .windows(query.len())
                    .any(|window| window.eq_ignore_ascii_case(query))
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    pub(crate) fn materialize_event(&self) -> Result<Event, DecodeError> {
        let mut tags = Vec::with_capacity(self.tag_count as usize);
        for tag in self.tags() {
            let elements: Vec<String> = tag
                .elements()
                .map(|element| element.map(AtomRef::to_owned_text))
                .collect::<Result<Vec<_>, DecodeError>>()?;
            tags.push(Tag::parse(elements).map_err(|_| DecodeError::InvalidTag)?);
        }
        Ok(Event::new(
            EventId::from_byte_array(*self.id_bytes()),
            PublicKey::from_byte_array(*self.pubkey_bytes()),
            Timestamp::from(self.created_at_secs()),
            Kind::from(self.kind_u16()),
            Tags::from_list(tags),
            self.content()?,
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
    type Item = Result<AtomRef<'a>, DecodeError>;

    /// `atom_end` comes from a tag-end that `from_trusted` never validates
    /// against the real atom directory, so both the directory read and the
    /// arena decode below are treated as untrusted input: an out-of-range
    /// offset becomes a typed `Err` instead of an out-of-bounds panic, and a
    /// malformed descriptor's `Err` is returned rather than `.expect`ed
    /// away. Either error halts this tag's iteration so a bogus `atom_end`
    /// cannot keep walking unrelated bytes as if they were atoms.
    fn next(&mut self) -> Option<Self::Item> {
        if self.atom_index == self.atom_end {
            return None;
        }
        let descriptor = (self.atom_index as usize)
            .checked_mul(4)
            .and_then(|delta| self.atom_refs_start.checked_add(delta))
            .and_then(|offset| offset.checked_add(4).map(|end| (offset, end)))
            .and_then(|(offset, end)| self.bytes.get(offset..end));
        let Some(descriptor) = descriptor else {
            self.atom_index = self.atom_end;
            return Some(Err(DecodeError::Truncated));
        };
        let descriptor: &[u8; 4] = descriptor.try_into().expect("length checked");
        self.atom_index += 1;
        let arena = &self.bytes[self.arena_start..self.arena_start + self.arena_len];
        match decode_atom(descriptor, arena) {
            Ok((atom, _cell)) => Some(Ok(atom)),
            Err(error) => {
                self.atom_index = self.atom_end;
                Some(Err(error))
            }
        }
    }
}

