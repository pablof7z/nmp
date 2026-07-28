//! Pure NIP-29 contextual publication (#838).
//!
//! The event's schema is already complete when it enters this module.
//! NIP-29 owns neither the event's kind nor its schema: it owns only the
//! selected group host and the `h` context it appends. It does not choose an
//! event kind, interpret content, materialize mentions or notifications, or
//! invent reply semantics.

use nostr::{RelayUrl, Tag, UnsignedEvent};

/// A complete draft contextualized for one NIP-29 group host.
///
/// Keeping the selected host and event together prevents the context owner
/// from returning an `h`-tagged event while silently dropping the relay on
/// which that group exists. Signing and publication remain orthogonal stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPublication {
    host: RelayUrl,
    event: UnsignedEvent,
}

impl GroupPublication {
    pub fn host(&self) -> &RelayUrl {
        &self.host
    }

    pub fn event(&self) -> &UnsignedEvent {
        &self.event
    }

    pub fn into_parts(self) -> (RelayUrl, UnsignedEvent) {
        (self.host, self.event)
    }
}

/// NIP-29 contextualization rejects tags whose authority belongs here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupContextError {
    /// The complete draft already carried `h` or `previous`.
    ///
    /// `h` is derived from the selected group. `previous` remains impossible
    /// until a real scoped live-window capability can mint it; callers cannot
    /// smuggle either tag through in a draft.
    ReservedTag(String),
}

/// Add exactly one `["h", group_id]` row to a complete draft.
///
/// Every existing field and tag survives byte-for-byte and in its original
/// order. The selected host is retained in [`GroupPublication`]. No
/// `previous` row is emitted.
pub fn contextualize_group_event(
    host: RelayUrl,
    group_id: &str,
    draft: UnsignedEvent,
) -> Result<GroupPublication, GroupContextError> {
    for tag in draft.tags.iter() {
        if let Some(name @ ("h" | "previous")) = tag.as_slice().first().map(String::as_str) {
            return Err(GroupContextError::ReservedTag(name.to_string()));
        }
    }

    let mut tags = draft.tags.iter().cloned().collect::<Vec<_>>();
    tags.push(Tag::parse(["h", group_id]).expect("'h' is a well-formed non-empty row"));
    let event = UnsignedEvent::new(
        draft.pubkey,
        draft.created_at,
        draft.kind,
        tags,
        draft.content,
    );

    Ok(GroupPublication { host, event })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventId, Keys, Kind, PublicKey, Timestamp};

    fn host() -> RelayUrl {
        RelayUrl::parse("wss://groups.example.com").unwrap()
    }

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn rows(event: &UnsignedEvent) -> Vec<Vec<String>> {
        event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    #[test]
    fn draft_kind_and_schema_survive_except_for_appended_h() {
        let pubkey = author();
        let created_at = Timestamp::from(1_700_000_000u64);
        let draft = UnsignedEvent::new(
            pubkey,
            created_at,
            Kind::from(20u16),
            vec![
                Tag::parse(["title", "sunset"]).unwrap(),
                Tag::parse(["imeta", "url https://cdn.example/sunset.jpg"]).unwrap(),
            ],
            "draft content".to_string(),
        );

        let publication = contextualize_group_event(host(), "photographers", draft).unwrap();
        assert_eq!(publication.host(), &host());
        assert_eq!(publication.event().pubkey, pubkey);
        assert_eq!(publication.event().created_at, created_at);
        assert_eq!(publication.event().kind, Kind::from(20u16));
        assert_eq!(publication.event().content, "draft content");
        assert_eq!(
            rows(publication.event()),
            vec![
                vec!["title".to_string(), "sunset".to_string()],
                vec![
                    "imeta".to_string(),
                    "url https://cdn.example/sunset.jpg".to_string()
                ],
                vec!["h".to_string(), "photographers".to_string()],
            ]
        );
    }

    #[test]
    fn c7_q_reply_survives_without_nip29_interpreting_it() {
        let parent = EventId::from_slice(&[7; 32]).unwrap();
        let draft = UnsignedEvent::new(
            author(),
            Timestamp::from(1_700_000_000u64),
            Kind::from(9u16),
            vec![Tag::parse([
                "q",
                &parent.to_hex(),
                "wss://chat.example.com",
                &author().to_hex(),
            ])
            .unwrap()],
            "reply".to_string(),
        );

        let publication = contextualize_group_event(host(), "chat", draft).unwrap();
        assert_eq!(rows(publication.event())[0][0], "q");
        assert_eq!(
            rows(publication.event()).last().unwrap(),
            &vec!["h".to_string(), "chat".to_string()]
        );
    }

    #[test]
    fn caller_cannot_mint_group_or_previous_authority() {
        for reserved in ["h", "previous"] {
            let draft = UnsignedEvent::new(
                author(),
                Timestamp::from(1_700_000_000u64),
                Kind::from(30023u16),
                vec![Tag::parse([reserved, "caller-value"]).unwrap()],
                String::new(),
            );
            assert_eq!(
                contextualize_group_event(host(), "group-a", draft),
                Err(GroupContextError::ReservedTag(reserved.to_string()))
            );
        }
    }

    #[test]
    fn publication_never_synthesizes_previous() {
        let draft = UnsignedEvent::new(
            author(),
            Timestamp::from(1_700_000_000u64),
            Kind::from(30023u16),
            Vec::new(),
            String::new(),
        );
        let publication = contextualize_group_event(host(), "group-a", draft).unwrap();
        assert_eq!(
            rows(publication.event()),
            vec![vec!["h".to_string(), "group-a".to_string()]]
        );
    }
}
