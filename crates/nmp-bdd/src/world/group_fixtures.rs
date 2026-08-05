//! The unsigned draft a scenario hands the group door.
//!
//! Split from [`super::groups`] because it is staging rather than acting: the
//! draft the app composed, which the group contextualizes before signing.
//!
//! #1292 deleted the pre-signed half of this file along with the group's
//! pre-signed publish door: an app no longer hands NMP bytes it signed
//! itself, so there is nothing left to stage unchanged, and the id LABELS
//! that existed only so a `.feature` could name an id known before
//! publication went with it.

use nostr::{Kind, Tag, Timestamp};

use nmp_grammar::EventBuilder;

use super::groups::APP_CHOSEN_CREATED_AT;
use super::NmpWorld;

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
}
