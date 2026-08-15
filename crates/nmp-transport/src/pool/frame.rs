//! Wire-frame conversions for the [`super::Pool`] worker/translator.
//!
//! HARVEST source: the old repo's `crates/nmp-network/src/pool/frame.rs`
//! (the `tungstenite::Message -> RelayFrame` direction) and
//! `relay_worker/socket_io.rs` (the nonblocking-IO classifier). Unlike the
//! harvested opaque-text handoff, this boundary parses every ordinary relay
//! text once into an owned `nostr::RelayMessage`; exact observations that the
//! engine published after commit can instead take the fail-closed preparse
//! path below. Verification and the engine consume the same owned value on a
//! miss or rejected lease.
//!
//! `Ping`/`Pong`/`Close`/`Binary` remain transport-internal signals the
//! keepalive FSM and the translator's `Disconnected` event already cover;
//! surfacing them as relay messages would duplicate that vocabulary.

use nostr::{JsonUtil, RelayMessage};
use tungstenite::{Message, Utf8Bytes};

use super::committed_observations::{
    CommittedObservationCache, CommittedObservationCandidate, RelayScope,
};
use super::RelayFrame;

#[cfg(feature = "bench-instrumentation")]
mod diagnostic_preparsed_ceiling {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use super::RelayFrame;
    use nostr::{Event, SubscriptionId};

    #[derive(Default)]
    struct Cache {
        subscription_id: Option<SubscriptionId>,
        events: VecDeque<Arc<Event>>,
    }

    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    static ENABLED: AtomicBool = AtomicBool::new(false);

    fn cache() -> &'static Mutex<Cache> {
        CACHE.get_or_init(|| Mutex::new(Cache::default()))
    }

    pub(super) fn configure(subscription_id: Option<SubscriptionId>, events: Vec<Arc<Event>>) {
        ENABLED.store(
            subscription_id.is_some() && !events.is_empty(),
            Ordering::Release,
        );
        *cache().lock().expect("diagnostic preparsed cache lock") = Cache {
            subscription_id,
            events: events.into(),
        };
    }

    pub(super) fn take() -> Option<RelayFrame> {
        if !ENABLED.load(Ordering::Acquire) {
            return None;
        }
        let mut cache = cache().lock().expect("diagnostic preparsed cache lock");
        if cache.events.is_empty() {
            ENABLED.store(false, Ordering::Release);
            return None;
        }
        let event = cache.events.pop_front();
        let subscription_id = cache.subscription_id.clone();
        let frame = event
            .zip(subscription_id)
            .map(|(event, subscription_id)| RelayFrame::Event {
                subscription_id,
                event,
                observation_candidate: None,
            });
        crate::ingest_attribution::diagnostic_preparsed_ceiling_lookup(frame.is_some());
        frame
    }
}

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
        super::event_payload(text)
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

#[cfg(feature = "bench-instrumentation")]
pub(crate) fn configure_diagnostic_preparsed_ceiling(
    subscription_id: Option<nostr::SubscriptionId>,
    events: Vec<std::sync::Arc<nostr::Event>>,
) {
    diagnostic_preparsed_ceiling::configure(subscription_id, events);
}

/// Convert one inbound `tungstenite::Message` into a [`RelayFrame`].
/// Returns `None` for message kinds the engine never needs to see as a
/// frame: `Ping`/`Pong` (keepalive-internal — consumed by the worker's
/// [`crate::keepalive::KeepaliveState`]), `Close` (surfaced instead as a
/// [`super::PoolEvent::Disconnected`]), and the raw `Frame` variant tungstenite
/// itself never yields to a reader.
/// What one inbound websocket message turned out to be.
///
/// Three outcomes, not two. The distinction that matters is between a message
/// this boundary UNDERSTOOD and chose not to forward, and one it could not
/// read at all: the first is provably not an EVENT, while the second could
/// have been an EVENT for any subscription, and a caller counting what a
/// relay returned has to tell those apart (#1668). Collapsing them into
/// `None` is what hid the second class entirely.
#[derive(Debug)]
pub(super) enum ClassifiedFrame {
    /// One frame to hand to the engine.
    Frame(RelayFrame),
    /// Read and understood, with nothing the engine needs: a keepalive,
    /// binary, or close message, or a decoded relay message this boundary
    /// deliberately drops.
    Consumed,
    /// A TEXT frame that did not decode into a `RelayMessage`. What the relay
    /// meant by it is unknowable.
    Undecodable,
}

pub(super) fn classify_message(
    message: Message,
    relay: RelayScope,
    committed_observations: &CommittedObservationCache,
) -> ClassifiedFrame {
    match message {
        Message::Text(text) => classify_owned_text(text, relay, committed_observations),
        // Consumed by the worker's keepalive FSM or the close path, and
        // never a relay message: nothing failed to decode here.
        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Close(_) => {
            ClassifiedFrame::Consumed
        }
        Message::Frame(_) => ClassifiedFrame::Consumed,
    }
}

fn classify_owned_text(
    text: Utf8Bytes,
    relay: RelayScope,
    committed_observations: &CommittedObservationCache,
) -> ClassifiedFrame {
    let candidate = event_payload(text.as_str()).map(|payload| {
        CommittedObservationCandidate::new(*blake3::hash(payload.as_bytes()).as_bytes())
    });
    if let Some(candidate) = candidate {
        match committed_observations.lookup(relay, candidate.digest(), text) {
            Ok(hit) => return ClassifiedFrame::Frame(RelayFrame::CommittedObservation(hit)),
            Err(text) => return classify_text_with_candidate(text.as_str(), Some(candidate)),
        }
    }
    classify_text_with_candidate(text.as_str(), None)
}

#[cfg(test)]
impl ClassifiedFrame {
    /// The frame, when a test asserts this input produced one. Deliberately
    /// not an `Option`-shaped helper: a test that wants a non-frame outcome
    /// must name which one, because `Consumed` and `Undecodable` are the
    /// distinction this type exists to keep.
    fn expect_frame(self, message: &str) -> RelayFrame {
        match self {
            Self::Frame(frame) => frame,
            other => panic!("{message}: got {other:?}"),
        }
    }
}

/// Parse one websocket text into the owned value carried through verification
/// and engine ingest. Malformed or unsupported relay messages fail closed at
/// this boundary and never become a pool event.
#[cfg(test)]
pub(super) fn classify_text(text: &str) -> ClassifiedFrame {
    classify_text_with_candidate(text, None)
}

pub(super) fn classify_text_with_candidate(
    text: &str,
    observation_candidate: Option<CommittedObservationCandidate>,
) -> ClassifiedFrame {
    #[cfg(feature = "bench-instrumentation")]
    if let Some(frame) = diagnostic_preparsed_ceiling::take() {
        return ClassifiedFrame::Frame(frame);
    }
    #[cfg(feature = "bench-instrumentation")]
    let diagnostic_digest = match diagnostic_duplicate_ceiling::lookup(text) {
        Some((_, Some(hit))) => {
            return ClassifiedFrame::Frame(RelayFrame::diagnostic_duplicate_ceiling_token(
                hit.event_kind,
                hit.encoded_bytes,
            ));
        }
        Some((digest, None)) => Some(digest),
        None => None,
    };
    #[cfg(feature = "bench-instrumentation")]
    let started = std::time::Instant::now();
    let parsed = RelayMessage::from_json(text).ok();
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::parse(started.elapsed(), parsed.is_some());
    // The one place a relay text frame is genuinely unreadable. Everything
    // below this line decoded, so everything below can say what it was.
    let Some(message) = parsed else {
        return ClassifiedFrame::Undecodable;
    };
    let message: RelayMessage<'static> = message;
    // Decoded, and provably an AUTH rather than an EVENT: dropping it costs
    // no caller any certainty about what the relay returned.
    if matches!(&message, RelayMessage::Auth { challenge } if challenge.is_empty()) {
        return ClassifiedFrame::Consumed;
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
    match message {
        RelayMessage::Event {
            subscription_id,
            event,
        } if observation_candidate.is_some() => {
            ClassifiedFrame::Frame(RelayFrame::from_observed_event(
                subscription_id.into_owned(),
                event.into_owned(),
                observation_candidate.expect("guarded above"),
            ))
        }
        message => ClassifiedFrame::Frame(RelayFrame::from_message(message)),
    }
}

fn event_payload(text: &str) -> Option<&str> {
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
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'\"') {
        return None;
    }
    cursor = skip_string(bytes, cursor)?;
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
            b'\\' => match *bytes.get(cursor + 1)? {
                b'\"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                    cursor += 2;
                }
                b'u' => {
                    let codepoint = json_hex_quad(bytes, cursor + 2)?;
                    cursor += 6;
                    if (0xD800..=0xDBFF).contains(&codepoint) {
                        if bytes.get(cursor..cursor + 2) != Some(b"\\u") {
                            return None;
                        }
                        let low = json_hex_quad(bytes, cursor + 2)?;
                        if !(0xDC00..=0xDFFF).contains(&low) {
                            return None;
                        }
                        cursor += 6;
                    } else if (0xDC00..=0xDFFF).contains(&codepoint) {
                        return None;
                    }
                }
                _ => return None,
            },
            b'\"' => return Some(cursor + 1),
            0x00..=0x1f => return None,
            _ => cursor += 1,
        }
    }
    None
}

fn json_hex_quad(bytes: &[u8], cursor: usize) -> Option<u16> {
    let digits = bytes.get(cursor..cursor + 4)?;
    digits.iter().try_fold(0_u16, |value, digit| {
        let digit = match *digit {
            b'0'..=b'9' => u16::from(*digit - b'0'),
            b'a'..=b'f' => u16::from(*digit - b'a') + 10,
            b'A'..=b'F' => u16::from(*digit - b'A') + 10,
            _ => return None,
        };
        Some((value << 4) | digit)
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::RelayUrl;

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
            .expect_frame("valid AUTH")
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
        let frame = classify_text(&raw).expect_frame("valid EVENT");
        assert_eq!(
            std::sync::Arc::strong_count(frame.event().expect("EVENT allocation")),
            1,
            "classification owns one shared allocation, not a deep copy"
        );
        assert_eq!(frame.into_event().expect("EVENT frame"), event);
    }

    #[test]
    fn committed_observation_preserves_subscription_id_and_exact_fallback() {
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let relay_scope = RelayScope::new(&relay);
        let cache = CommittedObservationCache::new(8);
        let event = nostr::EventBuilder::text_note("committed")
            .sign_with_keys(&nostr::Keys::generate())
            .expect("signed event");
        let first_raw =
            RelayMessage::event(nostr::SubscriptionId::new("first"), event.clone()).as_json();
        let first = classify_message(Message::Text(first_raw.into()), relay_scope, &cache)
            .expect_frame("first EVENT");
        let (subscription_id, parsed, candidate) =
            first.into_observed_event().expect("ordinary EVENT");
        assert_eq!(subscription_id.as_str(), "first");
        let candidate = candidate.expect("raw EVENT locator candidate");
        assert_eq!(parsed, event);
        cache.apply_update(
            [],
            [super::super::CommittedObservationPublication::new(
                relay.clone(),
                candidate,
                event.id,
                event.kind.as_u16(),
            )],
        );

        let second_raw =
            RelayMessage::event(nostr::SubscriptionId::new("second"), event.clone()).as_json();
        let invalid_subscription = second_raw.replacen("\"second\"", "42", 1);
        assert!(matches!(
            classify_message(
                Message::Text(invalid_subscription.into()),
                relay_scope,
                &cache,
            ),
            ClassifiedFrame::Undecodable,
        ));
        let invalid_escape = second_raw.replacen("\"second\"", r#""bad\x""#, 1);
        assert!(matches!(
            classify_message(Message::Text(invalid_escape.into()), relay_scope, &cache),
            ClassifiedFrame::Undecodable,
        ));
        let hit = classify_message(Message::Text(second_raw.into()), relay_scope, &cache)
            .expect_frame("cached EVENT");
        assert!(matches!(hit, RelayFrame::CommittedObservation(_)));
        let (fallback_subscription_id, fallback, fallback_candidate) = hit
            .into_ordinary_fallback()
            .expect("valid fallback frame")
            .into_observed_event()
            .expect("exact fallback EVENT");
        assert_eq!(fallback_subscription_id.as_str(), "second");
        assert_eq!(fallback, event);
        assert_eq!(fallback_candidate, Some(candidate));
    }

    #[test]
    fn classify_malformed_event_reports_an_undecodable_frame() {
        assert!(matches!(
            classify_text(r#"["EVENT","sub",{"id":"abc"}]"#),
            ClassifiedFrame::Undecodable,
        ));
    }

    #[test]
    fn classify_invalid_json_reports_an_undecodable_frame() {
        assert!(matches!(
            classify_text(r#"["AUTH", not-valid-json"#),
            ClassifiedFrame::Undecodable,
        ));
    }

    /// A decoded AUTH this boundary drops is not an undecodable frame. It
    /// parsed, so it is provably not an EVENT, and reporting it would erase
    /// returned-frame counts a relay never put in doubt (#1668).
    #[test]
    fn classify_empty_auth_challenge_is_consumed_not_undecodable() {
        assert!(matches!(
            classify_text(r#"["AUTH",""]"#),
            ClassifiedFrame::Consumed,
        ));
    }

    /// Keepalive and binary traffic is read and understood. Counting it as
    /// undecodable would erase a count on every ping.
    #[test]
    fn non_text_messages_are_consumed_not_undecodable() {
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let relay = RelayScope::new(&relay);
        let cache = CommittedObservationCache::new(0);
        for message in [
            Message::Binary(vec![1, 2, 3].into()),
            Message::Ping(Vec::new().into()),
            Message::Pong(Vec::new().into()),
        ] {
            assert!(matches!(
                classify_message(message, relay, &cache),
                ClassifiedFrame::Consumed,
            ));
        }
    }
}
