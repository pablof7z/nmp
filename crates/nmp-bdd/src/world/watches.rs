//! Watching ONE named relay directly -- the concern behind
//! `features/routing/subscription-collapse.feature` and the per-relay
//! subscription budget (#931).
//!
//! This is a genuinely different concern from the feed in
//! [`super::actions`], not just more of it. A feed is observed through the
//! app-facing delta channel; a watch exists so the suite can observe what NMP
//! puts on a SOCKET, which means the world has to keep bookkeeping no feed
//! ever needs: which values of which tag (and which authors) are watched
//! RIGHT NOW, so that "every value I watch" has a referent; a pin target, so
//! the scenario does not also depend on relay discovery; and a way to wait
//! for the client wire to go quiet before any count is read.
//!
//! The NIP-29 group fixtures live here for the same reason. `Given I
//! administer <n> groups` exists only to give the derived `#d` set something
//! to resolve to, so its staging, its seeding, and the watch it feeds are one
//! story and split badly.

use std::collections::BTreeSet;
use std::time::Instant;

use nostr::{Tag, Timestamp};

use nmp_resolver::LiveQuery;
use nmp_router::RelayUrl;

use nmp_test_support::relays::WireRecord;

use super::budgets::{WIRE_QUIET, WIRE_SETTLE};
use super::observe::FeedState;
use super::queries::{
    authored_note_query, my_group_state_query, tagged_note_query, tagged_note_query_values,
    WatchShape, GROUP_ADMINS_KIND,
};
use super::NmpWorld;

/// How many independent app watches the scale step splits its values across.
///
/// One over the 20-subscription relay ceiling the collapse scenario asserts,
/// and deliberately so: that is the whole falsifier. Remove the structural
/// union and these 21 watches reach the wire as 21 subscriptions, which is
/// already too many, so the scenario fails without needing 1,200 of them.
const UNCOALESCED_CATALOG_WATCHES: usize = 21;

/// Deal `value-0001..value-{n}` round-robin into [`UNCOALESCED_CATALOG_WATCHES`]
/// batches (fewer, if `n` is smaller than that).
///
/// Round-robin rather than contiguous runs so no batch is a naturally
/// compact range: the coalescer must union genuinely interleaved value sets,
/// as it would for a catalog an app discovered in arbitrary order.
fn catalog_value_batches(n: usize) -> Vec<BTreeSet<String>> {
    let batch_count = n.min(UNCOALESCED_CATALOG_WATCHES);
    if batch_count == 0 {
        return Vec::new();
    }

    let mut batches = vec![BTreeSet::new(); batch_count];
    for i in 1..=n {
        batches[(i - 1) % batch_count].insert(format!("value-{i:04}"));
    }
    batches
}

impl NmpWorld {
    // ---- wire subscription aggregation --------------------------------

    /// `Given relay <name> is the relay I watch directly` -- registers the
    /// relay (well-behaved by default; a later `Given` may still reconfigure
    /// it) and names it as the pin target for every later `watch` step.
    pub fn set_watch_relay(&mut self, name: &str) {
        self.relay_config_mut(name);
        self.watch_relay = Some(name.to_string());
    }

    fn watch_relay_url(&self) -> RelayUrl {
        let name = self
            .watch_relay
            .as_ref()
            .expect("nmp-bdd: no relay has been named as the one I watch directly");
        self.relays[name].url.clone()
    }

    async fn open_watch(&mut self, key: String, query: LiveQuery) {
        let (handle, rx) = self
            .handle()
            .subscribe(query)
            .expect("nmp-bdd: watch subscription construction");
        self.watches.insert(key, FeedState::new(handle, rx));
    }

    /// `When I watch for notes tagged <tag> as <value>`, plus the shaped
    /// variants (`the latest N notes tagged ...`, `... from the last N days`)
    /// whose whole point is that they must NOT merge with an unshaped
    /// sibling.
    ///
    /// The watch key carries the shape, so two watches for the same value
    /// under different shapes are two distinct open watches rather than one
    /// silently overwriting the other.
    pub async fn watch_tag_value_shaped(&mut self, tag: char, value: &str, shape: WatchShape) {
        self.ensure_started().await;
        let url = self.watch_relay_url();
        self.watched_tag_values
            .entry(tag)
            .or_default()
            .insert(value.to_string());
        let key = match (shape.limit, shape.since) {
            (None, None) => format!("{tag}={value}"),
            (limit, since) => format!("{tag}={value}/limit={limit:?}/since={since:?}"),
        };
        self.open_watch(key, tagged_note_query(&url, tag, value, shape))
            .await;
    }

    /// `When I watch for notes tagged <tag> as <value>`.
    pub async fn watch_tag_value(&mut self, tag: char, value: &str) {
        self.watch_tag_value_shaped(tag, value, WatchShape::default())
            .await;
    }

    /// `When I watch for notes tagged <tag> as <n> different values` -- the
    /// scale shape (a catalog of groups, a directory of channels), where the
    /// fan-out is the difference between one relay connection working and
    /// hitting its concurrent-subscription ceiling.
    ///
    /// The scale scenario needs 1,200 VALUES but not 1,200 synchronous app
    /// handles. Opening each singleton separately forced 1,200 whole-plan
    /// recompiles and made the test harness itself superlinear (#994), even
    /// though the router coalesces the identical 1,200-atom final bag in
    /// milliseconds. Twenty-one independent batches preserve the falsifier:
    /// without coalescing they exceed the scenario's 20-subscription relay
    /// ceiling; with coalescing every value must still survive into filters
    /// bounded at 500 values apiece.
    pub async fn watch_n_tag_values(&mut self, tag: char, n: usize) {
        self.ensure_started().await;
        let url = self.watch_relay_url();
        for (index, values) in catalog_value_batches(n).into_iter().enumerate() {
            self.watched_tag_values
                .entry(tag)
                .or_default()
                .extend(values.iter().cloned());
            self.open_watch(
                format!("{tag}=catalog-batch-{index:02}"),
                tagged_note_query_values(&url, tag, values, WatchShape::default()),
            )
            .await;
        }
    }

    /// `When I stop watching notes tagged <tag> as <value>`. Explicitly
    /// withdrawn through `Handle::unsubscribe`, never left to a drop.
    pub async fn stop_watching_tag_value(&mut self, tag: char, value: &str) {
        let watch = self
            .watches
            .remove(&format!("{tag}={value}"))
            .unwrap_or_else(|| panic!("nmp-bdd: no open watch for #{tag} = {value:?}"));
        self.handle().unsubscribe(watch.handle);
        if let Some(values) = self.watched_tag_values.get_mut(&tag) {
            values.remove(value);
        }
    }

    /// `When I watch for notes from <person>` / `... the latest <n> notes
    /// from <person>` -- the author-axis control.
    pub async fn watch_author(&mut self, person: &str, limit: Option<usize>) {
        self.ensure_started().await;
        let url = self.watch_relay_url();
        let author_hex = self.person(person).public_key().to_hex();
        self.watched_authors.insert(author_hex.clone());
        self.open_watch(
            format!("author={person}"),
            authored_note_query(&url, &author_hex, limit),
        )
        .await;
    }

    /// `When I stop watching notes from <person>`.
    pub async fn stop_watching_author(&mut self, person: &str) {
        let author_hex = self.person(person).public_key().to_hex();
        let watch = self
            .watches
            .remove(&format!("author={person}"))
            .unwrap_or_else(|| panic!("nmp-bdd: no open watch for {person}"));
        self.handle().unsubscribe(watch.handle);
        self.watched_authors.remove(&author_hex);
    }

    /// Every value of `tag` currently watched.
    pub fn watched_tag_values(&self, tag: char) -> BTreeSet<String> {
        self.watched_tag_values
            .get(&tag)
            .cloned()
            .unwrap_or_default()
    }

    /// Every author currently watched, as hex.
    pub fn watched_authors(&self) -> BTreeSet<String> {
        self.watched_authors.clone()
    }

    /// Block until EVERY started relay's client-to-relay wire has been silent
    /// for a whole [`WIRE_QUIET`] window. Every wire assertion goes through
    /// here first: see that constant's doc for why an unsettled count is an
    /// artifact rather than a fact.
    pub async fn wire_settled(&self) {
        for relay in self.relays.values() {
            relay.wait_wire_quiet(WIRE_QUIET, WIRE_SETTLE).await;
        }
    }

    /// Settle the wire, then keep re-reading `relay`'s record until `pred`
    /// holds, bounded by [`WIRE_SETTLE`].
    ///
    /// QUIESCENCE ALONE IS NOT ENOUGH to establish that something downstream
    /// of an INBOUND frame has happened. `wait_wire_quiet` watches
    /// CLIENT-TO-RELAY traffic only, so the sequence "seed a kind:39001 ->
    /// relay pushes the EVENT -> the client ingests it, re-resolves the
    /// derived set, recompiles and emits a REQ" has a genuinely quiet client
    /// wire in the MIDDLE of it.
    ///
    /// USED FOR SEQUENCING A STIMULUS, NOT FOR TAKING AN ASSERTION. A quiet
    /// outbound socket cannot prove that an inbound EVENT has traversed
    /// ingestion, resolution, and recompilation. The subscription-collapse
    /// feature's ordinary tag/author scenarios now separately exclude the
    /// NIP-77 capability crossover that caused #1004; its derived-group
    /// scenario still uses this helper only to make the initial outer REQ
    /// observable before injecting one more inbound group (§8.1c).
    pub async fn wire_record_when(
        &self,
        relay: &str,
        pred: impl Fn(&WireRecord) -> bool,
    ) -> WireRecord {
        let deadline = Instant::now() + WIRE_SETTLE;
        loop {
            self.wire_settled().await;
            let record = self.wire_record(relay);
            if pred(&record) || Instant::now() >= deadline {
                return record;
            }
            tokio::time::sleep(WIRE_QUIET).await;
        }
    }

    /// `Given I administer <n> groups` -- one kind:39001 (NIP-29 group
    /// admins) fixture per group, each naming me, staged at the watched
    /// relay. These are what the group-state query's inner demand resolves
    /// from, so they also define what "every `d` value I watch" means.
    pub fn stage_administered_groups(&mut self, n: usize) {
        for _ in 0..n {
            self.group_counter += 1;
            let group = format!("group-{:04}", self.group_counter);
            self.watched_tag_values
                .entry('d')
                .or_default()
                .insert(group.clone());
            self.pending_groups.push(group);
        }
    }

    /// The kind:39001 event that makes me an admin of `group`, seeded into
    /// the watched relay exactly as a real relay would already hold it.
    pub(super) async fn seed_group_admins(&mut self, group: &str) {
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: administering a group needs a logged-in account");
        let me_pk = self.person(&me).public_key();
        let host = self.person("group-host");
        let created_at = self.next_created_at();
        let event = nostr::EventBuilder::new(nostr::Kind::from(GROUP_ADMINS_KIND), "")
            .tags([Tag::identifier(group), Tag::public_key(me_pk)])
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(&host)
            .expect("nmp-bdd: a group-admins fixture must sign cleanly");
        let relay = self
            .watch_relay
            .clone()
            .expect("nmp-bdd: no relay has been named as the one I watch directly");
        self.relays[&relay].seed_signed_event(&event).await;
    }

    /// `When I open the group state of every group I administer`.
    pub async fn open_group_state_watch(&mut self) {
        self.ensure_started().await;
        let url = self.watch_relay_url();
        self.open_watch("group-state".to_string(), my_group_state_query(&url))
            .await;
    }

    /// `When I am made an admin of one more group` -- a LIVE kind:39001,
    /// landing at the relay after the watch is already open, so the value set
    /// grows through the same ingest path a real admin grant would take.
    ///
    /// "AFTER THE WATCH IS ALREADY OPEN" IS ENFORCED HERE, not assumed. The
    /// preceding step opens the watch and returns as soon as the subscription
    /// is registered -- but the outer `#d` REQ is causally downstream of the
    /// INNER subscription's results (the relay must push the already-seeded
    /// kind:39001 rows, the client must ingest them and re-resolve the derived
    /// set, and only then does an outer REQ exist). Seeding the new group
    /// inside that gap makes it resolve alongside the original ones, so the
    /// FIRST outer REQ already carries every value and there is nothing live
    /// left to replace.
    ///
    /// That is not a wrong outcome -- one subscription carrying every value is
    /// exactly the contract -- but it makes the REPLACEMENT this scenario
    /// exists to observe unobservable, and it happens under load: measured at
    /// roughly one run in eight. Waiting for an outer `#d` subscription to
    /// actually be live first is what makes "one more" mean one more.
    pub async fn made_admin_of_one_more_group(&mut self) {
        self.ensure_started().await;
        let relay = self
            .watch_relay
            .clone()
            .expect("nmp-bdd: no relay has been named as the one I watch directly");
        self.wire_record_when(&relay, |record| {
            !record.live_subscription_ids_naming_tag('d').is_empty()
        })
        .await;
        self.group_counter += 1;
        let group = format!("group-{:04}", self.group_counter);
        self.watched_tag_values
            .entry('d')
            .or_default()
            .insert(group.clone());
        self.seed_group_admins(&group).await;
    }

    /// Every REQ/CLOSE `name`'s client has put on the socket, decoded.
    pub fn wire_record(&self, name: &str) -> WireRecord {
        self.relays
            .get(name)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown relay {name:?}"))
            .wire_record()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The batching is only legitimate if it is invisible to the contract:
    /// the same 1,200 values, and still one more watch than the relay ceiling
    /// the scenario asserts.
    #[test]
    fn catalog_batches_preserve_every_value_and_exceed_the_uncoalesced_relay_ceiling() {
        let batches = catalog_value_batches(1_200);
        let values = batches
            .iter()
            .flat_map(|batch| batch.iter().cloned())
            .collect::<BTreeSet<_>>();
        let expected = (1..=1_200)
            .map(|i| format!("value-{i:04}"))
            .collect::<BTreeSet<_>>();

        assert_eq!(batches.len(), UNCOALESCED_CATALOG_WATCHES);
        assert!(batches.len() > 20);
        assert!(batches.iter().all(|batch| !batch.is_empty()));
        assert_eq!(values, expected);
    }
}
