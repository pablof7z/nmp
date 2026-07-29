//! The identity plane: who a write publishes as, and what happens when the
//! key it names cannot currently sign.
//!
//! Its own module because the identity scenarios name their accounts by
//! PUBKEY rather than by a person's name -- `features/identity/` is written
//! for a reader who cares which key signed, so its vocabulary is hex, not
//! "Alice". Every such hex string is an ordinary fixture-person label
//! ([`super::NmpWorld::person`]), so a scenario that says
//! `"2bd8...6e90" is the active account` and later `the published event is
//! authored by "2bd8...6e90"` is talking about one keypair throughout,
//! exactly the way `Alice` does elsewhere.
//!
//! What this module owns is the identity itself: which accounts exist, which
//! one a write named, how a decided identity survives an account switch and
//! a genuine restart, and where a display form stops. Whether anything can
//! currently SIGN for a named key is a different question, and lives in
//! [`super::signers`].

use std::time::{Duration, Instant};

use nostr::PublicKey;

use nmp::mechanism::outbox::WriteStatus;
use nmp::mechanism::runtime::ReceiptReattachment;
use nmp_grammar::{Durability, EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};

use super::budgets::{EVENTUALLY, NEVER};
use super::observe::ReceiptState;
use super::NmpWorld;

impl NmpWorld {
    // ---- Given: who exists, and who can sign ---------------------------

    /// `Given the account with pubkey "<hex>" is registered with a working
    /// signer` / `Given my podcast identity "<hex>" is registered with a
    /// working signer`.
    ///
    /// "Registered" means two separate things that the scenarios keep
    /// separate: the keypair exists in this world, and a signing capability
    /// for it is attached to the engine. This step does both;
    /// [`Self::register_identity_without_signer`] does only the first, which
    /// is the entire subject of `awaiting-signer.feature`.
    pub fn register_identity_with_signer(&mut self, label: &str) {
        self.register_identity(label);
        self.identities_with_signers.push(label.to_string());
        // A Background that had to publish something already started the
        // engine (`features/writes/replaceable-edits` states a stored winner
        // before it names a second identity), and `spawn_engine`'s
        // registration pass has therefore already run. Registering an
        // identity is exactly what an app does at any moment of its life, so
        // the capability goes in now rather than waiting for a restart that
        // may never come.
        if self.started {
            self.add_signer_for(label);
        }
    }

    /// `Given no signer is registered for "<hex>"` -- the keypair exists (a
    /// scenario can name it, and the engine can freeze a body under it) but
    /// nothing in the world can sign for it.
    pub fn register_identity_without_signer(&mut self, label: &str) {
        self.register_identity(label);
    }

    /// What both registrations share: the keypair exists under this label,
    /// the label is remembered in order, it inherits the relay list the
    /// scenario stated as mine, and this scenario runs on a store that
    /// survives its engine (see `staging::open_store` for why that follows
    /// from naming an identity at all).
    fn register_identity(&mut self, label: &str) {
        self.person(label);
        self.identity_labels.push(label.to_string());
        self.durable_store = true;
        self.inherit_my_write_relays(label);
    }

    /// `Given my podcast identity "<hex>" ...` -- the same registration,
    /// remembered under the name later steps use to refer back to it ("the
    /// podcast identity's signer is slow to answer").
    pub fn register_podcast_identity(&mut self, label: &str) {
        self.register_identity_with_signer(label);
        self.podcast_identity = Some(label.to_string());
    }

    /// Every identity in an identity scenario writes to the relay list the
    /// scenario stated once, in `my relay list names "..." as my write
    /// relay`. The Background states it as MINE because that is how an app
    /// owner says it; the point of the feature is that several identities
    /// this one user holds all publish, so the outbox has to know where each
    /// of their events goes.
    fn inherit_my_write_relays(&mut self, label: &str) {
        for relay in self.write_relay_of(super::ME) {
            self.declare_write_relay(label, &relay);
        }
    }

    /// The other direction of the same rule: relays declared AFTER the
    /// identities were registered (which is the order the Backgrounds use)
    /// reach every identity too.
    pub(super) fn propagate_my_write_relay(&mut self, relay: &str) {
        for label in self.identity_labels.clone() {
            self.declare_write_relay(&label, relay);
        }
    }

    /// `Given I am logged in as the account with pubkey "<hex>"` -- every
    /// `features/writes/` Background.
    ///
    /// The same registration [`Self::register_identity`] performs, minus the
    /// durable store: a scenario that never reconstructs its engine has
    /// nothing to reopen, and redb transactions per scenario are the one cost
    /// the suite's wall clock actually notices. What it does share is the
    /// relay-list inheritance, and that is the load-bearing half -- the
    /// Background states the write relay as MINE and then publishes as this
    /// key, so without it the account has no route and `Auto` has nothing to
    /// resolve.
    pub fn log_in_as_identity(&mut self, label: &str) {
        self.person(label);
        if !self.identity_labels.iter().any(|known| known == label) {
            self.identity_labels.push(label.to_string());
        }
        self.inherit_my_write_relays(label);
        self.active_person = Some(label.to_string());
    }

    /// `Given "<hex>" is the active account`.
    pub async fn activate_identity(&mut self, label: &str) {
        self.person(label);
        self.active_person = Some(label.to_string());
        if self.started {
            let keys = self.person(label);
            self.handle().set_active_account(Some(keys.public_key()));
        }
    }

    /// `Given no account is active` -- stated before anything starts, so the
    /// engine comes up logged out exactly as a real launch does.
    pub fn no_account_is_active(&mut self) {
        assert!(
            !self.started,
            "nmp-bdd: state whether an account is active before anything runs"
        );
        self.active_person = None;
    }

    /// The label a scenario means by "the podcast identity".
    pub fn podcast_identity(&self) -> String {
        self.podcast_identity
            .clone()
            .expect("nmp-bdd: no podcast identity was registered in this scenario")
    }

    /// The label a scenario means by "that account" / "the first account" --
    /// whoever the scenario last named as active.
    pub fn current_identity(&self) -> String {
        self.active_person
            .clone()
            .expect("nmp-bdd: no account is active for this step to refer back to")
    }

    /// `Given the user pasted the npub form of "<hex>" into the identity
    /// picker` -- the display form really is a display form: it is the
    /// bech32 rendering of the key this world minted for that label.
    pub fn paste_npub_of(&mut self, label: &str) {
        use nostr::nips::nip19::ToBech32;
        let npub = self
            .person(label)
            .public_key()
            .to_bech32()
            .expect("nmp-bdd: a public key always renders as an npub");
        self.pasted_npub = Some(npub);
    }

    /// `When the app decodes it to a public key` -- through the exported
    /// bech32 door, at the app's own boundary, which is the only place a
    /// display form is ever decoded.
    pub fn decode_pasted_npub(&mut self) {
        let pasted = self
            .pasted_npub
            .clone()
            .expect("nmp-bdd: nothing was pasted for the app to decode");
        let entity = nmp_grammar::decode_nostr_entity(&pasted)
            .expect("nmp-bdd: the pasted npub must decode at the app's boundary");
        let nmp_grammar::NostrEntity::Pubkey { pubkey } = entity else {
            panic!("nmp-bdd: an npub decodes to a bare pubkey, not {entity:?}");
        };
        self.decoded_identity = Some(
            PublicKey::from_hex(&pubkey).expect("nmp-bdd: the decoded entity is a public key"),
        );
    }

    /// The key the app decoded, for the publish step that names "that
    /// identity".
    pub fn decoded_identity(&self) -> PublicKey {
        self.decoded_identity
            .expect("nmp-bdd: the app has not decoded an identity yet")
    }

    // ---- When: publishing under an identity ----------------------------

    /// `When I compose an event of kind <n> saying "<text>" and publish it
    /// naming no identity` / `... naming identity "<hex>"`.
    pub async fn publish_composed_event(&mut self, kind: u16, text: &str, identity: Identity) {
        self.ensure_started().await;
        let stream = self
            .handle()
            .publish_tracked(WriteIntent {
                payload: WritePayload::Event(
                    EventBuilder::new(nostr::Kind::Custom(kind)).content(text),
                ),
                durability: Durability::Durable,
                routing: WriteRouting::Auto,
                identity,
                correlation: None,
            })
            .expect("BDD receipt correlation namespace must be available");
        self.last_publish_was_auto = true;
        self.last_publish_label = self.label_of(identity);
        self.last_receipt_id = Some(stream.id);
        let state = ReceiptState::new(stream.statuses);
        self.receipts_by_text.insert(text.to_string(), state);
        self.last_receipt_text = Some(text.to_string());
    }

    /// Which registered identity a publish resolved to -- the key
    /// `Explicit` named, or whoever was active when `Active` was accepted.
    /// Recorded at publish time precisely because `Active` stops meaning
    /// "whoever is active" the instant the write is accepted.
    fn label_of(&self, identity: Identity) -> Option<String> {
        match identity {
            Identity::Active => self.active_person.clone(),
            Identity::Explicit(pubkey) => self
                .people
                .iter()
                .find(|(_, keys)| keys.public_key() == pubkey)
                .map(|(label, _)| label.clone()),
        }
    }

    /// `When I compose ... and publish it naming as identity the npub form
    /// of "<hex>"` -- the refusal is STRUCTURAL, not a message the engine
    /// sends back: `Identity::Explicit` carries a `PublicKey`, and a bech32
    /// string is not one. The app's own decode door is where a display form
    /// becomes a key, and it refuses this input before any write exists.
    pub fn refuse_bech32_identity(&mut self, label: &str) {
        use nostr::nips::nip19::ToBech32;
        let npub = self
            .person(label)
            .public_key()
            .to_bech32()
            .expect("nmp-bdd: a public key always renders as an npub");
        let refusal = PublicKey::from_hex(&npub)
            .err()
            .map(|err| err.to_string())
            .expect("nmp-bdd: a bech32 npub must not parse as the pubkey this field takes");
        self.identity_refusal = Some(refusal);
    }

    /// `When I switch the active account to "<hex>"`.
    pub async fn switch_active_identity(&mut self, label: &str) {
        self.ensure_started().await;
        let keys = self.person(label);
        self.handle().set_active_account(Some(keys.public_key()));
        self.active_person = Some(label.to_string());
    }

    /// `When I cancel that write`.
    pub fn cancel_last_write(&mut self) {
        let id = self
            .last_receipt_id
            .expect("nmp-bdd: no write is in flight to cancel");
        self.handle()
            .cancel_write(id)
            .expect("nmp-bdd: an accepted write must be cancellable");
    }

    // ---- Then: what the identity plane produced ------------------------

    /// The statuses the write for `text` has reported, waiting (bounded)
    /// until `pred` holds. After a restart this reads the REATTACHED stream,
    /// which is the only stream that exists on the far side of one.
    fn identity_receipt_eventually(
        &mut self,
        text: Option<&str>,
        window: Duration,
        pred: impl Fn(&[WriteStatus]) -> bool,
    ) -> bool {
        let receipt = self.identity_receipt_mut(text);
        receipt.eventually_within(window, pred)
    }

    fn identity_receipt_mut(&mut self, text: Option<&str>) -> &mut ReceiptState {
        if let Some(text) = text {
            return self
                .receipts_by_text
                .get_mut(text)
                .unwrap_or_else(|| panic!("nmp-bdd: nothing was published saying {text:?}"));
        }
        // A restart replaces the live stream with the reattached one: on the
        // far side of a process boundary that is the only stream that exists.
        if let Some(receipt) = self.restarted_receipt.as_mut() {
            return receipt;
        }
        if let Some(key) = self.last_receipt_text.clone() {
            return self
                .receipts_by_text
                .get_mut(&key)
                .expect("the last publish's receipt is always retained");
        }
        // A write the identity plane did not itself issue. `features/writes/`
        // asks the same questions of another plane's publishes -- "it never
        // reports accepted", "no journal row was written", "the published
        // event is authored by X" -- and since #995 every publish this world
        // makes is one entry in one ordered list, so "the publish" is its
        // last entry however it got there.
        self.receipts
            .last_mut()
            .expect("nmp-bdd: no publish is in flight")
    }

    /// Every status the write under discussion has reported so far, for
    /// assertion messages.
    pub fn identity_receipt_statuses(&mut self, text: Option<&str>) -> Vec<WriteStatus> {
        let receipt = self.identity_receipt_mut(text);
        receipt.eventually_within(Duration::from_millis(0), |_| true);
        receipt.seen.clone()
    }

    /// Has the write under discussion reported its first receipt fact?
    ///
    /// `publish` may return after registering this receipt but before the
    /// engine thread dispatches its first status. A step whose claim requires
    /// a real receipt fact must therefore wait boundedly here; the
    /// zero-duration [`Self::identity_receipt_statuses`] snapshot is only for
    /// assertion messages after a positive observation has established what
    /// exists.
    pub fn identity_receipt_reported_anything(&mut self, text: Option<&str>) -> bool {
        self.identity_receipt_eventually(text, EVENTUALLY, |seen| !seen.is_empty())
    }

    /// `Then the write reports accepted`.
    pub fn write_reported_accepted(&mut self, text: Option<&str>) -> bool {
        self.identity_receipt_eventually(text, EVENTUALLY, |seen| {
            seen.iter().any(|s| matches!(s, WriteStatus::Accepted))
        })
    }

    /// `Then it never reports accepted` -- costs its full window, since
    /// there is no early exit from "this did not happen".
    pub fn write_never_reported_accepted(&mut self, text: Option<&str>) -> bool {
        !self.identity_receipt_eventually(text, NEVER, |seen| {
            seen.iter().any(|s| matches!(s, WriteStatus::Accepted))
        })
    }

    /// `Then the write is refused ...` -- `Failed` FIRST and alone, so no
    /// acceptance ever preceded it.
    pub fn write_refused_before_acceptance(&mut self, text: Option<&str>) -> bool {
        self.identity_receipt_eventually(text, EVENTUALLY, |seen| {
            matches!(seen.first(), Some(WriteStatus::Failed(_)))
        })
    }

    /// `Then the write is reported cancelled`.
    pub fn write_reported_cancelled(&mut self, text: Option<&str>) -> bool {
        self.identity_receipt_eventually(text, EVENTUALLY, |seen| {
            seen.iter().any(|s| matches!(s, WriteStatus::Cancelled))
        })
    }

    /// `Then the receipt reports it awaiting a signer for "<hex>"` -- the
    /// park, named. The pubkey on the status is the whole point: an app
    /// renders "waiting for your podcast signer" from it rather than
    /// inferring a stall from the absence of anything else.
    pub fn write_awaiting_signer_for(&mut self, label: &str, text: Option<&str>) -> bool {
        let pubkey = self.person(label).public_key();
        self.identity_receipt_eventually(text, EVENTUALLY, move |seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::AwaitingCapability { pubkey: p } if *p == pubkey))
        })
    }

    /// `Then the write is never refused`.
    pub fn write_never_refused(&mut self, text: Option<&str>) -> bool {
        !self.identity_receipt_eventually(text, NEVER, |seen| {
            seen.iter().any(|s| matches!(s, WriteStatus::Failed(_)))
        })
    }

    /// `Then nothing is signed` -- after a cancel, no signature is ever
    /// produced, however late a capability turns up.
    pub fn nothing_was_signed(&mut self, text: Option<&str>) -> bool {
        !self.identity_receipt_eventually(text, NEVER, |seen| {
            seen.iter().any(|s| matches!(s, WriteStatus::Signed(_)))
        })
    }

    /// `Then the write is signed by that signer`.
    pub fn write_was_signed(&mut self, text: Option<&str>) -> bool {
        self.identity_receipt_eventually(text, EVENTUALLY, |seen| {
            seen.iter().any(|s| matches!(s, WriteStatus::Signed(_)))
        })
    }

    /// `Then the published event is authored by "<hex>"` / `Then "<text>" is
    /// authored by "<hex>"` -- read off the RELAY, which is the only place
    /// an app can point at "the published event" and mean the thing the
    /// world received.
    pub fn published_event_authored_by(&mut self, label: &str, text: Option<&str>) -> bool {
        let author = self.person(label).public_key();
        self.await_admitted(EVENTUALLY, move |event| {
            event.pubkey == author && text.is_none_or(|t| event.content == t)
        })
    }

    /// `Then "<relay>" received it`.
    pub fn relay_received_the_write(&mut self, relay: &str) -> bool {
        let text = self.last_receipt_text.clone();
        self.await_admitted_at(relay, EVENTUALLY, move |event| {
            text.as_deref().is_none_or(|t| event.content == t)
        })
    }

    /// `Then "<relay>" received nothing` / `... nothing yet`. Costs its full
    /// window: the claim is that nothing arrives, not that nothing has yet.
    pub fn relay_received_nothing(&mut self, relay: &str) -> bool {
        !self.await_admitted_at(relay, NEVER, |_| true)
    }

    /// `Then "<hex>" is still the active account`.
    pub fn active_identity_is(&mut self, label: &str) -> bool {
        self.active_person.as_deref() == Some(label)
    }

    /// `Then the receipt can be reattached by its stable id` -- the id the
    /// publish door returned still names retained facts, which is what makes
    /// a parked write something the app owns rather than something NMP is
    /// holding on its behalf.
    pub fn receipt_reattaches_by_id(&mut self) -> bool {
        let Some(id) = self.last_receipt_id else {
            return false;
        };
        matches!(
            self.handle().reattach_receipt(id),
            ReceiptReattachment::Attached { .. }
        )
    }

    /// `Then no journal row was written and no write id was allocated` --
    /// acceptance IS the journal write, so a receipt that never accepted
    /// never wrote one, and the pre-acceptance correlation id it carried is
    /// not a durable write id.
    pub fn nothing_was_journaled(&mut self, text: Option<&str>) -> bool {
        self.identity_receipt_statuses(text)
            .iter()
            .all(|s| !matches!(s, WriteStatus::Accepted))
    }

    /// The label of whoever is active right now, for a restart step that
    /// says nothing about who should be active on the far side.
    pub fn active_identity_label(&self) -> Option<String> {
        self.active_person.clone()
    }

    /// The label a scenario means by "the first account" -- the first one its
    /// Background registered.
    pub fn first_identity(&self) -> String {
        self.identity_labels
            .first()
            .cloned()
            .expect("nmp-bdd: no identity was registered in this scenario")
    }

    /// `Then the pending write still awaits "<hex>"` -- the accepted write is
    /// STILL about that one key, whichever way the scenario got here.
    ///
    /// Two shapes count, and they are the two shapes a pin can take. When no
    /// signer answers for the key, the park names it outright. When one is
    /// registered but slow, there is no status to read, so the fact is which
    /// signer was approached: that key's, and nobody else's, with nothing
    /// signed yet.
    pub fn write_still_pinned_to(&mut self, label: &str) -> bool {
        if self.write_awaiting_signer_for(label, None) {
            return true;
        }
        let asked = self.signer_ask_count_for(label);
        if asked == 0 {
            return false;
        }
        let others: usize = self
            .identity_labels
            .clone()
            .into_iter()
            .filter(|other| other != label)
            .map(|other| self.signer_ask_count_for(&other))
            .sum();
        others == 0
    }

    /// Let anything the last write was going to do actually happen. Its own
    /// method because [`super::signers`] needs it for the same reason every
    /// negative assertion does: an ask that has not been issued yet is not
    /// proof that it never will be.
    pub(super) fn settle_last_write(&mut self) {
        self.identity_receipt_eventually(None, NEVER, |_| false);
    }

    /// The refusal the app's own decode door produced, if a step asked it to
    /// take a display form where a key belongs.
    pub fn identity_refusal(&self) -> Option<&String> {
        self.identity_refusal.as_ref()
    }

    // ---- reading the relays --------------------------------------------

    fn await_admitted(&mut self, window: Duration, pred: impl Fn(&nostr::Event) -> bool) -> bool {
        let deadline = Instant::now() + window;
        loop {
            let hit = self
                .relays
                .values()
                .flat_map(|relay| relay.admitted_events())
                .any(|event| pred(&event));
            if hit {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn await_admitted_at(
        &mut self,
        relay: &str,
        window: Duration,
        pred: impl Fn(&nostr::Event) -> bool,
    ) -> bool {
        let deadline = Instant::now() + window;
        loop {
            let hit = self
                .relays
                .get(relay)
                .map(|r| r.admitted_events().iter().any(&pred))
                .unwrap_or(false);
            if hit {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
