//! The event a scenario hands the group door, in both of its forms.
//!
//! Split from [`super::groups`] because it is staging rather than acting: an
//! unsigned draft the app composed, or an event some app signed EARLIER and is
//! now publishing unchanged. The two are not variations of one thing -- the
//! first is contextualized before signing and the second may not be touched at
//! all -- so the world keeps them apart, and signing the second is deferred
//! until the scenario stops adding rows to it, because its id and signature
//! only mean anything over the final tag list.
//!
//! The id LABELS live here for the same reason. A `.feature` cannot spell a
//! real event id: one is only known after signing. So `has id "9f2c..."` BINDS
//! that word to the id the event actually got, and every later step naming it
//! compares against the binding. What the scenario asserts is identity
//! preservation, which is exactly what a binding proves.

use nostr::{Event, EventId, Kind, Tag, Timestamp, UnsignedEvent};

use nmp_grammar::EventBuilder;

use super::groups::{keys_from_hex, APP_CHOSEN_CREATED_AT};
use super::NmpWorld;

/// A signed fixture event under construction. Signing is deferred until the
/// scenario stops adding tags to it, because the id and the signature only
/// mean anything over the final tag list.
pub(super) struct PendingSignedEvent {
    pub(super) author: String,
    pub(super) kind: u16,
    pub(super) content: String,
    pub(super) tags: Vec<Tag>,
}

impl NmpWorld {
    /// `Given an unsigned event of kind K with content "..."`.
    pub fn stage_draft(&mut self, kind: u16, content: &str) {
        self.staged_draft = Some(EventBuilder::new(Kind::from(kind)).content(content));
    }

    /// `And that event carries the tags "d"="portfolio" and "t"="landscape"`
    /// / `... already carries an h tag with value "X"` / `... a previous tag`.
    pub fn draft_add_tag(&mut self, name: &str, value: &str) {
        let draft = self
            .staged_draft
            .take()
            .expect("nmp-bdd: no unsigned event has been staged to add a tag to");
        self.staged_draft = Some(draft.tag(
            Tag::parse([name, value]).expect("nmp-bdd: a two-value fixture tag is well-formed"),
        ));
    }

    /// `And that event carries a created_at the app chose` -- a fixed epoch,
    /// so "it survived unchanged" compares against a known number rather than
    /// against whatever the clock did.
    pub fn draft_chooses_created_at(&mut self) {
        let draft = self
            .staged_draft
            .take()
            .expect("nmp-bdd: no unsigned event has been staged to timestamp");
        self.staged_draft = Some(draft.created_at(Timestamp::from(APP_CHOSEN_CREATED_AT)));
    }

    /// The draft the scenario supplied, for the `Then` that compares it
    /// against what was delivered.
    pub fn supplied_draft(&self) -> Option<&EventBuilder> {
        self.staged_draft.as_ref()
    }

    /// `Given an event signed earlier by "<hex>" of kind K with content "..."`.
    pub fn stage_signed_event(&mut self, author_hex: &str, kind: u16, content: &str) {
        self.people
            .entry(author_hex.to_string())
            .or_insert_with(|| keys_from_hex(author_hex));
        self.staged_signed_parts = Some(PendingSignedEvent {
            author: author_hex.to_string(),
            kind,
            content: content.to_string(),
            tags: Vec::new(),
        });
        self.staged_signed_event = None;
    }

    /// `And that signed event carries an h tag with value "X"` (and the
    /// two-value form). Signing is still deferred: the id must cover the
    /// final tag list.
    pub fn signed_event_add_tag(&mut self, name: &str, value: &str) {
        self.staged_signed_parts
            .as_mut()
            .expect("nmp-bdd: no signed event has been staged to add a tag to")
            .tags
            .push(
                Tag::parse([name, value]).expect("nmp-bdd: a two-value fixture tag is well-formed"),
            );
        self.staged_signed_event = None;
    }

    /// Sign the staged parts, once, and keep the result.
    pub fn signed_event(&mut self) -> Event {
        if let Some(event) = self.staged_signed_event.clone() {
            return event;
        }
        let author = self
            .staged_signed_parts
            .as_ref()
            .expect("nmp-bdd: no signed event has been staged")
            .author
            .clone();
        let keys = self.person(&author);
        let parts = self
            .staged_signed_parts
            .as_ref()
            .expect("nmp-bdd: no signed event has been staged");
        let event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(APP_CHOSEN_CREATED_AT),
            Kind::from(parts.kind),
            parts.tags.clone(),
            parts.content.clone(),
        )
        .sign_with_keys(&keys)
        .expect("nmp-bdd: fixture keys sign cleanly");
        self.staged_signed_event = Some(event.clone());
        event
    }

    /// `And that signed event has id "9f2c..."` -- BINDS that word to the id
    /// the event actually got. See `NmpWorld::id_labels`' doc for why a
    /// binding, not a literal, is the honest reading of the scenario.
    pub fn bind_id_label(&mut self, label: &str) {
        let id = self.signed_event().id;
        self.id_labels.insert(label.to_string(), id);
    }

    /// The real id a scenario's id word stands for.
    pub fn labelled_id(&self, label: &str) -> EventId {
        *self
            .id_labels
            .get(label)
            .unwrap_or_else(|| panic!("nmp-bdd: no event was ever given the id {label:?}"))
    }

    /// `Given that signed event carries no h tag`.
    pub fn assert_signed_event_has_no_context(&self) {
        let parts = self
            .staged_signed_parts
            .as_ref()
            .expect("nmp-bdd: no signed event has been staged");
        assert!(
            !parts
                .tags
                .iter()
                .any(|tag| tag.as_slice().first().map(String::as_str) == Some("h")),
            "nmp-bdd: this scenario stages an event with NO h tag, but one was added"
        );
    }
}
