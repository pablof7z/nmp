//! The NIP-29 `Group` door: staging a group identity, reading through it,
//! publishing through it, and the facts a group `Then` step reads.
//!
//! Its own module for the same reason [`super::watches`] is one. A group
//! scenario asks two questions no feed step asks: WHICH HOST an event reached
//! (and, just as often, which hosts it did not), and WHAT THE DELIVERED EVENT
//! LITERALLY WAS -- its id, its tag order, its signature. Answering either
//! needs the world-side relay witness rather than the app-visible feed, plus
//! bookkeeping (staged drafts, staged signed events, id labels) that exists
//! only for these scenarios.
//!
//! Two things here are deliberate and worth stating out loud:
//!
//! - **Writes go through the real product door.** `group.publish(&engine,
//!   author, ..)` -- an INHERENT method on `nip29::Group`, not a hand-built
//!   `WriteIntent`. That is the whole point of the feature under test, so the
//!   harness must not reimplement it.
//!   The mint half (#1242) has no harness of its own here: governed scenarios
//!   never reach this runner, and its proof lives with the narrowest contract
//!   owner -- `nip29::group`'s own tests, and their FFI/Swift/Kotlin mirrors.
//! - **Reads go through the same subscription call every other read in this
//!   suite uses** (`Handle::subscribe`, which `Engine::observe` is a thin
//!   wrapper over), fed by `group.read(filter)` -- one ordinary `LiveQuery`
//!   already assembled from one branch per host. There is no group-shaped
//!   read door to call, which IS the contract; the group only mints the
//!   query.
//!
//! Two siblings carry the rest. [`super::group_fixtures`] owns the event a
//! scenario hands the door -- unsigned draft, already-signed event, and the
//! id labels that stand in for ids a `.feature` cannot spell.
//! [`super::group_surface`] answers the questions no run can answer at all:
//! what the door DECLARES, and what the ownership gate says about it.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use nostr::{Event, EventId, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

use nmp::nip29::{self, Group, GroupContextError, GroupPublishError};
use nmp::Engine;
use nmp::ReceiptStream;
use nmp_grammar::Filter;

use super::budgets::EVENTUALLY;
use super::observe::{FeedState, ReceiptState};
use super::NmpWorld;

/// What the STEP ITSELF handed the group door -- the only honest witness for
/// "I named no relay and no tag on that call".
///
/// It is recorded by the `When` step from its own arguments rather than
/// inferred later: the claim is about what the APP said, and after the call
/// the intent looks identical whoever supplied the host.
#[derive(Debug, Default, Clone, Copy)]
pub struct GroupCall {
    /// The step passed a relay. Always false: no group operation accepts one.
    pub named_relay: bool,
    /// The step passed a tag name or tag value.
    pub named_tag: bool,
    /// The step passed a kind number.
    pub named_kind: bool,
}

/// Every integer in a phrase like `kind 9` / `kinds 9 and 9000` /
/// `kinds 39002 and 39001`. One parser so `Given a filter selecting <kinds>`
/// and `Then the request selects exactly <kinds>` can never disagree about
/// what the scenario's own words mean.
pub fn parse_kind_list(raw: &str) -> BTreeSet<u16> {
    let mut kinds = BTreeSet::new();
    let mut digits = String::new();
    for c in raw.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if !digits.is_empty() {
            kinds.insert(
                digits
                    .parse()
                    .unwrap_or_else(|_| panic!("nmp-bdd: {digits:?} is not a kind number")),
            );
            digits.clear();
        }
    }
    assert!(!kinds.is_empty(), "nmp-bdd: no kind number in {raw:?}");
    kinds
}

impl NmpWorld {
    // ---- Given-time staging (no I/O yet) -------------------------------

    /// `Given the group "photographers" hosted by relay "wss://..."`.
    ///
    /// Stages the identity only. The `nip29::Group` value cannot exist yet --
    /// a scripted relay has no URL until it is bound -- and that is fine,
    /// because construction is the one thing this door promises costs nothing.
    pub fn stage_group(&mut self, group_id: &str, relay: &str) {
        self.relay_config_mut(relay);
        self.group_hosts
            .insert(group_id.to_string(), relay.to_string());
        self.default_group
            .get_or_insert_with(|| group_id.to_string());
    }

    /// `Given I am logged in as "<64 hex>"`. The scenario names the account by
    /// its own key material, so the fixture keypair is DERIVED from it rather
    /// than minted -- otherwise a later step naming the same hex (an event
    /// "signed earlier by" it, a relay list belonging to it) would be talking
    /// about a different person.
    pub fn log_in_as_key(&mut self, secret_hex: &str) {
        let keys = keys_from_hex(secret_hex);
        self.people.insert(super::ME.to_string(), keys.clone());
        self.people.insert(secret_hex.to_string(), keys);
        self.active_person = Some(super::ME.to_string());
    }

    /// `Given my relay list has never been fetched` -- the relay stays in the
    /// world (so "it received no event" is a real claim about a live relay),
    /// but nothing in the directory says it is mine. An `Auto` write here
    /// would have nowhere to go; a group write does not care.
    pub fn forget_my_relay_list(&mut self) {
        self.forget_relay_list(super::ME);
    }

    /// `Given relay "R" cannot connect`. Bound (so it has a URL the group can
    /// be constructed with), then severed at start, so a connection attempt is
    /// refused rather than silently succeeding against nothing.
    pub fn set_unreachable(&mut self, relay: &str) {
        self.relay_config_mut(relay);
        self.unreachable_relays.insert(relay.to_string());
    }

    /// `Given relay "R" rejects kind 9001 with "restricted: ..."`.
    pub fn set_reject_kind(&mut self, relay: &str, kind: u16, message: &str) {
        self.relay_config_mut(relay).reject_kind = Some((kind, message.to_string()));
    }

    /// `Given signing fails for this account`.
    pub fn fail_signing(&mut self) {
        self.signer_fails.store(true, Ordering::SeqCst);
    }

    /// `Given a filter selecting kind 9` and its siblings. The APP supplies
    /// this; the group contributes no kind of its own.
    pub fn stage_filter(&mut self, kinds: BTreeSet<u16>) {
        self.staged_filters.push(Filter {
            kinds: Some(kinds),
            ..Filter::default()
        });
    }

    /// The public key of the person a scenario names by 64 hex characters.
    ///
    /// A `.feature` cannot spell a real public key any more than it can spell
    /// a real event id, so the hex is the scenario's NAME for a person and
    /// every step naming it resolves through the one fixture keypair map.
    /// `p tag naming "b0b0..."` is therefore checked against that person's
    /// actual key, which is what the app would have passed.
    pub fn member_pubkey(&mut self, name: &str) -> nostr::PublicKey {
        self.person(name).public_key()
    }

    /// `Given I have never observed anything from this group`.
    pub fn assert_no_group_observation(&self) {
        assert!(
            self.feed.is_none() && self.watches.is_empty(),
            "nmp-bdd: this scenario is about publishing with no subscription, \
             but one is already open"
        );
    }

    /// `Given I am not an admin of "<group>"`. There is nothing to configure:
    /// NMP holds no membership state, which is the claim the paired `Then`
    /// makes. Asserting the group exists keeps the step from being a no-op
    /// against a typo.
    pub fn assert_no_permission_claim(&self, group_id: &str) {
        assert!(
            self.group_hosts.contains_key(group_id),
            "nmp-bdd: no group {group_id:?} was staged"
        );
    }

    // ---- the group value ------------------------------------------------

    /// The ONE `nip29::Group` value for `group_id`, built on first use.
    ///
    /// Every read and every write in a scenario resolves through here, so the
    /// build counter below is a real witness: "the same group instance minted
    /// all four" and "no group had to be reconstructed" are both statements
    /// about this map, not about a step's good intentions.
    pub fn group_value(&mut self, group_id: Option<&str>) -> Group {
        let id = self.group_name(group_id);
        if let Some(group) = self.group_values.get(&id) {
            return group.clone();
        }
        let relay = self
            .group_hosts
            .get(&id)
            .unwrap_or_else(|| panic!("nmp-bdd: no group {id:?} was staged"))
            .clone();
        let host = self.relay_url(&relay);
        let scope =
            nip29::on([host]).expect("nmp-bdd: a single staged host is always a nonempty scope");
        let group = scope.group(id.clone());
        self.group_values.insert(id.clone(), group.clone());
        *self.group_builds.entry(id).or_default() += 1;
        group
    }

    /// The currently logged-in account's public key -- the `author` every
    /// group write now freezes explicitly (#878, #1033). Every group scenario
    /// backgrounds `Given I am logged in as "<hex>"`, so this always resolves
    /// through the one canonical [`super::ME`] identity.
    pub fn me_pubkey(&mut self) -> PublicKey {
        self.person(super::ME).public_key()
    }

    /// How many times a `nip29::Group` was CONSTRUCTED for `group_id`.
    pub fn group_build_count(&self, group_id: Option<&str>) -> usize {
        let id = self.group_name_ref(group_id);
        self.group_builds.get(&id).copied().unwrap_or(0)
    }

    fn group_name(&mut self, group_id: Option<&str>) -> String {
        self.group_name_ref(group_id)
    }

    fn group_name_ref(&self, group_id: Option<&str>) -> String {
        match group_id {
            Some(id) => id.to_string(),
            None => self
                .default_group
                .clone()
                .expect("nmp-bdd: no group was staged"),
        }
    }

    /// The default group's id -- the value of the `h` row it mints.
    pub fn group_host_group_id(&self) -> String {
        self.group_name_ref(None)
    }

    /// The relay name hosting `group_id` -- what a `Then` naming "its host"
    /// resolves through.
    pub fn group_host_name(&self, group_id: Option<&str>) -> String {
        let id = self.group_name_ref(group_id);
        self.group_hosts
            .get(&id)
            .unwrap_or_else(|| panic!("nmp-bdd: no group {id:?} was staged"))
            .clone()
    }

    // ---- reads: the group mints a demand, `observe` is the door ---------

    /// `When I observe a live query built from the group's demand for that
    /// filter` (and its siblings).
    ///
    /// `group.read(filter)` -- already the one ordinary `LiveQuery`, one
    /// branch per host in the group's scope -- handed to the SAME
    /// subscription call every other read in this suite uses. No
    /// group-shaped read verb is called here because none exists, which is
    /// the contract.
    pub async fn observe_group_demand(&mut self, group_id: Option<&str>, filter: Filter) {
        self.ensure_started().await;
        let group = self.group_value(group_id);
        let query = group
            .read(filter)
            .expect("nmp-bdd: a single-host group read declares exactly one branch");
        let (handle, rx) = self
            .handle()
            .subscribe(query)
            .expect("nmp-bdd: group subscription construction");
        // One group serves SEVERAL simultaneous observations, so an earlier
        // one is retained rather than dropped -- dropping it would withdraw
        // the subscription and quietly make "four at once" mean "one".
        if let Some(previous) = self.feed.take() {
            let key = format!("group-{}", self.watches.len() + 1);
            self.watches.insert(key, previous);
        }
        self.feed = Some(FeedState::new(handle, rx));
    }

    /// How many group observations are open RIGHT NOW -- the app-level count
    /// behind "four independent subscriptions exist at once". Deliberately
    /// app-level: NMP collapses demands sharing a relay and a tag scope into
    /// one wire subscription, which is a shipped contract of its own.
    pub fn open_group_observations(&self) -> usize {
        usize::from(self.feed.is_some()) + self.watches.len()
    }

    /// Every staged filter, in the order the scenario named them.
    pub fn staged_filters(&self) -> Vec<Filter> {
        self.staged_filters.clone()
    }

    /// The last staged filter -- what "that filter" means.
    pub fn last_staged_filter(&self) -> Filter {
        self.staged_filters
            .last()
            .cloned()
            .expect("nmp-bdd: no filter was staged")
    }

    // ---- writes: the one publish door, through the group ----------------

    /// Every group `When` goes through here: settle the wire, snapshot the
    /// relay counters, run the operation against the REAL `Engine`, and keep
    /// whichever of the two outcomes happened -- a receipt stream, or a typed
    /// refusal that never reached the door.
    pub async fn group_operation<F>(&mut self, group_id: Option<&str>, call: GroupCall, op: F)
    where
        F: FnOnce(&Group, &Engine) -> Result<ReceiptStream, GroupPublishError>,
    {
        self.ensure_started().await;
        self.wire_settled().await;
        self.snapshot_relay_contacts();
        let group = self.group_value(group_id);
        self.group_call = call;
        self.group_refusal = None;
        // A group write is never `Auto`: the route is minted by the group.
        self.last_publish_was_auto = false;
        let outcome = {
            let engine = self
                .engine
                .as_ref()
                .expect("nmp-bdd: the engine must be started before publishing");
            op(&group, engine)
        };
        match outcome {
            Ok(receipts) => {
                self.last_receipt_id = Some(receipts.id);
                self.receipts.push(ReceiptState::new(receipts.statuses));
            }
            Err(GroupPublishError::Context(error)) => {
                // A refusal at the door minted no obligation, so it adds
                // nothing to the world's list of publishes -- which is what
                // makes `receipt_count() == 0` the honest assertion for it.
                self.group_refusal = Some(error);
            }
            Err(GroupPublishError::Engine(error)) => {
                panic!("nmp-bdd: the publish door refused a group write: {error:?}")
            }
            Err(GroupPublishError::Users(error)) => {
                panic!("nmp-bdd: a valid fixture user batch was refused: {error:?}")
            }
        }
    }

    /// The typed refusal the last group publication produced, if it never
    /// reached the publish door.
    pub fn group_refusal(&self) -> Option<GroupContextError> {
        self.group_refusal.clone()
    }

    /// What the step itself named on the last group call.
    pub fn group_call(&self) -> GroupCall {
        self.group_call
    }

    // ---- what actually reached a relay ----------------------------------

    /// The id the last publication's receipt reported signing/freezing.
    ///
    /// Read off the receipt rather than recomputed here: the point of most of
    /// these scenarios is what the ENGINE produced, and a harness that
    /// recomputed the id would be asserting against its own arithmetic.
    pub fn published_event_id(&mut self) -> Option<EventId> {
        self.receipt_eventually(|seen| {
            seen.iter().any(|s| {
                matches!(
                    s,
                    nmp_engine::publish_queue::WriteFact::Signing(
                        nmp_engine::publish_queue::SigningState::Signed { .. }
                    )
                )
            })
        });
        self.receipt_statuses().into_iter().find_map(|s| match s {
            nmp_engine::publish_queue::WriteFact::Signing(
                nmp_engine::publish_queue::SigningState::Signed { event_id },
            ) => Some(event_id),
            _ => None,
        })
    }

    /// The event `relay` was handed, verbatim -- bounded-waited for, because
    /// a receipt beat and a socket write are not the same instant.
    pub async fn delivered_event_at(&mut self, relay: &str) -> Option<Event> {
        let id = self.published_event_id()?;
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            let found = self
                .relays
                .get(relay)
                .map(|r| r.admitted_events())
                .unwrap_or_default()
                .into_iter()
                .find(|event| event.id == id);
            if found.is_some() || Instant::now() >= deadline {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The same event, wherever it landed -- for the claims that are about
    /// the event's own bytes rather than about which host received it.
    pub async fn published_event(&mut self) -> Option<Event> {
        let host = self.group_host_name(None);
        self.delivered_event_at(&host).await
    }

    /// Every relay that was handed the last published event.
    pub fn relays_holding_published_event(&self, id: EventId) -> Vec<String> {
        self.relays
            .iter()
            .filter(|(_, relay)| relay.admitted_events().iter().any(|event| event.id == id))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Every event `relay` was handed, verbatim.
    pub fn events_received_by(&self, relay: &str) -> Vec<Event> {
        self.relays
            .get(relay)
            .map(|r| r.admitted_events())
            .unwrap_or_default()
    }

    /// Relays that are bound but severed -- consulted by `ensure_started`.
    pub(super) fn is_unreachable(&self, relay: &str) -> bool {
        self.unreachable_relays.contains(relay)
    }

    /// `Given relay "R" holds a kind K event with h "X" saying "T"` --
    /// pre-existing group content, seeded exactly as a real relay would
    /// already hold it.
    pub async fn seed_group_event(&mut self, relay: &str, kind: u16, group_id: &str, text: &str) {
        self.ensure_started().await;
        let keys = self.person("group-member");
        let created_at = self.next_created_at();
        let event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(created_at),
            Kind::from(kind),
            vec![Tag::parse(["h", group_id]).expect("'h' is well-formed")],
            text.to_string(),
        )
        .sign_with_keys(&keys)
        .expect("nmp-bdd: a group fixture must sign cleanly");
        self.relays[relay].seed_signed_event(&event).await;
    }

    /// Every REQ any relay has been sent that names `#h` -- the wire witness
    /// behind every "the request is pinned/scoped/selects" assertion.
    pub fn group_requests(&self, relay: &str) -> Vec<nmp_test_support::relays::WireReq> {
        self.wire_record(relay)
            .reqs
            .into_iter()
            .filter(|req| req.names_tag('h'))
            .collect()
    }

    /// Distinct still-live subscriptions on `relay` that scope by `#h`.
    pub fn live_group_subscriptions(&self, relay: &str) -> Vec<String> {
        self.wire_record(relay)
            .live_subscription_ids_naming_tag('h')
    }
}

/// A fixed epoch for every fixture timestamp a scenario says the app chose,
/// so "created_at survived unchanged" compares against a known number.
pub(super) const APP_CHOSEN_CREATED_AT: u64 = 1_700_000_042;

/// A scenario names an account by 64 hex characters of key material, so the
/// keypair is DERIVED from exactly those bytes -- the same word in two
/// different steps must mean the same person.
pub(super) fn keys_from_hex(secret_hex: &str) -> Keys {
    Keys::parse(secret_hex)
        .unwrap_or_else(|_| panic!("nmp-bdd: {secret_hex:?} is not a secp256k1 secret key"))
}
