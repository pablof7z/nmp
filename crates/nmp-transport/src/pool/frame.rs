//! Wire-frame conversions for the [`super::Pool`] worker/translator.
//!
//! HARVEST source: the old repo's `crates/nmp-network/src/pool/frame.rs`
//! (the `tungstenite::Message -> RelayFrame` direction, including the NIP-42
//! `AUTH` pre-classification) and `relay_worker/socket_io.rs` (the
//! nonblocking-IO classifier). The AUTH classifier here is a fresh, minimal
//! `serde_json`-based sniff (the old repo's `nmp_nip42_types::parse_auth_frame`
//! helper is a sibling crate this workspace does not have); the shape and the
//! reasoning — cheap substring fast-path before parsing, fall through to
//! `Text` on anything malformed — are the harvested part.
//!
//! `crate::pool::RelayFrame` is substrate-grade and deliberately narrower
//! than the old repo's: only `Text`/`Auth` are exposed (no `Ping`/`Pong`/
//! `Close`/`Binary` variants — those are transport-internal signals the
//! keepalive FSM and the translator's `Disconnected` event already cover;
//! surfacing them as frames would duplicate that vocabulary for no reader).

use tungstenite::Message;

use super::RelayFrame;

/// Convert one inbound `tungstenite::Message` into a [`RelayFrame`].
/// Returns `None` for message kinds the engine never needs to see as a
/// frame: `Ping`/`Pong` (keepalive-internal — consumed by the worker's
/// [`crate::keepalive::KeepaliveState`]), `Close` (surfaced instead as a
/// [`super::PoolEvent::Disconnected`]), and the raw `Frame` variant tungstenite
/// itself never yields to a reader.
pub(super) fn classify_message(message: &Message) -> Option<RelayFrame> {
    match message {
        Message::Text(text) => Some(classify_text(text.as_str())),
        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Close(_) => None,
        Message::Frame(_) => None,
    }
}

/// Peek a text frame for the NIP-42 `["AUTH", <challenge>]` shape;
/// fall through to [`RelayFrame::Text`] on anything else (non-AUTH frame,
/// malformed JSON, empty/non-string challenge).
pub(super) fn classify_text(text: &str) -> RelayFrame {
    // Cheap fast-path: only parse JSON if the frame looks like it might be
    // an AUTH frame (NIP-42 frames are `["AUTH", ...]`, case-sensitive).
    if !text.contains("\"AUTH\"") {
        return RelayFrame::Text(text.to_string());
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return RelayFrame::Text(text.to_string());
    };
    let challenge = parsed
        .as_array()
        .filter(|arr| arr.len() >= 2)
        .filter(|arr| arr[0].as_str() == Some("AUTH"))
        .and_then(|arr| arr[1].as_str())
        .filter(|s| !s.is_empty());
    match challenge {
        Some(challenge) => RelayFrame::Auth(challenge.to_string()),
        None => RelayFrame::Text(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_auth_extracts_non_empty_challenge() {
        match classify_text(r#"["AUTH","challenge-token-123"]"#) {
            RelayFrame::Auth(challenge) => assert_eq!(challenge, "challenge-token-123"),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn classify_passes_non_auth_text_through_untouched() {
        let raw = r#"["EVENT","sub",{"id":"abc"}]"#;
        match classify_text(raw) {
            RelayFrame::Text(text) => assert_eq!(text, raw),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn classify_malformed_auth_falls_through_to_text() {
        let raw = r#"["AUTH",""]"#;
        match classify_text(raw) {
            RelayFrame::Text(text) => assert_eq!(text, raw),
            other => panic!("expected Text for empty challenge, got {other:?}"),
        }
    }

    #[test]
    fn classify_does_not_misfire_on_auth_substring_in_other_frames() {
        let raw = r#"["EVENT","sub",{"content":"the \"AUTH\" word"}]"#;
        match classify_text(raw) {
            RelayFrame::Text(text) => assert_eq!(text, raw),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn classify_invalid_json_falls_through_to_text() {
        let raw = r#"["AUTH", not-valid-json"#;
        match classify_text(raw) {
            RelayFrame::Text(text) => assert_eq!(text, raw),
            other => panic!("expected Text for invalid JSON, got {other:?}"),
        }
    }

    #[test]
    fn non_text_messages_yield_none() {
        assert!(classify_message(&Message::Binary(vec![1, 2, 3].into())).is_none());
        assert!(classify_message(&Message::Ping(Vec::new().into())).is_none());
        assert!(classify_message(&Message::Pong(Vec::new().into())).is_none());
    }
}
