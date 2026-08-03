//! The replaceable plane: which version of a whole-value event is the local
//! winner, and what a compare-and-swap replacement did about it.
//!
//! Its own module because the subject is a THIRD party to every other write
//! plane in this world -- the STORE's row. `writes` is about the payload an
//! app hands over and `identity` about who it publishes as; neither can
//! express "the winner moved while my write was in flight", which is the
//! whole of `features/writes/replaceable-edits.feature`.
//!
//! Two harness decisions are worth stating outright, because both are places
//! where a reader could reasonably suspect the fixture of doing the engine's
//! work for it:
//!
//! - **Every winner is established through the ONE publish door.** A scenario
//!   that says "my contact list X is the stored winner" gets a real accepted
//!   kind:3 with the stated `created_at`, and the word X is bound to the id
//!   that write actually froze. Nothing is written into the store behind the
//!   engine's back, so the row a CAS later compares against is a row the
//!   engine itself put there.
//! - **Those fixture writes are routed to a relay no scenario names**
//!   ([`OTHER_DEVICE`]). A publish must name somewhere -- an empty explicit
//!   route is refused at the acceptance door, which is correct and is another
//!   feature's subject -- and sending the Background's winner to
//!   `wss://hub.example` would make "hub received nothing" false for reasons
//!   that have nothing to do with the write under test. The name is the
//!   scenario's own: these writes are the ones "another device" made.
//!
//! What the module deliberately does NOT do is decide anything. The winner is
//! read back through an ordinary subscription, exactly as an app would, and
//! every conflict, stamp and refusal comes off the receipt.

use std::time::{Duration, Instant};

use nostr::{EventId, Timestamp};

use nmp::mechanism::publish_queue::WriteStatus;
use nmp_grammar::{Durability, EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};

use super::budgets::EVENTUALLY;
use super::observe::{FeedState, ReceiptState};
use super::queries::contact_list_query;
use super::NmpWorld;

/// Where a fixture winner is published. See the module doc for why a write
/// that only exists to put a row in the store still has to name a relay, and
/// why it must not be one the scenario asserts about.
const OTHER_DEVICE: &str = "wss://other-device.invalid";

impl NmpWorld {
    // ---- Given: what the store already holds -----------------------------

    /// `Given my contact list "<label>" created at "<ts>" is the stored
    /// winner` / `Given another device replaced it with "<label>" created at
    /// "<ts>"` / `Given that identity's contact list ... is its stored
    /// winner`.
    ///
    /// One method for all three because they are one fact stated from three
    /// angles: at this coordinate, this version is what the store holds. The
    /// only thing that varies is whose coordinate it is.
    pub async fn stage_stored_winner(&mut self, owner: &str, label: &str, at: Timestamp) {
        if !self.relay_configs.contains_key(OTHER_DEVICE) {
            self.register_bystander_relay(OTHER_DEVICE);
        }
        self.ensure_started().await;
        let pubkey = self.person(owner).public_key();
        let routing = WriteRouting::Explicit([self.relay_url(OTHER_DEVICE)].into_iter().collect());
        let statuses = self
            .handle()
            .publish(WriteIntent {
                payload: WritePayload::Event(EventBuilder {
                    kind: nostr::Kind::ContactList,
                    tags: Vec::new(),
                    content: String::new(),
                    created_at: Some(at),
                }),
                durability: Durability::Durable,
                routing,
                identity: Identity::Explicit(pubkey),
                correlation: None,
            })
            .expect("BDD receipt correlation namespace must be available");
        // A fixture winner is still a publish, so it takes its place in the
        // world's one ordered receipt list (#995) rather than opening a
        // private stream nothing else can see.
        self.receipts.push(ReceiptState::new(statuses));
        let frozen = self.frozen_id_of_last_publish().unwrap_or_else(|| {
            panic!(
                "nmp-bdd: staging a stored winner must be accepted and frozen; saw {:?}",
                self.receipt_statuses()
            )
        });
        self.bind_id(label, frozen);
        self.stated_created_at.insert(label.to_string(), at);
        // The engine has to have finished writing the row before the next
        // step CAS-es against it, and the row is what the app can read.
        assert_eq!(
            self.stored_winner_of(owner),
            Some(frozen),
            "nmp-bdd: the staged version must be the store's winner before anything CAS-es \
             against it"
        );
    }

    /// `Given "<hex>"'s contact list "<label>" is stored locally` -- somebody
    /// ELSE's version, which cannot come through my publish door because I
    /// cannot sign for them. It arrives the only way a foreign event ever
    /// does: published to a relay, and observed from it.
    pub async fn observe_foreign_contact_list(&mut self, owner: &str, label: &str) {
        if !self.relay_configs.contains_key(OTHER_DEVICE) {
            self.register_bystander_relay(OTHER_DEVICE);
        }
        let keys = self.person(owner);
        let created_at = self.next_created_at();
        self.ensure_started().await;
        self.relays[OTHER_DEVICE]
            .seed_contact_list(&keys, &[], created_at)
            .await;
        let id = self.stored_winner_of(owner).unwrap_or_else(|| {
            panic!("nmp-bdd: {owner}'s contact list never reached this world's store")
        });
        self.bind_id(label, id);
        self.stated_created_at
            .insert(label.to_string(), Timestamp::from_secs(created_at));
        self.foreign_contact_lists
            .insert(owner.to_string(), label.to_string());
    }

    // ---- When: replacing it ---------------------------------------------

    /// `When I publish a replacement contact list naming "<label>" as the
    /// version it replaces`, in all of its forms.
    ///
    /// `base` is the word the scenario named, `created_at` is stated only by
    /// the scenario that deliberately loads the foot-gun, and `identity`
    /// decides WHICH coordinate is CAS-ed -- the same resolution that decides
    /// the author, which is the point of the last scenario in the file.
    pub async fn publish_replacement(
        &mut self,
        identity: Identity,
        base: &str,
        created_at: Option<Timestamp>,
    ) {
        self.stage_replacement(identity, base, created_at);
        self.publish_staged_replacement().await;
    }

    /// `When I read the stored winner and compose a replacement naming
    /// "<label>" as the version it replaces` -- composed and HELD, so the
    /// scenario can move the winner underneath it before it is published.
    /// That gap is what makes "checked at acceptance, not at compose time" a
    /// claim with two distinguishable answers.
    pub fn stage_replacement(
        &mut self,
        identity: Identity,
        base: &str,
        created_at: Option<Timestamp>,
    ) {
        let expected_base = Some(self.id_of(base));
        self.pending_replacement = Some((
            identity,
            expected_base,
            EventBuilder {
                kind: nostr::Kind::ContactList,
                tags: Vec::new(),
                content: String::new(),
                created_at,
            },
        ));
    }

    /// `And I publish that replacement`.
    pub async fn publish_staged_replacement(&mut self) {
        let (identity, expected_base, builder) = self
            .pending_replacement
            .take()
            .expect("nmp-bdd: no replacement was composed for this step to publish");
        self.ensure_started().await;
        let statuses = self
            .handle()
            .publish(WriteIntent {
                payload: WritePayload::ReplaceableEdit {
                    builder,
                    expected_base,
                },
                durability: Durability::Durable,
                routing: WriteRouting::Auto,
                identity,
                correlation: None,
            })
            .expect("BDD receipt correlation namespace must be available");
        self.last_publish_was_auto = true;
        self.last_receipt_text = None;
        self.receipts.push(ReceiptState::new(statuses));
    }

    // ---- Then: the row, the conflict, the stamp --------------------------

    /// `Then the write is accepted`.
    pub fn replacement_accepted(&mut self) -> bool {
        self.receipt_eventually(|seen| seen.iter().any(|s| matches!(s, WriteStatus::Accepted)))
    }

    /// `Then the write is refused with a replaceable conflict` -- and the
    /// conflict is FIRST, so nothing was ever accepted on top of a stale
    /// base.
    pub fn replacement_conflicted(&mut self) -> bool {
        self.receipt_eventually(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::ReplaceableConflict { .. }))
        }) && !self
            .receipt_statuses()
            .iter()
            .any(|s| matches!(s, WriteStatus::Accepted))
    }

    /// `Then the conflict names "<a>" as expected and "<b>" as actual`.
    pub fn conflict_names(&mut self, expected: &str, actual: &str) -> bool {
        let expected = self.id_of(expected);
        let actual = self.id_of(actual);
        self.receipt_statuses().iter().any(|s| {
            matches!(
                s,
                WriteStatus::ReplaceableConflict { expected: e, actual: a }
                    if *e == Some(expected) && *a == Some(actual)
            )
        })
    }

    /// The id the last replacement froze, if it got that far.
    pub fn replacement_id(&mut self) -> Option<EventId> {
        self.frozen_id_of_last_publish()
    }

    /// The body id the world's most recent publish froze, waited for.
    /// `WriteStatus::Signed(id)` is the only place an app learns it.
    pub(super) fn frozen_id_of_last_publish(&mut self) -> Option<EventId> {
        self.receipt_eventually(|seen| seen.iter().any(|s| matches!(s, WriteStatus::Signed(_))));
        self.receipt_statuses().iter().find_map(|s| match s {
            WriteStatus::Signed(id) => Some(*id),
            _ => None,
        })
    }

    /// The timestamp the acceptance transaction settled on, read off the
    /// event that actually went out -- the only copy an app can inspect.
    pub fn replacement_created_at(&mut self) -> Option<Timestamp> {
        let id = self.replacement_id()?;
        self.admitted_event_with_id(id)
            .map(|event| event.created_at)
    }

    /// The timestamp of the version a scenario's word names.
    ///
    /// Read from what the scenario SAID when it staged that version, not off
    /// the wire. #995 retires a replaceable write's delivery obligation the
    /// moment a newer write takes the same coordinate, so a displaced
    /// predecessor may correctly never reach any relay -- and the sentence
    /// two lines above the assertion already states its timestamp outright.
    pub fn created_at_of(&self, label: &str) -> Option<Timestamp> {
        self.stated_created_at.get(label).copied()
    }

    /// The one foreign contact list this scenario put in the store, for the
    /// `Then` that asks whether it is unchanged without naming it again.
    pub fn only_foreign_contact_list_label(&self) -> String {
        let mut labels = self.foreign_contact_lists.values();
        let only = labels
            .next()
            .cloned()
            .expect("nmp-bdd: this scenario stored no foreign contact list");
        assert!(
            labels.next().is_none(),
            "nmp-bdd: more than one foreign contact list is stored, so 'unchanged' is \
             ambiguous"
        );
        only
    }

    /// `Then the replacement is the stored winner` / `... for "<hex>"`.
    pub fn replacement_is_the_winner(&mut self, owner: &str) -> bool {
        let Some(id) = self.replacement_id() else {
            return false;
        };
        self.await_stored_winner(owner, id)
    }

    /// `Then the stored winner is still "<label>"` / `Then my own contact
    /// list is still "<label>"` / `Then "<hex>"'s contact list is unchanged`.
    pub fn stored_winner_is(&mut self, owner: &str, label: &str) -> bool {
        let expected = self.id_of(label);
        self.stored_winner_of(owner) == Some(expected)
    }

    /// The store's current winner at `owner`'s contact-list coordinate, read
    /// through an ordinary subscription -- the only door an app has.
    pub fn stored_winner_of(&mut self, owner: &str) -> Option<EventId> {
        self.read_winner(owner, None)
    }

    fn await_stored_winner(&mut self, owner: &str, expected: EventId) -> bool {
        self.read_winner(owner, Some(expected)) == Some(expected)
    }

    /// One bounded read of the winner. `expected` makes it a wait rather than
    /// a sample: the assertion "this is the winner now" has to survive the
    /// row still being on its way, while "what is the winner?" cannot wait
    /// for an answer it does not know.
    fn read_winner(&mut self, owner: &str, expected: Option<EventId>) -> Option<EventId> {
        let author = self.person(owner).public_key().to_hex();
        let relay = self.relay_url(OTHER_DEVICE);
        let (handle_id, rx) = self
            .handle()
            .subscribe(contact_list_query(&relay, &author))
            .expect("BDD subscription construction");
        let mut feed = FeedState::new(handle_id, rx);
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            feed.drain_available();
            // One coordinate has one winner; when a row is still on its way
            // the store can briefly hold the one it is replacing, so the
            // latest `created_at` is what "the winner" means.
            let latest = feed
                .rows
                .values()
                .map(|row| &row.event)
                .max_by_key(|event| (event.created_at, event.id))
                .map(|event| event.id);
            match (latest, expected) {
                (Some(id), Some(want)) if id == want => return latest,
                (Some(_), None) => return latest,
                _ => {}
            }
            if Instant::now() >= deadline {
                return latest;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // ---- id words --------------------------------------------------------

    fn bind_id(&mut self, label: &str, id: EventId) {
        self.id_labels.insert(label.to_string(), id);
    }

    /// The real id a scenario's word stands for.
    pub fn id_of(&self, label: &str) -> EventId {
        *self
            .id_labels
            .get(label)
            .unwrap_or_else(|| panic!("nmp-bdd: no version is bound to the word {label:?}"))
    }
}
