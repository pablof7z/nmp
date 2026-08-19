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

pub(super) fn classify_text_with_candidate(
    text: &str,
    observation_candidate: Option<CommittedObservationCandidate>,
) -> ClassifiedFrame {
    let parsed = RelayMessage::from_json(text).ok();
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

