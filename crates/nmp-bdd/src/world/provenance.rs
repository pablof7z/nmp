//! Read provenance: WHICH RELAYS served a row the app is looking at.
//!
//! Its own module because provenance is a third axis to everything the other
//! world modules track. `observe` owns the delta fold, `actions` owns the
//! stimulus, `groups` owns the NIP-29 door -- none of them is about the
//! question this one answers, which is: for a row already on the feed, who
//! delivered it, and can the app tell one relay's copy from two relays'.
//!
//! It owns both halves of that question, and they belong together:
//!
//! - the FIXTURE that makes two relays disagree. NIP-29 group metadata
//!   (kind 39000) is signed by the HOST RELAY, so one group id genuinely
//!   exists twice, signed by two different keys, with two different contents.
//!   Manufacturing that needs a per-host signing key, which is what
//!   [`NmpWorld::stage_host_signed_group_metadata`] mints -- unlike
//!   `world::groups`' fixtures, which are all signed by one member key
//!   because no group scenario before this one cared who signed.
//! - the OBSERVER a `Then` reads it through
//!   ([`NmpWorld::row_sources_eventually`]), which is the only way out of the
//!   fold's `Row::sources` and therefore the only thing a provenance
//!   assertion can be written against.

use std::collections::BTreeSet;

use nostr::{Kind, Tag, Timestamp, UnsignedEvent};

use nmp_router::RelayUrl;

use super::budgets::EVENTUALLY;
use super::observe::FeedState;
use super::queries::{authored_note_query_from_relays, group_metadata_query};
use super::NmpWorld;

impl NmpWorld {
    /// `When I read P's notes from relays A and B` -- a literal-author query
    /// pinned to every named relay.
    ///
    /// A follows query would exercise the bounded outbox solver, which may
    /// satisfy coverage without contacting every candidate. This door makes
    /// the provenance scenario's precondition mechanical: both named relays
    /// own the live demand and must each serve their copy of the same event.
    pub async fn open_authored_notes_from_relays(&mut self, person: &str, relay_names: &[String]) {
        self.ensure_started().await;
        let author = self.person(person).public_key().to_hex();
        let relays: BTreeSet<RelayUrl> = relay_names
            .iter()
            .map(|name| self.relay_url(name))
            .collect();
        let (handle_id, rx) = self
            .handle()
            .subscribe(authored_note_query_from_relays(relays, &author, None))
            .expect("BDD subscription construction");
        self.feed = Some(FeedState::new(handle_id, rx));
    }

    /// `Given relay "R" hosts group "G" with metadata saying "..."` -- a
    /// kind-39000 signed by THAT HOST's own key.
    ///
    /// Staged rather than seeded, like every other `Given` (see
    /// `staging`'s doc): a scenario names two hosts on two lines, and seeding
    /// on the first line would start the world before the second relay
    /// existed to be bound.
    pub fn stage_host_signed_group_metadata(&mut self, relay: &str, group_id: &str, name: &str) {
        assert!(
            !self.started,
            "nmp-bdd: stage every host's group metadata before the world starts"
        );
        self.relay_config_mut(relay);
        self.pending_group_metadata.push((
            relay.to_string(),
            group_id.to_string(),
            name.to_string(),
        ));
    }

    /// Seed one staged host-signed kind-39000, called by `ensure_started`
    /// once every relay is bound.
    ///
    /// The signing key is derived from the RELAY's name, so two hosts of one
    /// group id are two different authors -- which is the whole point: the
    /// addressable coordinate includes the pubkey, so these two events are
    /// two rows rather than one winner, and each row's provenance names the
    /// host that signed it.
    pub(super) async fn seed_staged_group_metadata(&mut self) {
        for (relay, group_id, name) in std::mem::take(&mut self.pending_group_metadata) {
            let host = self.person(&format!("host-of-{relay}"));
            let created_at = self.next_created_at();
            let event = UnsignedEvent::new(
                host.public_key(),
                Timestamp::from(created_at),
                Kind::from(39_000u16),
                vec![Tag::identifier(group_id)],
                name,
            )
            .sign_with_keys(&host)
            .expect("nmp-bdd: a host metadata fixture must sign cleanly");
            self.relays[&relay].seed_signed_event(&event).await;
        }
    }

    /// `When I read the metadata for group "G" from relays "A" and "B"` --
    /// one feed, pinned to both hosts, selecting the group's coordinate and
    /// naming no author. Naming no author is load-bearing: an author-scoped
    /// read could only ever return one host's version, so it could not
    /// observe divergence at all.
    pub async fn open_group_metadata_feed(&mut self, group_id: &str, relay_names: &[String]) {
        self.ensure_started().await;
        let relays: BTreeSet<RelayUrl> = relay_names
            .iter()
            .map(|name| self.relay_url(name))
            .collect();
        let (handle_id, rx) = self
            .handle()
            .subscribe(group_metadata_query(relays, group_id))
            .expect("BDD subscription construction");
        self.feed = Some(FeedState::new(handle_id, rx));
    }

    /// The relay URLs a scenario named, as a row's `sources` set would spell
    /// them.
    pub fn relay_urls(&self, names: &[String]) -> BTreeSet<RelayUrl> {
        names.iter().map(|name| self.relay_url(name)).collect()
    }

    /// Bounded-wait until the feed holds a row whose content is `content` AND
    /// whose source set is exactly `expected`.
    ///
    /// Exactly, not "contains": "delivered by both relays" and "delivered by
    /// only this one" are the two claims these scenarios distinguish, and a
    /// superset test would make the second one unfalsifiable.
    pub fn row_sources_eventually(&mut self, content: &str, expected: &BTreeSet<RelayUrl>) -> bool {
        self.feed_rows_eventually(|rows| {
            rows.iter()
                .any(|(text, sources)| text == content && sources == expected)
        })
    }

    /// Whether ANY row saying `content` is on the feed at all -- the
    /// precondition behind every assertion above (`then`'s empty-world rule:
    /// a provenance check against a row that never arrived would pass or fail
    /// for the wrong reason).
    pub fn feed_holds_row_saying(&mut self, content: &str) -> bool {
        self.feed_rows_eventually(|rows| rows.iter().any(|(text, _)| text == content))
    }

    /// Bounded-wait until the feed holds exactly `n` rows, and report how
    /// many it settled on. Returned rather than asserted so a failure can
    /// say what the count actually was.
    pub fn feed_row_count_eventually(&mut self, n: usize) -> usize {
        self.feed_rows_eventually(|rows| rows.len() == n);
        self.row_provenance().len()
    }

    /// Every row on the feed as (content, sources) -- for failure messages,
    /// never as a substitute for a bounded wait.
    pub fn row_provenance(&mut self) -> Vec<(String, BTreeSet<RelayUrl>)> {
        let Some(feed) = self.feed.as_mut() else {
            return Vec::new();
        };
        feed.drain_available();
        feed.rows
            .values()
            .map(|row| (row.content().to_string(), row.sources().clone()))
            .collect()
    }

    /// The one bounded read every observer above shares: drains the delta
    /// stream until `pred` holds over the accumulated (content, sources)
    /// pairs, or the window runs out.
    fn feed_rows_eventually(
        &mut self,
        pred: impl Fn(&[(String, BTreeSet<RelayUrl>)]) -> bool,
    ) -> bool {
        let feed = self.feed.as_mut().expect("nmp-bdd: no feed is open");
        feed.eventually(EVENTUALLY, |f| {
            let rows: Vec<(String, BTreeSet<RelayUrl>)> = f
                .rows
                .values()
                .map(|row| (row.content().to_string(), row.sources().clone()))
                .collect();
            pred(&rows)
        })
    }
}
