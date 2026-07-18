//! Benchmark-only immutable canonical-event runs for issue #696.

use crate::binary_event::StoredEventView;

const MAGIC: &[u8; 4] = b"NMCR";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 32;

pub(super) fn encode(
    run_id: u64,
    min_event_key: u64,
    events: &[Vec<u8>],
) -> Result<Vec<u8>, String> {
    if events.is_empty() {
        return Err("canonical run must not be empty".to_owned());
    }
    let event_count = u32::try_from(events.len())
        .map_err(|_| "canonical run event count exceeds u32".to_owned())?;
    min_event_key
        .checked_add(events.len() as u64 - 1)
        .ok_or_else(|| "canonical run event-key range overflows".to_owned())?;
    let arena_len = events.iter().try_fold(0usize, |total, event| {
        StoredEventView::parse(event)
            .map_err(|error| format!("invalid canonical event: {error}"))?;
        total
            .checked_add(event.len())
            .ok_or_else(|| "canonical run arena length overflows".to_owned())
    })?;
    let arena_len_u32 =
        u32::try_from(arena_len).map_err(|_| "canonical run arena exceeds u32".to_owned())?;
    let directory_len = events
        .len()
        .checked_add(1)
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| "canonical run directory length overflows".to_owned())?;
    let capacity = HEADER_LEN
        .checked_add(directory_len)
        .and_then(|length| length.checked_add(arena_len))
        .ok_or_else(|| "canonical run length overflows".to_owned())?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(MAGIC);
    encoded.push(VERSION);
    encoded.extend_from_slice(&[0, 0, 0]);
    encoded.extend_from_slice(&run_id.to_be_bytes());
    encoded.extend_from_slice(&min_event_key.to_be_bytes());
    encoded.extend_from_slice(&event_count.to_be_bytes());
    encoded.extend_from_slice(&arena_len_u32.to_be_bytes());
    let mut offset = 0u32;
    encoded.extend_from_slice(&offset.to_be_bytes());
    for event in events {
        offset = offset
            .checked_add(
                u32::try_from(event.len())
                    .map_err(|_| "canonical event length exceeds u32".to_owned())?,
            )
            .ok_or_else(|| "canonical run offset overflows".to_owned())?;
        encoded.extend_from_slice(&offset.to_be_bytes());
    }
    for event in events {
        encoded.extend_from_slice(event);
    }
    debug_assert_eq!(encoded.len(), capacity);
    Ok(encoded)
}

#[derive(Debug, Clone, Copy)]
pub(super) struct View<'a> {
    bytes: &'a [u8],
    run_id: u64,
    min_event_key: u64,
    event_count: usize,
    arena_start: usize,
}

impl<'a> View<'a> {
    pub(super) fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        let view = Self::from_trusted(bytes)?;
        let mut previous = 0usize;
        for index in 0..view.event_count {
            let next = read_u32(bytes, HEADER_LEN + (index + 1) * 4)? as usize;
            if next <= previous || next > bytes.len() - view.arena_start {
                return Err("canonical run offsets are invalid".to_owned());
            }
            StoredEventView::parse(&bytes[view.arena_start + previous..view.arena_start + next])
                .map_err(|error| format!("invalid nested canonical event: {error}"))?;
            previous = next;
        }
        Ok(view)
    }

    /// Open a run already fully validated at insertion or recovery time.
    pub(super) fn from_trusted(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.len() < HEADER_LEN {
            return Err("canonical run is truncated".to_owned());
        }
        if &bytes[..4] != MAGIC {
            return Err("canonical run magic is invalid".to_owned());
        }
        if bytes[4] != VERSION {
            return Err("canonical run version is unsupported".to_owned());
        }
        if bytes[5..8] != [0, 0, 0] {
            return Err("canonical run reserved bytes are nonzero".to_owned());
        }
        let run_id = read_u64(bytes, 8)?;
        let min_event_key = read_u64(bytes, 16)?;
        let event_count = read_u32(bytes, 24)? as usize;
        if event_count == 0 {
            return Err("canonical run must not be empty".to_owned());
        }
        min_event_key
            .checked_add(event_count as u64 - 1)
            .ok_or_else(|| "canonical run event-key range overflows".to_owned())?;
        let arena_len = read_u32(bytes, 28)? as usize;
        let directory_len = event_count
            .checked_add(1)
            .and_then(|count| count.checked_mul(4))
            .ok_or_else(|| "canonical run directory length overflows".to_owned())?;
        let arena_start = HEADER_LEN
            .checked_add(directory_len)
            .ok_or_else(|| "canonical run directory length overflows".to_owned())?;
        let expected_len = arena_start
            .checked_add(arena_len)
            .ok_or_else(|| "canonical run length overflows".to_owned())?;
        if expected_len > bytes.len() {
            return Err("canonical run is truncated".to_owned());
        }
        if expected_len < bytes.len() {
            return Err("canonical run has trailing bytes".to_owned());
        }
        if read_u32(bytes, HEADER_LEN)? != 0 {
            return Err("canonical run first offset is nonzero".to_owned());
        }
        if read_u32(bytes, HEADER_LEN + event_count * 4)? as usize != arena_len {
            return Err("canonical run final offset disagrees with arena".to_owned());
        }
        Ok(Self {
            bytes,
            run_id,
            min_event_key,
            event_count,
            arena_start,
        })
    }

    pub(super) fn run_id(self) -> u64 {
        self.run_id
    }

    pub(super) fn min_event_key(self) -> u64 {
        self.min_event_key
    }

    pub(super) fn max_event_key(self) -> u64 {
        self.min_event_key + self.event_count as u64 - 1
    }

    pub(super) fn len(self) -> usize {
        self.event_count
    }

    pub(super) fn event_bytes(self, event_key: u64) -> Result<Option<&'a [u8]>, String> {
        let Some(index) = event_key.checked_sub(self.min_event_key) else {
            return Ok(None);
        };
        let Ok(index) = usize::try_from(index) else {
            return Ok(None);
        };
        if index >= self.event_count {
            return Ok(None);
        }
        let start = read_u32(self.bytes, HEADER_LEN + index * 4)? as usize;
        let end = read_u32(self.bytes, HEADER_LEN + (index + 1) * 4)? as usize;
        Ok(Some(
            &self.bytes[self.arena_start + start..self.arena_start + end],
        ))
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "canonical run is truncated".to_owned())?;
    Ok(u32::from_be_bytes(raw.try_into().expect("u32 width")))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "canonical run is truncated".to_owned())?;
    Ok(u64::from_be_bytes(raw.try_into().expect("u64 width")))
}

#[cfg(test)]
mod tests {
    use nostr::{EventBuilder, Keys, Kind};

    use super::*;
    use crate::binary_event;

    fn event_values() -> Vec<Vec<u8>> {
        let keys = Keys::generate();
        [
            "a".to_owned(),
            "different-sized content".to_owned(),
            "z".repeat(200),
        ]
        .into_iter()
        .map(|content| {
            let event = EventBuilder::new(Kind::TextNote, content)
                .sign_with_keys(&keys)
                .unwrap();
            binary_event::encode_event(&event).unwrap()
        })
        .collect()
    }

    #[test]
    fn round_trip_and_bounded_lookup_are_exact() {
        let events = event_values();
        let encoded = encode(7, 41, &events).unwrap();
        let view = View::parse(&encoded).unwrap();
        assert_eq!(view.run_id(), 7);
        assert_eq!(view.min_event_key(), 41);
        assert_eq!(view.max_event_key(), 43);
        assert_eq!(view.len(), 3);
        assert_eq!(view.event_bytes(40).unwrap(), None);
        assert_eq!(view.event_bytes(41).unwrap(), Some(events[0].as_slice()));
        assert_eq!(view.event_bytes(43).unwrap(), Some(events[2].as_slice()));
        assert_eq!(view.event_bytes(44).unwrap(), None);
    }

    #[test]
    fn every_truncation_and_trailing_bytes_are_rejected() {
        let encoded = encode(7, 41, &event_values()).unwrap();
        for length in 0..encoded.len() {
            assert!(View::parse(&encoded[..length]).is_err(), "length {length}");
        }
        let mut trailing = encoded;
        trailing.push(0);
        assert!(View::parse(&trailing).is_err());
    }

    #[test]
    fn corrupt_headers_offsets_and_nested_events_are_rejected() {
        let encoded = encode(7, 41, &event_values()).unwrap();
        for offset in [0usize, 4, 5, 24, 32, 36] {
            let mut corrupt = encoded.clone();
            corrupt[offset] ^= 0xff;
            assert!(View::parse(&corrupt).is_err(), "offset {offset}");
        }
        let mut bad_range = encoded.clone();
        bad_range[16..24].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(View::parse(&bad_range).is_err());
        let arena_start = HEADER_LEN + (event_values().len() + 1) * 4;
        let mut bad_nested = encoded;
        bad_nested[arena_start] ^= 0xff;
        assert!(View::parse(&bad_nested).is_err());
    }
}
