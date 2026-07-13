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
    let message: RelayMessage<'static> = RelayMessage::from_json(text).ok()?;
    if matches!(&message, RelayMessage::Auth { challenge } if challenge.is_empty()) {
        return None;
    }
    Some(RelayFrame::from_message(message))
}

#[cfg(test)]
mod tests {
    use super::*;

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
