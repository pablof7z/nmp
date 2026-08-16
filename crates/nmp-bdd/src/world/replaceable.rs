//! The replaceable plane: which version of a whole-value event is the local
//! winner.
//!
//! Its own module because the subject is a THIRD party to every other write
//! plane in this world -- the STORE's row. `writes` is about the payload an
//! app hands over and `identity` about who it publishes as; neither expresses
//! "which version is the coordinate's current winner", which is what
//! `Given my contact list "<label>" ... is the stored winner` (the shared
//! Background step every scenario in `features/writes/` that needs a
//! pre-existing winner uses) establishes.
//!
//! One harness decision is worth stating outright: **every winner is
//! established through the ONE publish door.** A scenario that says "my
//! contact list X is the stored winner" gets a real accepted kind:3 with the
//! stated `created_at`, and the word X is bound to the id that write
//! actually froze. Nothing is written into the store behind the engine's
//! back. Those fixture writes are routed to a relay no scenario names
//! ([`OTHER_DEVICE`]) -- a publish must name somewhere, and sending the
//! Background's winner to `wss://hub.example` would make "hub received
//! nothing" false for reasons that have nothing to do with the write under
//! test. The name is the scenario's own: these writes are the ones "another
//! device" made.

use std::time::{Duration, Instant};

use nostr::{EventId, Timestamp};

use nmp_engine::publish_queue::{SigningState, WriteFact};
use nmp_grammar::{EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};

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
    /// winner`.
    pub async fn stage_stored_winner(&mut self, owner: &str, label: &str, at: Timestamp) {
        if !self.relay_configs.contains_key(OTHER_DEVICE) {
            self.register_bystander_relay(OTHER_DEVICE);
        }
        self.ensure_started().await;
        let pubkey = self.person(owner).public_key();
        let routing = WriteRouting::Explicit([self.relay_url(OTHER_DEVICE)].into_iter().collect());
        let result = self.handle().publish(WriteIntent {
            payload: WritePayload::Event(EventBuilder {
                kind: nostr::Kind::ContactList,
                tags: Vec::new(),
                content: String::new(),
                created_at: Some(at),
            }),
            routing,
            identity: Identity::Explicit(pubkey),
            correlation: None,
        });
        // A fixture winner is still a publish, so it takes its place in the
        // world's one ordered receipt list (#995) rather than opening a
        // private stream nothing else can see.
        self.receipts.push(ReceiptState::from_publish(result));
        let frozen = self.frozen_id_of_last_publish().unwrap_or_else(|| {
            panic!(
                "nmp-bdd: staging a stored winner must be accepted and frozen; saw {:?}",
                self.receipt_statuses()
            )
        });
        self.bind_id(label, frozen);
        // The engine has to have finished writing the row before the next
        // step reads against it, and the row is what the app can read.
        assert_eq!(
            self.stored_winner_of(owner),
            Some(frozen),
            "nmp-bdd: the staged version must be the store's winner before anything reads \
             against it"
        );
    }

    // ---- Then: whether the accepted write went through --------------------

    /// `Then the write is accepted` -- `publish()` returning `Ok`. Also read
    /// by `features/diagnostics/stalled-writes.feature`, whose subject is a
    /// write nothing can deliver rather than a replacement: "the obligation
    /// was accepted" is the same observable in both.
    pub fn replacement_accepted(&mut self) -> bool {
        self.publish_was_accepted()
    }

    /// The body id the world's most recent publish froze, waited for.
    /// `SigningState::Signed` is the only place an app learns it.
    pub(super) fn frozen_id_of_last_publish(&mut self) -> Option<EventId> {
        self.receipt_eventually(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteFact::Signing(SigningState::Signed { .. })))
        });
        self.receipt_statuses().iter().find_map(|s| match s {
            WriteFact::Signing(SigningState::Signed { event_id }) => Some(*event_id),
            _ => None,
        })
    }

    /// The store's current winner at `owner`'s contact-list coordinate, read
    /// through an ordinary subscription -- the only door an app has.
    pub fn stored_winner_of(&mut self, owner: &str) -> Option<EventId> {
        self.read_winner(owner, None)
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
                .max_by_key(|row| (row.created_at(), row.id()))
                .map(|row| row.id());
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
