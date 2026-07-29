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

use std::collections::BTreeSet;

use nostr::{Tag, Timestamp};

use nmp_grammar::{Durability, EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_router::RelayUrl;

use nmp::mechanism::core::StalledWriteStage;
use nmp::mechanism::outbox::WriteStatus;
use nmp_test_support::relays::ScriptedRelay;

use super::budgets::{EVENTUALLY, RECONNECT};
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
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        });
    }

    // ---- Then: where the write was routed -------------------------------

    /// The relay set the receipt reported, resolved from scenario names.
    fn urls_of(&self, names: &[String]) -> BTreeSet<RelayUrl> {
        names.iter().map(|name| self.relay_url(name)).collect()
    }

    /// `Then the <thing> is routed to exactly <relays>` -- the WHOLE answer,
    /// read off the receipt's own routing picture.
    pub fn routed_exactly(&mut self, names: &[String]) -> bool {
        let wanted = self.urls_of(names);
        self.receipt_eventually(
            |seen| matches!(seen.iter().rev().find(|s| matches!(s, WriteStatus::Routed { .. })), Some(WriteStatus::Routed { relays, .. }) if *relays == wanted),
        )
    }

    /// `Then the <thing> is routed to <relay>` -- a member of the answer,
    /// which is the right claim when a scenario names one source of several.
    pub fn routed_to(&mut self, name: &str) -> bool {
        let url = self.relay_url(name);
        self.receipt_eventually(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { relays, .. } if relays.contains(&url)))
        })
    }

    /// `Then the note is never routed to <relay>` -- costs its full negative
    /// budget, and is a claim about the ROUTE rather than about contact: a
    /// relay may be contacted for a read and still never be a destination.
    pub fn never_routed_to(&mut self, name: &str) -> bool {
        let url = self.relay_url(name);
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { relays, .. } if relays.contains(&url)))
        })
    }

    /// `Then routing is complete` -- zero unknowns remain, so the answer can
    /// never change again.
    pub fn routing_is_complete(&mut self) -> bool {
        self.receipt_eventually(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { complete: true, .. }))
        })
    }

    /// `Then routing is not complete`.
    pub fn routing_stays_open(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { complete: true, .. }))
        })
    }

    /// `Then the note is routed to no relay` -- no destination was ever
    /// named, which is different from one being named and never delivered to.
    pub fn routed_nowhere(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { relays, .. } if !relays.is_empty()))
        })
    }

    /// The FINAL route of a publish named by order -- what "the profile and
    /// the note are routed to the same relays" compares.
    pub fn final_route_at(&mut self, ordinal: usize) -> BTreeSet<RelayUrl> {
        // Wait for a route to exist at all before reading it, or an empty
        // answer would compare equal to another empty answer and prove
        // nothing.
        let _ = self.receipt_eventually_at(ordinal, |seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { complete: true, .. }))
        });
        self.receipt_statuses_at(ordinal)
            .iter()
            .rev()
            .find_map(|s| match s {
                WriteStatus::Routed { relays, .. } => Some(relays.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    // ---- Then: the refusal ----------------------------------------------

    /// Bounded wait for a park whose reason contains `needle`.
    ///
    /// A wait rather than a read, because a park's REASON converges: the same
    /// write is first parked on "nobody has looked yet" and later, once every
    /// source has finished, on "there is nothing to find". Both are true in
    /// turn and the second is the one a scenario about settled absence means,
    /// so reading the first reason to arrive would assert the opposite of
    /// what the scenario says.
    pub fn park_reason_contains(&mut self, needle: &str) -> bool {
        let owned = needle.to_string();
        let matches = move |seen: &[WriteStatus]| {
            seen.iter()
                .filter_map(park_reason)
                .any(|reason| reason.contains(&owned))
        };
        if self.restarted_receipt.is_some() {
            return self.restarted_receipt_eventually(matches);
        }
        self.receipt_eventually(matches)
    }

    /// Every park reason this write has reported so far, newest last -- for
    /// assertion MESSAGES and for the diagnostics cross-check, never as a
    /// substitute for the bounded wait above. Reads the REATTACHED stream
    /// after a restart, because on the far side of a process boundary that is
    /// the only stream that exists.
    pub fn park_reasons(&mut self) -> Vec<String> {
        let reasoned = |seen: &[WriteStatus]| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::AwaitingRoute { .. }))
        };
        if self.restarted_receipt.is_some() {
            let _ = self.restarted_receipt_eventually(reasoned);
            return self
                .restarted_receipt_statuses()
                .iter()
                .filter_map(park_reason)
                .collect();
        }
        let _ = self.receipt_eventually(reasoned);
        self.receipt_statuses()
            .iter()
            .filter_map(park_reason)
            .collect()
    }

    /// `Then the publish reports no routing problem` -- the negative form,
    /// costing its own budget: nothing ever parked this write.
    pub fn never_parked(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::AwaitingRoute { .. }))
        })
    }

    /// `Then the note is never reported as sent`.
    pub fn never_sent(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter().any(|s| {
                matches!(
                    s,
                    WriteStatus::Sent { .. }
                        | WriteStatus::Acked(_)
                        | WriteStatus::HandoffAmbiguous { .. }
                )
            })
        })
    }

    /// `Then the publish has not failed`.
    pub fn never_failed(&mut self) -> bool {
        self.receipt_never(|seen| seen.iter().any(|s| matches!(s, WriteStatus::Failed(_))))
    }

    // ---- Then: what reached a socket ------------------------------------

    /// How many copies of `event` the named relay actually admitted -- the
    /// wire-side count behind "offered exactly once".
    pub fn copies_admitted(&self, relay: &str, event: nostr::EventId) -> usize {
        self.relays
            .get(relay)
            .map(|r| {
                r.admitted_events()
                    .into_iter()
                    .filter(|e| e.id == event)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Wait (bounded) for `relay` to admit `event` at all.
    ///
    /// The precondition of every count: a receipt beat and a socket write are
    /// not the same instant, so reading the relay's log the moment routing
    /// reported a destination says only that nothing has arrived YET. Waiting
    /// is also strictly safer for the count that follows -- it gives a second
    /// copy more time to show up, never less.
    pub async fn wait_for_copy(&self, relay: &str, event: nostr::EventId) -> bool {
        let deadline = std::time::Instant::now() + EVENTUALLY;
        loop {
            if self.copies_admitted(relay, event) > 0 {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Wait (bounded) for ANY relay in this world to be contacted -- the
    /// precondition of every "nothing outside X was contacted" claim, which
    /// on an empty world is true and worthless.
    pub async fn wait_any_relay_contacted(&self) -> bool {
        let deadline = std::time::Instant::now() + EVENTUALLY;
        loop {
            if self.any_relay_contacted() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Every relay this world ever admitted the given event at.
    pub fn relays_holding(&self, event: nostr::EventId) -> Vec<String> {
        self.relay_order
            .iter()
            .filter(|name| self.copies_admitted(name, event) > 0)
            .cloned()
            .collect()
    }

    /// Every relay the ENGINE planned a session to, by scenario name where
    /// this world knows one and by URL where it does not. A URL with no name
    /// is a relay nobody configured -- exactly what "no relay outside the
    /// ones configured" is looking for.
    pub fn planned_relays(&mut self) -> Vec<String> {
        let known: Vec<(RelayUrl, String)> = self
            .relay_order
            .iter()
            .filter(|name| self.relays.contains_key(*name))
            .map(|name| (self.relay_url(name), name.clone()))
            .collect();
        let Some(snapshot) = self.diagnostics_matching(|snap| !snap.relays.is_empty()) else {
            return Vec::new();
        };
        snapshot
            .relays
            .iter()
            .map(|row| {
                known
                    .iter()
                    .find(|(url, _)| *url == row.relay)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| row.relay.to_string())
            })
            .collect()
    }

    /// `Then diagnostics reports the note among the stalled writes` -- the
    /// engine-global answer to "is anything quietly stuck", which no single
    /// receipt can give.
    ///
    /// Narrowed to `Unroutable`, which is the only stage a scenario about the
    /// OUTBOX can produce: the write is signed and its author has a signer, so
    /// it is neither unsignable nor undeliverable -- it has nowhere to be
    /// delivered TO. Reading every stage would let this pass on a stall that
    /// had nothing to do with routing.
    ///
    /// A bounded WAIT, and named apart from #1025's [`Self::stalled_writes`]
    /// for exactly that reason: that one reads the snapshot a scenario
    /// explicitly captured, which is right when the scenario says `When I read
    /// the diagnostics`. An outbox scenario never says that -- it publishes
    /// and asks -- so it has to wait for the census to move on its own.
    pub fn unroutable_writes(&mut self) -> Vec<(String, Timestamp)> {
        let unroutable = |snap: &nmp::mechanism::core::DiagnosticsSnapshot| {
            snap.stalled_writes
                .iter()
                .any(|stalled| stalled.stage == StalledWriteStage::Unroutable)
        };
        let Some(snapshot) = self.diagnostics_matching(unroutable) else {
            return Vec::new();
        };
        snapshot
            .stalled_writes
            .iter()
            .filter(|stalled| stalled.stage == StalledWriteStage::Unroutable)
            .map(|stalled| (stalled.detail.clone(), stalled.stalled_since))
            .collect()
    }

    /// The engine's clock as this world last saw it before publishing -- the
    /// lower bound a stalled entry's `stalled_since` must fall above, so
    /// "how long it has been so" is a fact the engine recorded rather than a
    /// number it made up.
    pub fn last_publish_at(&self) -> Timestamp {
        self.last_publish_at
            .expect("nmp-bdd: nothing has been published in this scenario")
    }

    /// A bounded wait for the diagnostics stream to say anything at all --
    /// the precondition behind every claim about what it does NOT contain.
    pub fn diagnostics_ran(&mut self) -> bool {
        self.diagnostics_matching(|_| true).is_some()
    }

    /// Give the world one full observation budget of quiet. Used by the
    /// steps whose claim is about the absence of contact, so the world has
    /// had as long to do the wrong thing as any positive assertion allows.
    pub async fn settle(&mut self) {
        tokio::time::sleep(EVENTUALLY).await;
    }
}

fn park_reason(status: &WriteStatus) -> Option<String> {
    match status {
        WriteStatus::AwaitingRoute { detail } => Some(detail.clone()),
        _ => None,
    }
}
