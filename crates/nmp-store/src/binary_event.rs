//! Portable binary codecs for canonical persisted event state.
//!
//! Immutable signed NIP-01 event bytes, optional canonical local state, and
//! self-contained displaced-row provenance deliberately have distinct
//! envelopes. Canonical relay observations are fixed-width redb rows outside
//! this codec, while queries borrow fixed fields, tags, and content directly
//! from the immutable event value.
//! All integers are big-endian and every parser uses checked slices; unlike
//! nostrdb's packed native struct, this is alignment-safe and endian-stable on
//! every Rust target, including wasm.

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
const VERSION: u8 = 3;
const FLAG_LOCAL: u8 = 1;

// magic + version + reserved + id + pubkey + sig + created_at + kind
// + tag_count + encoded_tag_bytes + content_bytes
const EVENT_HEADER_LEN: usize = 158;
const ID_OFFSET: usize = 8;
const PUBKEY_OFFSET: usize = ID_OFFSET + 32;
const SIG_OFFSET: usize = PUBKEY_OFFSET + 32;
const CREATED_AT_OFFSET: usize = SIG_OFFSET + 64;
const KIND_OFFSET: usize = CREATED_AT_OFFSET + 8;
const TAG_COUNT_OFFSET: usize = KIND_OFFSET + 2;
const TAG_BYTES_LEN_OFFSET: usize = TAG_COUNT_OFFSET + 4;
const CONTENT_LEN_OFFSET: usize = TAG_BYTES_LEN_OFFSET + 4;

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

fn check_magic_and_version(bytes: &[u8], magic: &[u8; 4]) -> Result<(), DecodeError> {
    if bytes.get(..4) != Some(magic) {
        return Err(if bytes.len() < 4 {
            DecodeError::Truncated
        } else {
            DecodeError::BadMagic
        });
    }
    let version = *bytes.get(4).ok_or(DecodeError::Truncated)?;
    if version != VERSION {
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

fn encoded_tags_len(tags: &[Tag]) -> Result<usize, DecodeError> {
    let mut total = 0usize;
    for tag in tags {
        checked_add_len(&mut total, 4)?;
        for element in tag.as_slice() {
            checked_add_len(&mut total, 4)?;
            checked_add_len(&mut total, element.len())?;
        }
    }
    u32::try_from(total).map_err(|_| DecodeError::LengthOverflow)?;
    Ok(total)
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
    let tag_bytes_len = encoded_tags_len(tags)?;
    let capacity = checked_capacity(EVENT_HEADER_LEN, tag_bytes_len, event.content.len())?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(EVENT_MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(event.id.as_bytes());
    out.extend_from_slice(event.pubkey.as_bytes());
    out.extend_from_slice(event.sig.as_ref());
    push_u64(&mut out, event.created_at.as_secs());
    push_u16(&mut out, event.kind.as_u16());
    push_u32(&mut out, tags.len())?;
    push_u32(&mut out, tag_bytes_len)?;
    push_u32(&mut out, event.content.len())?;
    debug_assert_eq!(out.len(), EVENT_HEADER_LEN);

    for tag in tags {
        let elements = tag.as_slice();
        push_u32(&mut out, elements.len())?;
        for element in elements {
            push_u32(&mut out, element.len())?;
            out.extend_from_slice(element.as_bytes());
        }
    }
    out.extend_from_slice(event.content.as_bytes());
    Ok(out)
}

/// Encode the canonical local-intent ownership/signature state independently
/// from relay observations. Table absence represents `Provenance::local ==
/// None`; this envelope therefore always represents `Some(LocalOrigin)`, even
/// when its owner set is empty.
pub(crate) fn encode_local(local: &LocalOrigin) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(checked_local_len(local.owners.len())?);
    out.extend_from_slice(LOCAL_MAGIC);
    out.push(VERSION);
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
    check_magic_and_version(bytes, LOCAL_MAGIC)?;
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
    out.push(VERSION);
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
    check_magic_and_version(bytes, PROVENANCE_MAGIC)?;
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
    out.push(VERSION);
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
    check_magic_and_version(bytes, COMPOSITE_MAGIC)?;
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
    tag_bytes_len: usize,
    content_start: usize,
    content_len: usize,
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

impl<'a> StoredEventView<'a> {
    /// Fully validate an immutable event envelope before exposing borrowed
    /// fields and iterators.
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        let view = Self::from_trusted(bytes)?;
        let tags_end = EVENT_HEADER_LEN
            .checked_add(view.tag_bytes_len)
            .ok_or(DecodeError::LengthOverflow)?;
        if tags_end != view.content_start {
            return Err(DecodeError::LengthOverflow);
        }

        let mut cursor = EVENT_HEADER_LEN;
        for _ in 0..view.tag_count {
            let elements = take_u32(bytes, &mut cursor)?;
            if elements == 0 {
                return Err(DecodeError::EmptyTag);
            }
            for _ in 0..elements {
                let len = take_u32(bytes, &mut cursor)? as usize;
                std::str::from_utf8(take(bytes, &mut cursor, len)?)
                    .map_err(|_| DecodeError::InvalidUtf8)?;
            }
        }
        if cursor != tags_end {
            return Err(DecodeError::LengthOverflow);
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
        check_magic_and_version(bytes, EVENT_MAGIC)?;
        if bytes[5..8] != [0, 0, 0] {
            return Err(DecodeError::InvalidReserved);
        }
        let tag_count = read_u32(bytes, TAG_COUNT_OFFSET)?;
        let tag_bytes_len = read_u32(bytes, TAG_BYTES_LEN_OFFSET)? as usize;
        let content_len = read_u32(bytes, CONTENT_LEN_OFFSET)? as usize;
        let content_start = EVENT_HEADER_LEN
            .checked_add(tag_bytes_len)
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

        Ok(Self {
            bytes,
            tag_count,
            tag_bytes_len,
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
            cursor: EVENT_HEADER_LEN,
            remaining: self.tag_count,
        }
    }

    pub(crate) fn matches_filter(&self, filter: &Filter) -> bool {
        self.matches_filter_after_index(filter, IndexedMatch::None)
    }

    pub(crate) fn matches_filter_after_index(
        &self,
        filter: &Filter,
        indexed: IndexedMatch,
    ) -> bool {
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
        for (name, wanted) in &filter.generic_tags {
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
                tag_name.len() == 1
                    && tag_name.as_bytes()[0] == name.as_char() as u8
                    && wanted.contains(value)
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
            let elements: Vec<String> = tag.elements().map(ToOwned::to_owned).collect();
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
    element_count: u32,
}

impl<'a> TagRef<'a> {
    pub(crate) fn elements(&self) -> TagElementsIter<'a> {
        TagElementsIter {
            bytes: self.bytes,
            cursor: 0,
            remaining: self.element_count,
        }
    }
}

pub(crate) struct TagsIter<'a> {
    bytes: &'a [u8],
    cursor: usize,
    remaining: u32,
}

impl<'a> Iterator for TagsIter<'a> {
    type Item = TagRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let count = take_u32(self.bytes, &mut self.cursor).expect("validated tag count");
        let start = self.cursor;
        for _ in 0..count {
            let len = take_u32(self.bytes, &mut self.cursor).expect("validated tag length");
            take(self.bytes, &mut self.cursor, len as usize).expect("validated tag bytes");
        }
        self.remaining -= 1;
        Some(TagRef {
            bytes: &self.bytes[start..self.cursor],
            element_count: count,
        })
    }
}

pub(crate) struct TagElementsIter<'a> {
    bytes: &'a [u8],
    cursor: usize,
    remaining: u32,
}

impl<'a> Iterator for TagElementsIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let len = take_u32(self.bytes, &mut self.cursor).expect("validated element length");
        let raw =
            take(self.bytes, &mut self.cursor, len as usize).expect("validated element bytes");
        self.remaining -= 1;
        Some(std::str::from_utf8(raw).expect("validated element utf8"))
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
        assert_eq!(event_bytes[4], VERSION);
        assert_eq!(provenance_bytes[4], VERSION);
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
            assert_eq!(encoded[4], VERSION);
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
        invalid_version[4] = VERSION.wrapping_add(1);
        assert_eq!(
            decode_local(&invalid_version).unwrap_err(),
            DecodeError::UnsupportedVersion(VERSION.wrapping_add(1))
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
        assert_eq!(encoded[4], VERSION);
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
