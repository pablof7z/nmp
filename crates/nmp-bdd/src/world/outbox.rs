//! The OUTBOX plane: the operator-configured relay sets an `Auto` write adds
//! to whatever a relay list said, the three-valued knowledge staging those
//! scenarios turn on, and the route/refusal observables a `Then` reads back.
//!
//! Its own module because "where does an ordinary event go" is answered by
//! things no other family stages: two operator sets (`app_relays`,
//! `fallback_relays`) that belong to nobody in particular, a p-tagged
//! recipient's INBOX rather than their outbox, and the difference between an
//! indexer that has finished looking and one that has not. [`super::staging`]
//! owns how a person's relay list becomes a directory fact; this owns
//! everything the OPERATOR configured and everything a route assertion needs
//! to read.
//!
//! Two staging choices here are load-bearing and easy to get wrong:
//!
//! - **"the indexers have not yet finished" is staged as two indexers, one of
//!   which withholds its end-of-stored-events.** A world whose only indexer
//!   withheld everything could never deliver a relay list that ARRIVES later,
//!   and half these scenarios turn on exactly that arrival. Settlement needs
//!   EVERY source to finish, so one withholding source is enough to keep an
//!   absence unsettled while the other still answers ordinary queries.
//! - **"the indexers finish their stored events" rebinds the withholding
//!   indexer on its own port**, so the engine's own pool reconnects and
//!   replays its discovery subscription there. Nothing is injected into the
//!   directory: the absence settles because a real relay really did say end
//!   of stored events, which is the only thing that may settle one.

use nostr::Tag;

use nmp_grammar::{EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_router::RelayUrl;

use nmp_test_support::relays::ScriptedRelay;

use super::budgets::RECONNECT;
use super::{NmpWorld, ME};

impl NmpWorld {
    // ---- Given: what the operator configured ---------------------------

    /// `Given app relays <list> are configured` -- the set the app itself
    /// wants everything to reach. Every kind, every author, always, additive.
    pub fn configure_app_relays(&mut self, names: &[String]) {
        assert!(
            !self.started,
            "nmp-bdd: state the app-relay topology before anything runs"
        );
        for name in names {
            self.relay_config_mut(name);
            if !self.app_relay_names.contains(name) {
                self.app_relay_names.push(name.clone());
            }
        }
    }

    /// `Given fallback relays <list> are configured` -- the per-recipient
    /// top-up, which an app relay suppresses entirely.
    pub fn configure_fallback_relays(&mut self, names: &[String]) {
        assert!(
            !self.started,
            "nmp-bdd: state the fallback-relay topology before anything runs"
        );
        for name in names {
            self.relay_config_mut(name);
            if !self.fallback_relay_names.contains(name) {
                self.fallback_relay_names.push(name.clone());
            }
        }
    }

    /// `Given no fallback relays are configured` -- the same replacement
    /// semantics as its app-relay twin: a statement about the world's final
    /// topology, clearing whatever a Background configured.
    pub fn no_fallback_relays(&mut self) {
        assert!(
            !self.started,
            "nmp-bdd: state the fallback-relay topology before anything runs"
        );
        self.fallback_relay_names.clear();
    }

    /// Every relay the operator configured as an app relay, by scenario name.
    pub fn app_relay_names(&self) -> &[String] {
        &self.app_relay_names
    }

    /// Every person this scenario has named anywhere. The recipient half of
    /// "no relay outside the author's, the app's, and the recipients'" has to
    /// sweep them all: an addressee's inbox is legitimately contacted, and
    /// which addressees exist is a fact about the scenario rather than about
    /// any one step.
    pub fn people_named(&self) -> Vec<String> {
        self.people.keys().cloned().collect()
    }

    /// The operator sets, resolved to the URLs their relays were bound to.
    pub(super) fn app_relay_urls(&self) -> Vec<RelayUrl> {
        self.app_relay_names
            .iter()
            .map(|name| self.relay_url(name))
            .collect()
    }

    pub(super) fn fallback_relay_urls(&self) -> Vec<RelayUrl> {
        self.fallback_relay_names
            .iter()
            .map(|name| self.relay_url(name))
            .collect()
    }

    // ---- Given: what a relay list says ---------------------------------

    /// `Given my relay list names only <relays> as my write relay(s)` /
    /// `... names only <relay> as a read-marked relay` -- a REPLACEMENT, not
    /// an addition. kind:10002 is replaceable, so a scenario narrowing what
    /// its Background stated is describing the list it actually has.
    pub fn replace_my_relay_list(&mut self, write: &[String], read: &[String]) {
        self.write_relay_of.remove(ME);
        self.read_relay_of.remove(ME);
        for relay in write {
            self.declare_write_relay(ME, relay);
        }
        for relay in read {
            self.declare_read_relay(ME, relay);
        }
        // A list that names only read relays still EXISTS. Recording the
        // empty write half keeps the author `Known` (declares zero write
        // relays) rather than `Unknown`, which is the whole distinction these
        // scenarios are about.
        if write.is_empty() {
            self.declare_no_write_relays(ME);
        }
    }

    /// `Given my relay list declares no write relays` -- a real kind:10002
    /// that names none. An ANSWER, not ignorance.
    pub fn declare_no_write_relays(&mut self, person: &str) {
        self.person(person);
        self.write_relay_of.remove(person);
        if !self.declares_no_write_relays.iter().any(|p| p == person) {
            self.declares_no_write_relays.push(person.to_string());
        }
    }

    /// `Given <person>'s relay list names <relay> without marking it read or
    /// write` -- NIP-65's unmarked entry, which is BOTH halves. The most
    /// common shape in the wild, and the one a read/write split silently
    /// drops if it treats "no marker" as write-only.
    pub fn declare_unmarked_relay(&mut self, person: &str, relay: &str) {
        self.declare_write_relay(person, relay);
        self.declare_read_relay(person, relay);
    }

    // ---- Given: three-valued knowledge, and the sources behind it -------
    //
    // Moved here from `staging` because these are not staging of a PERSON's
    // protocol state at all -- they are staging of what the engine has been
    // ABLE TO LEARN, which is the axis this whole family turns on. A relay
    // list naming relays is `Known`, one declaring none is still `Known` (a
    // fact, just an empty one), and one never ingested is `Unknown` until the
    // sources finish looking.

    /// `Given <person>'s relay list names <relay> as their read relay` --
    /// their INBOX, which is what a p-tag fan-out reaches them at.
    pub fn declare_read_relay(&mut self, person: &str, relay: &str) {
        self.person(person);
        self.relay_config_mut(relay);
        let relays = self.read_relay_of.entry(person.to_string()).or_default();
        if !relays.iter().any(|r| r == relay) {
            relays.push(relay.to_string());
        }
    }

    /// `Given <person>'s relay list is ingested and names no relays at all`
    /// -- a REAL kind:10002 declaring nothing. Settled knowledge, not
    /// ignorance: a write mentioning them completes routing without ever
    /// parking on them.
    pub fn declare_no_relays(&mut self, person: &str) {
        self.person(person);
        if !self.declares_no_relays.iter().any(|p| p == person) {
            self.declares_no_relays.push(person.to_string());
        }
    }

    /// `Given <person>'s relay list has never been fetched` / `no relay list
    /// for <person> has ever been ingested` -- the world states out loud that
    /// it staged nothing for them, so a scenario cannot pass because a
    /// `Given` was silently forgotten.
    pub fn assert_relay_list_never_fetched(&self, person: &str) {
        assert!(
            !self.write_relay_of.contains_key(person)
                && !self.read_relay_of.contains_key(person)
                && !self.declares_no_relays.iter().any(|p| p == person)
                && !self.declares_no_write_relays.iter().any(|p| p == person),
            "nmp-bdd: {person}'s relay list is staged, so it HAS been fetched"
        );
    }

    /// Every configured indexer stops answering end-of-stored-events, which
    /// is what "we have not finished looking" is on the wire. Nothing can
    /// settle from here, so every unknown stays `Unknown` and every write
    /// depending on one stays parked.
    pub fn indexers_never_confirm_end_of_stored_events(&mut self) {
        assert!(
            !self.indexer_names.is_empty(),
            "nmp-bdd: an indexer must be configured before it can withhold its EOSE"
        );
        for name in self.indexer_names.clone() {
            self.set_reject_queries(&name);
        }
    }

    /// The complement: a well-behaved indexer answers end-of-stored-events,
    /// which is the DEFAULT here. Stated out loud where a scenario turns on
    /// it, so a settlement that happens cannot be mistaken for one the
    /// harness arranged behind the engine's back.
    pub fn assert_indexers_confirm_end_of_stored_events(&self) {
        assert!(
            !self.indexer_names.is_empty(),
            "nmp-bdd: nothing settles without a source; configure an indexer first"
        );
        for name in &self.indexer_names {
            assert!(
                !self
                    .relay_configs
                    .get(name)
                    .is_some_and(|config| config.reject_queries),
                "nmp-bdd: indexer {name:?} was staged to withhold its EOSE, so it cannot \
                 also be the source that settles an absence"
            );
        }
    }

    /// `Given no indexer relays are configured` -- fail-closed by
    /// construction: with no source to ask, nothing can ever settle.
    pub fn assert_no_indexers(&self) {
        assert!(
            self.indexer_names.is_empty(),
            "nmp-bdd: state the indexer topology before anything runs"
        );
    }

    /// `Given no app relays are configured` -- a statement about the world's
    /// final topology, so it CLEARS whatever a Background configured rather
    /// than merely asserting. A feature whose Background gives every scenario
    /// an app relay still needs one scenario without: "always additive" is
    /// only falsifiable against the empty set.
    pub fn no_app_relays(&mut self) {
        assert!(
            !self.started,
            "nmp-bdd: state the app-relay topology before anything runs"
        );
        self.app_relay_names.clear();
    }

    // ---- Given/When: whether the sources have finished looking ----------

    /// UNSTAGE every relay list fact about `person`.
    ///
    /// ALL FOUR staging maps, because a relay list is all four: the outbox
    /// half, the inbox half, the "ingested and declares nothing" fact, and
    /// the "ingested and declares no WRITE relay" one. Clearing fewer would
    /// leave an author the directory still answers `Known` for, and a
    /// scenario saying nobody has looked them up would then be testing
    /// `Known`-with-no-relays instead of `Unknown` -- the exact confusion
    /// three-valued knowledge exists to prevent.
    pub fn forget_relay_list(&mut self, person: &str) {
        self.write_relay_of.remove(person);
        self.read_relay_of.remove(person);
        self.declares_no_relays.retain(|p| p != person);
        self.declares_no_write_relays.retain(|p| p != person);
    }

    /// `Given the indexers have finished their stored events without a relay
    /// list for <person>` -- they are well-behaved and this person has none.
    ///
    /// Nothing about the settlement is staged: the absence settles on the
    /// wire, when the discovery subscription that this write's own declared
    /// need opens really does reach end-of-stored-events at every source.
    /// What IS staged is the only half a harness owns -- that there is no
    /// list for them to have found.
    pub fn indexers_finished_without_a_list_for(&mut self, person: &str) {
        self.person(person);
        self.ensure_indexers();
        self.forget_relay_list(person);
    }

    /// `Given the indexers have not yet finished their stored events for
    /// <person>` -- one configured indexer withholds its
    /// end-of-stored-events, so nothing can settle while the OTHER still
    /// answers ordinary queries (which is what lets a relay list arrive
    /// later at all).
    pub fn indexers_have_not_finished(&mut self, person: &str) {
        self.person(person);
        self.ensure_indexers();
        // Not having finished looking for them means not having their list:
        // a scenario that said this while a Background staged one would be
        // exercising `Known` under a sentence that says `Unknown`.
        self.forget_relay_list(person);
        let withholding = self
            .indexer_names
            .last()
            .cloned()
            .expect("nmp-bdd: ensure_indexers guarantees at least two");
        self.set_reject_queries(&withholding);
        if !self.withholding_indexers.contains(&withholding) {
            self.withholding_indexers.push(withholding);
        }
    }

    /// `When the indexers finish their stored events without a relay list for
    /// <person>` -- the withholding source starts answering. Rebound on its
    /// own port so the engine's pool reconnects and replays its discovery
    /// subscription there; the EOSE that settles the absence is a real frame
    /// from a real relay, never an injected fact.
    pub async fn indexers_finish_stored_events(&mut self) {
        assert!(
            !self.withholding_indexers.is_empty(),
            "nmp-bdd: no indexer was withholding its end-of-stored-events, so there is \
             nothing here for finishing to change"
        );
        for name in std::mem::take(&mut self.withholding_indexers) {
            self.relay_config_mut(&name).reject_queries = false;
            let port = self.relays[&name].port();
            let config = self.relay_configs[&name].clone();
            self.relays
                .get_mut(&name)
                .expect("nmp-bdd: a withholding indexer is a started relay")
                .disconnect()
                .await;
            let fresh = ScriptedRelay::start_on_port(port, &config).await;
            assert!(
                fresh.wait_contacted(RECONNECT).await,
                "nmp-bdd: indexer {name:?} was not recontacted within {RECONNECT:?} of \
                 answering again -- the engine's pool did not replay its discovery \
                 subscription, so no end-of-stored-events could arrive"
            );
            self.relays.insert(name, fresh);
        }
    }

    /// At least two indexers, because settlement needs EVERY source to finish
    /// and half these scenarios need one source to keep answering while
    /// another withholds.
    fn ensure_indexers(&mut self) {
        if self.indexer_names.len() < 2 {
            self.configure_n_indexers(2);
        }
    }

    // ---- When: publishing ----------------------------------------------

    /// `When I publish my profile` -- an ordinary kind:0 through the ordinary
    /// door, saying nothing about relays. The whole point of the app-relay
    /// scenarios is that this is not a special case.
    pub async fn publish_profile(&mut self) {
        self.publish_bare_kind(0, "{\"name\":\"me\"}").await;
    }

    /// `When I publish a kind <n> event`.
    pub async fn publish_kind(&mut self, kind: u16) {
        self.publish_bare_kind(kind, "an event of this kind").await;
    }

    async fn publish_bare_kind(&mut self, kind: u16, content: &str) {
        self.ensure_started().await;
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: publishing needs a logged-in account");
        let _ = self.person(&me);
        let mut builder = EventBuilder::new(nostr::Kind::from(kind)).content(content);
        if (30_000..=39_999).contains(&kind) {
            builder = builder.tag(Tag::identifier("d"));
        }
        self.publish_intent(WriteIntent {
            payload: WritePayload::Event(builder),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        });
    }
}
