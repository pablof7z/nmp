//! Wire-frame conversions for the [`super::Pool`] worker/translator.
//!
//! HARVEST source: the old repo's `crates/nmp-network/src/pool/frame.rs`
//! (the `tungstenite::Message -> RelayFrame` direction) and
//! `relay_worker/socket_io.rs` (the nonblocking-IO classifier). Unlike the
//! harvested opaque-text handoff, this boundary parses every relay text once
//! into an owned `nostr::RelayMessage`; verification and the engine consume
//! that same value.
//!
//! `Ping`/`Pong`/`Close`/`Binary` remain transport-internal signals the
//! keepalive FSM and the translator's `Disconnected` event already cover;
//! surfacing them as relay messages would duplicate that vocabulary.

use nostr::{JsonUtil, RelayMessage};
use tungstenite::Message;

use super::RelayFrame;

#[cfg(feature = "bench-instrumentation")]
mod diagnostic_duplicate_ceiling {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    #[derive(Clone, Copy)]
    pub(super) struct Entry {
        pub(super) event_kind: u16,
        pub(super) encoded_bytes: usize,
    }

    #[derive(Default)]
    struct Cache {
        capacity: usize,
        entries: HashMap<[u8; 32], Entry>,
        insertion_order: VecDeque<[u8; 32]>,
    }

    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    static CAPACITY: AtomicUsize = AtomicUsize::new(0);
    static EVENT_PAYLOAD_ONLY: AtomicBool = AtomicBool::new(false);

    fn cache() -> &'static Mutex<Cache> {
        CACHE.get_or_init(|| Mutex::new(Cache::default()))
    }

    pub(super) fn configure(capacity: usize, event_payload_only: bool) {
        let mut cache = cache()
            .lock()
            .expect("diagnostic duplicate ceiling cache lock");
        *cache = Cache {
            capacity,
            entries: HashMap::with_capacity(capacity),
            insertion_order: VecDeque::with_capacity(capacity),
        };
        EVENT_PAYLOAD_ONLY.store(event_payload_only, Ordering::Release);
        CAPACITY.store(capacity, Ordering::Release);
    }

    pub(super) fn lookup(text: &str) -> Option<([u8; 32], Option<Entry>)> {
        if CAPACITY.load(Ordering::Acquire) == 0 {
            return None;
        }
        let bytes = if EVENT_PAYLOAD_ONLY.load(Ordering::Acquire) {
            event_payload(text).unwrap_or(text).as_bytes()
        } else {
            text.as_bytes()
        };
        let digest = *blake3::hash(bytes).as_bytes();
        let entry = cache()
            .lock()
            .expect("diagnostic duplicate ceiling cache lock")
            .entries
            .get(&digest)
            .copied();
        crate::ingest_attribution::diagnostic_duplicate_ceiling_lookup(entry.is_some());
        Some((digest, entry))
    }

    pub(super) fn event_payload(text: &str) -> Option<&str> {
        let bytes = text.as_bytes();
        let mut cursor = skip_ws(bytes, 0);
        cursor = expect(bytes, cursor, b'[')?;
        cursor = skip_ws(bytes, cursor);
        if !bytes.get(cursor..)?.starts_with(b"\"EVENT\"") {
            return None;
        }
        cursor += b"\"EVENT\"".len();
        cursor = skip_ws(bytes, cursor);
        cursor = expect(bytes, cursor, b',')?;
        cursor = skip_json_value(bytes, skip_ws(bytes, cursor))?;
        cursor = skip_ws(bytes, cursor);
        cursor = expect(bytes, cursor, b',')?;
        let payload_start = skip_ws(bytes, cursor);
        if bytes.get(payload_start) != Some(&b'{') {
            return None;
        }
        let payload_end = skip_json_value(bytes, payload_start)?;
        cursor = skip_ws(bytes, payload_end);
        cursor = expect(bytes, cursor, b']')?;
        if skip_ws(bytes, cursor) != bytes.len() {
            return None;
        }
        text.get(payload_start..payload_end)
    }

    fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += 1;
        }
        cursor
    }

    fn expect(bytes: &[u8], cursor: usize, expected: u8) -> Option<usize> {
        (bytes.get(cursor) == Some(&expected)).then_some(cursor + 1)
    }

    fn skip_json_value(bytes: &[u8], cursor: usize) -> Option<usize> {
        match *bytes.get(cursor)? {
            b'\"' => skip_string(bytes, cursor),
            b'{' | b'[' => skip_composite(bytes, cursor),
            b',' | b']' | b'}' => None,
            _ => {
                let mut end = cursor;
                while bytes.get(end).is_some_and(|byte| {
                    !byte.is_ascii_whitespace() && !matches!(*byte, b',' | b']' | b'}')
                }) {
                    end += 1;
                }
                (end > cursor).then_some(end)
            }
        }
    }

    fn skip_string(bytes: &[u8], cursor: usize) -> Option<usize> {
        let mut cursor = cursor + 1;
        while let Some(byte) = bytes.get(cursor) {
            match *byte {
                b'\\' => {
                    cursor = cursor.checked_add(2)?;
                    if cursor > bytes.len() {
                        return None;
                    }
                }
                b'\"' => return Some(cursor + 1),
                0x00..=0x1f => return None,
                _ => cursor += 1,
            }
        }
        None
    }

    fn skip_composite(bytes: &[u8], cursor: usize) -> Option<usize> {
        let mut stack = [0_u8; 64];
        stack[0] = match *bytes.get(cursor)? {
            b'{' => b'}',
            b'[' => b']',
            _ => return None,
        };
        let mut depth = 1_usize;
        let mut cursor = cursor + 1;
        while let Some(byte) = bytes.get(cursor) {
            match *byte {
                b'\"' => cursor = skip_string(bytes, cursor)?,
                b'{' => {
                    if depth == stack.len() {
                        return None;
                    }
                    stack[depth] = b'}';
                    depth += 1;
                    cursor += 1;
                }
                b'[' => {
                    if depth == stack.len() {
                        return None;
                    }
                    stack[depth] = b']';
                    depth += 1;
                    cursor += 1;
                }
                b'}' | b']' => {
                    if depth == 0 || stack[depth - 1] != *byte {
                        return None;
                    }
                    depth -= 1;
                    cursor += 1;
                    if depth == 0 {
                        return Some(cursor);
                    }
                }
                _ => cursor += 1,
            }
        }
        None
    }

    pub(super) fn insert(digest: [u8; 32], entry: Entry) {
        let mut cache = cache()
            .lock()
            .expect("diagnostic duplicate ceiling cache lock");
        if cache.capacity == 0 || cache.entries.contains_key(&digest) {
            return;
        }
        if cache.entries.len() == cache.capacity {
            let evicted = cache
                .insertion_order
                .pop_front()
                .expect("full diagnostic cache has an eviction candidate");
            cache.entries.remove(&evicted);
        }
        cache.entries.insert(digest, entry);
        cache.insertion_order.push_back(digest);
        crate::ingest_attribution::diagnostic_duplicate_ceiling_insert();
    }
}

#[cfg(feature = "bench-instrumentation")]
pub(crate) fn configure_diagnostic_duplicate_ceiling(capacity: usize, event_payload_only: bool) {
    diagnostic_duplicate_ceiling::configure(capacity, event_payload_only);
}

/// Convert one inbound `tungstenite::Message` into a [`RelayFrame`].
/// Returns `None` for message kinds the engine never needs to see as a
/// frame: `Ping`/`Pong` (keepalive-internal — consumed by the worker's
/// [`crate::keepalive::KeepaliveState`]), `Close` (surfaced instead as a
/// [`super::PoolEvent::Disconnected`]), and the raw `Frame` variant tungstenite
/// itself never yields to a reader.
pub(super) fn classify_message(message: &Message) -> Option<RelayFrame> {
    match message {
        Message::Text(text) => classify_text(text.as_str()),
        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Close(_) => None,
        Message::Frame(_) => None,
    }
}

/// Parse one websocket text into the owned value carried through verification
/// and engine ingest. Malformed or unsupported relay messages fail closed at
/// this boundary and never become a pool event.
pub(super) fn classify_text(text: &str) -> Option<RelayFrame> {
    #[cfg(feature = "bench-instrumentation")]
    let diagnostic_digest = {
        diagnostic_duplicate_ceiling::lookup(text).map(|(digest, hit)| {
            if let Some(hit) = hit {
                return Err(RelayFrame::diagnostic_duplicate_ceiling_token(
                    hit.event_kind,
                    hit.encoded_bytes,
                ));
            }
            Ok(digest)
        })
    };
    #[cfg(feature = "bench-instrumentation")]
    if let Some(Err(hit)) = diagnostic_digest {
        return Some(hit);
    }
    #[cfg(feature = "bench-instrumentation")]
    let diagnostic_digest = diagnostic_digest.and_then(Result::ok);
    #[cfg(feature = "bench-instrumentation")]
    let started = std::time::Instant::now();
    let parsed = RelayMessage::from_json(text).ok();
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::parse(started.elapsed(), parsed.is_some());
    let message: RelayMessage<'static> = parsed?;
    if matches!(&message, RelayMessage::Auth { challenge } if challenge.is_empty()) {
        return None;
    }
    #[cfg(feature = "bench-instrumentation")]
    if let (Some(diagnostic_digest), RelayMessage::Event { event, .. }) =
        (diagnostic_digest, &message)
    {
        diagnostic_duplicate_ceiling::insert(
            diagnostic_digest,
            diagnostic_duplicate_ceiling::Entry {
                event_kind: event.kind.as_u16(),
                encoded_bytes: text.len(),
            },
        );
    }
    Some(RelayFrame::from_message(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "bench-instrumentation")]
    #[test]
    fn diagnostic_locator_ignores_subscription_id_and_preserves_exact_event_bytes() {
        let payload =
            r#"{"id":"abc","content":"brace } and escaped \\\" quote","tags":[["p","def"]]}"#;
        let first = format!(r#"["EVENT","first",{payload}]"#);
        let second = format!(r#" [ "EVENT" , "second" , {payload} ] "#);
        assert_eq!(
            diagnostic_duplicate_ceiling::event_payload(&first),
            Some(payload)
        );
        assert_eq!(
            diagnostic_duplicate_ceiling::event_payload(&second),
            Some(payload)
        );
        let mutated = first.replace("abc", "abd");
        assert_ne!(
            diagnostic_duplicate_ceiling::event_payload(&mutated),
            Some(payload)
        );
    }

    #[test]
    fn classify_auth_extracts_non_empty_challenge() {
        match classify_text(r#"["AUTH","challenge-token-123"]"#)
            .expect("valid AUTH")
            .into_message()
        {
            RelayMessage::Auth { challenge } => assert_eq!(challenge, "challenge-token-123"),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn classify_parses_event_once_into_owned_message() {
        let event = nostr::EventBuilder::text_note("typed")
            .sign_with_keys(&nostr::Keys::generate())
            .expect("signed event");
        let raw = RelayMessage::event(nostr::SubscriptionId::new("sub"), event.clone()).as_json();
        let frame = classify_text(&raw).expect("valid EVENT");
        assert_eq!(
            std::sync::Arc::strong_count(frame.event().expect("EVENT allocation")),
            1,
            "classification owns one shared allocation, not a deep copy"
        );
        assert_eq!(frame.into_event().expect("EVENT frame"), event);
    }

    #[test]
    fn classify_malformed_event_fails_closed() {
        assert!(classify_text(r#"["EVENT","sub",{"id":"abc"}]"#).is_none());
    }

    #[test]
    fn classify_empty_auth_challenge_fails_closed() {
        assert!(classify_text(r#"["AUTH",""]"#).is_none());
    }

    #[test]
    fn classify_invalid_json_fails_closed() {
        assert!(classify_text(r#"["AUTH", not-valid-json"#).is_none());
    }

    #[test]
    fn non_text_messages_yield_none() {
        assert!(classify_message(&Message::Binary(vec![1, 2, 3].into())).is_none());
        assert!(classify_message(&Message::Ping(Vec::new().into())).is_none());
        assert!(classify_message(&Message::Pong(Vec::new().into())).is_none());
    }
}
