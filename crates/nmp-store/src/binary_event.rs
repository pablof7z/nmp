//! Portable binary encoding for canonical persisted event rows.
//!
//! The format is intentionally boring: fixed-width integers are big-endian,
//! byte fields have explicit lengths, and decoding uses checked slices only.
//! Unlike nostrdb's packed native struct, this stays alignment-safe and
//! endian-stable on every Rust target while still permitting borrowed field
//! and tag reads directly from a redb value guard.

use std::collections::{BTreeMap, BTreeSet};

use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, EventId, Filter, Kind, PublicKey, RelayUrl, Tag, Tags, Timestamp};

use crate::{IntentId, LocalOrigin, Provenance, SigState, StoredEvent};

const MAGIC: &[u8; 4] = b"NMPE";
const VERSION: u8 = 1;
const FLAG_LOCAL: u8 = 1;
const HEADER_LEN: usize = 166;

const ID_OFFSET: usize = 8;
const PUBKEY_OFFSET: usize = ID_OFFSET + 32;
const SIG_OFFSET: usize = PUBKEY_OFFSET + 32;
const CREATED_AT_OFFSET: usize = SIG_OFFSET + 64;
const KIND_OFFSET: usize = CREATED_AT_OFFSET + 8;
const TAG_COUNT_OFFSET: usize = KIND_OFFSET + 2;
const TAG_BYTES_LEN_OFFSET: usize = TAG_COUNT_OFFSET + 4;
const CONTENT_LEN_OFFSET: usize = TAG_BYTES_LEN_OFFSET + 4;
const SEEN_COUNT_OFFSET: usize = CONTENT_LEN_OFFSET + 4;
const OWNER_COUNT_OFFSET: usize = SEEN_COUNT_OFFSET + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u8),
    InvalidFlags(u8),
    InvalidUtf8,
    EmptyTag,
    InvalidRelay,
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
    *cursor += 4;
    Ok(value)
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, DecodeError> {
    let value = read_u64(bytes, *cursor)?;
    *cursor += 8;
    Ok(value)
}

pub(crate) fn encode(stored: &StoredEvent) -> Result<Vec<u8>, DecodeError> {
    let event = &stored.event;
    let local = stored.provenance.local.as_ref();
    let flags = if local.is_some() { FLAG_LOCAL } else { 0 };
    let tags = event.tags.as_slice();
    let owners = local.map(|local| local.owners.len()).unwrap_or(0);

    let mut out = Vec::with_capacity(HEADER_LEN + event.content.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(flags);
    push_u16(&mut out, 0);
    out.extend_from_slice(event.id.as_bytes());
    out.extend_from_slice(event.pubkey.as_bytes());
    out.extend_from_slice(event.sig.as_ref());
    push_u64(&mut out, event.created_at.as_secs());
    push_u16(&mut out, event.kind.as_u16());
    push_u32(&mut out, tags.len())?;
    push_u32(&mut out, 0)?;
    push_u32(&mut out, event.content.len())?;
    push_u32(&mut out, stored.provenance.seen.len())?;
    push_u32(&mut out, owners)?;
    debug_assert_eq!(out.len(), HEADER_LEN);

    let tags_start = out.len();
    for tag in tags {
        let elements = tag.as_slice();
        push_u32(&mut out, elements.len())?;
        for element in elements {
            push_u32(&mut out, element.len())?;
            out.extend_from_slice(element.as_bytes());
        }
    }
    let tags_len =
        u32::try_from(out.len() - tags_start).map_err(|_| DecodeError::LengthOverflow)?;
    out[TAG_BYTES_LEN_OFFSET..TAG_BYTES_LEN_OFFSET + 4].copy_from_slice(&tags_len.to_be_bytes());
    out.extend_from_slice(event.content.as_bytes());
    for (relay, at) in &stored.provenance.seen {
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct StoredEventView<'a> {
    bytes: &'a [u8],
    flags: u8,
    tag_count: u32,
    tag_bytes_len: usize,
    content_start: usize,
    content_len: usize,
    provenance_start: usize,
    seen_count: u32,
    owner_count: u32,
}

impl<'a> StoredEventView<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        let view = Self::from_trusted(bytes)?;
        let tags_end = HEADER_LEN
            .checked_add(view.tag_bytes_len)
            .ok_or(DecodeError::LengthOverflow)?;
        if tags_end != view.content_start {
            return Err(DecodeError::LengthOverflow);
        }
        let mut cursor = HEADER_LEN;
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
                .get(view.content_start..view.provenance_start)
                .ok_or(DecodeError::Truncated)?,
        )
        .map_err(|_| DecodeError::InvalidUtf8)?;
        cursor = view.provenance_start;

        for _ in 0..view.seen_count {
            let relay_len = take_u32(bytes, &mut cursor)? as usize;
            std::str::from_utf8(take(bytes, &mut cursor, relay_len)?)
                .map_err(|_| DecodeError::InvalidUtf8)?;
            take_u64(bytes, &mut cursor)?;
        }
        if view.flags & FLAG_LOCAL != 0 {
            let state = *take(bytes, &mut cursor, 1)?
                .first()
                .expect("one byte requested");
            if state > 1 {
                return Err(DecodeError::InvalidLocalState(state));
            }
            for _ in 0..view.owner_count {
                take_u64(bytes, &mut cursor)?;
            }
        }
        if cursor != bytes.len() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(view)
    }

    /// Build a borrowed view after checking only the fixed header and direct
    /// section bounds. Canonical rows were fully validated when encoded or
    /// migrated; query hot paths use this door so an author/kind predicate
    /// does not walk tags or provenance merely to reject the row.
    pub(crate) fn from_trusted(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        if bytes.len() < HEADER_LEN {
            return Err(DecodeError::Truncated);
        }
        if bytes.get(..4) != Some(MAGIC) {
            return Err(DecodeError::BadMagic);
        }
        let version = bytes[4];
        if version != VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let flags = bytes[5];
        if flags & !FLAG_LOCAL != 0 {
            return Err(DecodeError::InvalidFlags(flags));
        }
        let tag_count = read_u32(bytes, TAG_COUNT_OFFSET)?;
        let tag_bytes_len = read_u32(bytes, TAG_BYTES_LEN_OFFSET)? as usize;
        let content_len = read_u32(bytes, CONTENT_LEN_OFFSET)? as usize;
        let seen_count = read_u32(bytes, SEEN_COUNT_OFFSET)?;
        let owner_count = read_u32(bytes, OWNER_COUNT_OFFSET)?;
        if flags & FLAG_LOCAL == 0 && owner_count != 0 {
            return Err(DecodeError::InvalidFlags(flags));
        }

        let content_start = HEADER_LEN
            .checked_add(tag_bytes_len)
            .ok_or(DecodeError::LengthOverflow)?;
        let provenance_start = content_start
            .checked_add(content_len)
            .ok_or(DecodeError::LengthOverflow)?;
        if provenance_start > bytes.len() {
            return Err(DecodeError::Truncated);
        }

        Ok(Self {
            bytes,
            flags,
            tag_count,
            tag_bytes_len,
            content_start,
            content_len,
            provenance_start,
            seen_count,
            owner_count,
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
            .expect("validated utf8")
    }

    pub(crate) fn tags(&self) -> TagsIter<'a> {
        TagsIter {
            bytes: self.bytes,
            cursor: HEADER_LEN,
            remaining: self.tag_count,
        }
    }

    pub(crate) fn matches_filter(&self, filter: &Filter) -> bool {
        if filter.ids.as_ref().is_some_and(|ids| {
            !ids.is_empty() && !ids.iter().any(|id| id.as_bytes() == self.id_bytes())
        }) {
            return false;
        }
        if filter.authors.as_ref().is_some_and(|authors| {
            !authors.is_empty()
                && !authors
                    .iter()
                    .any(|author| author.as_bytes() == self.pubkey_bytes())
        }) {
            return false;
        }
        if filter.kinds.as_ref().is_some_and(|kinds| {
            !kinds.is_empty() && !kinds.iter().any(|kind| kind.as_u16() == self.kind_u16())
        }) {
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

    pub(crate) fn materialize(&self) -> Result<StoredEvent, DecodeError> {
        let mut tags = Vec::with_capacity(self.tag_count as usize);
        for tag in self.tags() {
            let elements: Vec<String> = tag.elements().map(ToOwned::to_owned).collect();
            tags.push(Tag::parse(elements).map_err(|_| DecodeError::EmptyTag)?);
        }
        let event = Event::new(
            EventId::from_byte_array(*self.id_bytes()),
            PublicKey::from_byte_array(*self.pubkey_bytes()),
            Timestamp::from(self.created_at_secs()),
            Kind::from(self.kind_u16()),
            Tags::from_list(tags),
            self.content(),
            Signature::from_slice(self.signature_bytes())
                .map_err(|_| DecodeError::InvalidSignature)?,
        );

        let mut cursor = self.provenance_start;
        let mut seen = BTreeMap::new();
        for _ in 0..self.seen_count {
            let relay_len = take_u32(self.bytes, &mut cursor)? as usize;
            let relay = std::str::from_utf8(take(self.bytes, &mut cursor, relay_len)?)
                .map_err(|_| DecodeError::InvalidUtf8)?;
            let relay = RelayUrl::parse(relay).map_err(|_| DecodeError::InvalidRelay)?;
            let at = Timestamp::from(take_u64(self.bytes, &mut cursor)?);
            seen.insert(relay, at);
        }
        let local = if self.flags & FLAG_LOCAL != 0 {
            let state = *take(self.bytes, &mut cursor, 1)?
                .first()
                .expect("one byte requested");
            let sig_state = match state {
                0 => SigState::Pending,
                1 => SigState::Signed,
                other => return Err(DecodeError::InvalidLocalState(other)),
            };
            let mut owners = BTreeSet::new();
            for _ in 0..self.owner_count {
                owners.insert(IntentId(take_u64(self.bytes, &mut cursor)?));
            }
            Some(LocalOrigin { owners, sig_state })
        } else {
            None
        };

        Ok(StoredEvent {
            event,
            provenance: Provenance { seen, local },
        })
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
    fn portable_binary_round_trip_is_exact() {
        let expected = fixture();
        let encoded = encode(&expected).unwrap();
        assert_eq!(&encoded[..4], MAGIC);
        assert_eq!(encoded[4], VERSION);
        let actual = StoredEventView::parse(&encoded)
            .unwrap()
            .materialize()
            .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn borrowed_filter_matching_reads_fields_tags_and_content_without_materializing() {
        let stored = fixture();
        let encoded = encode(&stored).unwrap();
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
        let encoded = encode(&stored).unwrap();
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
    fn malformed_lengths_fail_closed_without_unsafe_reads() {
        let stored = fixture();
        let encoded = encode(&stored).unwrap();
        for len in 0..encoded.len() {
            assert!(StoredEventView::parse(&encoded[..len]).is_err());
        }
    }
}
