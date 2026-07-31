//! The write plane: what an app HANDS the publish door, and what came back
//! out of it.
//!
//! Two payload shapes live here because `features/writes/` is written about
//! exactly two, and the difference between them is the whole subject:
//!
//! - a **builder**, which carries no author, no id and no signature, so NMP
//!   fills those in and the app's identity selects who it publishes as
//!   (`event-builder.feature`);
//! - an **already-signed event**, which states its author in its own bytes,
//!   so nothing is filled in, nothing is re-signed, and an identity may only
//!   restate what is already there (`pre-signed-events.feature`).
//!
//! Apart from [`super::actions`] because those are stimuli phrased as a
//! person doing something ordinary ("I publish a note"), while these are
//! about the PAYLOAD's shape -- an app stating a created_at, carrying tags
//! NMP has never heard of, or handing over somebody else's signed event
//! verbatim. Apart from [`super::identity`] because that plane is about WHO a
//! write publishes as; this one is about WHAT it publishes.
//!
//! Every composed publish is kept in a list rather than keyed by the text it
//! says. `event-builder.feature` publishes the same word twice on purpose --
//! "the same logical event composed twice is two valid events" -- so a map
//! keyed by content could not tell the two apart, which is precisely the
//! claim under test.

use std::time::{Duration, Instant};

use nostr::{Event, EventId, JsonUtil, Tag, Timestamp};

use nmp::mechanism::delivery::WriteStatus;
use nmp_grammar::{Durability, EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};

use super::budgets::{EVENTUALLY, NEVER};
use super::observe::ReceiptState;
use super::NmpWorld;

/// One builder this scenario published: exactly what the app handed over,
/// and WHICH of the world's publishes it was.
///
/// The value is kept because "I never stated my own pubkey, created_at, id or
/// signature" is a claim about the value handed over, and nothing in the
/// result can answer it. The ordinal is kept because #995 made the world's
/// receipts one ordered list of every publish -- so a builder does not need a
/// second receipt list of its own, only a way back into that one.
pub(super) struct ComposedWrite {
    pub(super) builder: EventBuilder,
    pub(super) receipt: usize,
}

impl NmpWorld {
    // ---- composing a builder --------------------------------------------

    /// `When I compose an event of kind <n> saying "<text>" ...` -- staged,
    /// not published, because a scenario handing over a tag table composes on
    /// one line and publishes on the next.
    pub fn stage_composed_event(
        &mut self,
        kind: u16,
        text: &str,
        created_at: Option<Timestamp>,
        tags: Vec<Tag>,
    ) {
        self.pending_builder = Some(EventBuilder {
            kind: nostr::Kind::from(kind),
            tags,
            content: text.to_string(),
            created_at,
        });
    }

    /// `And I publish it` -- the staged builder, through the one publish
    /// door, with the routing NMP derives on its own.
    pub async fn publish_staged_event(&mut self) {
        let builder = self
            .pending_builder
            .take()
            .expect("nmp-bdd: nothing was composed for this step to publish");
        self.ensure_started().await;
        let statuses = self
            .handle()
            .publish(WriteIntent {
                payload: WritePayload::Event(builder.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::Auto,
                identity: Identity::Active,
                correlation: None,
            })
            .expect("BDD receipt correlation namespace must be available");
        self.last_publish_was_auto = true;
        // Not keyed by the text it says: `event-builder.feature` publishes the
        // same word twice on purpose ("the same logical event composed twice
        // is two valid events"), so the identity plane's by-text map could not
        // tell the two apart -- which is precisely the claim under test. The
        // world's own receipt list is already ordered by publish, so the
        // builder only has to remember which entry is its.
        self.last_receipt_text = None;
        self.composed.push(ComposedWrite {
            builder,
            receipt: self.receipts.len(),
        });
        self.receipts.push(ReceiptState::new(statuses));
    }

    /// The one-line form: compose and publish in a single step.
    pub async fn compose_and_publish_event(
        &mut self,
        kind: u16,
        text: &str,
        created_at: Option<Timestamp>,
        tags: Vec<Tag>,
    ) {
        self.stage_composed_event(kind, text, created_at, tags);
        self.publish_staged_event().await;
    }

    /// How many builders this scenario published.
    pub fn composed_count(&self) -> usize {
        self.composed.len()
    }

    /// `Then I never stated my own pubkey, created_at, id or signature` -- a
    /// claim about the VALUE handed over, and the reason the builder is kept.
    /// A builder has no pubkey, id or signature field to have stated, so the
    /// only one that can be checked at all is the timestamp, and its absence
    /// is what the scenario means.
    pub fn last_builder_stated_no_timestamp(&self) -> bool {
        self.composed
            .last()
            .expect("nmp-bdd: nothing was composed in this scenario")
            .builder
            .created_at
            .is_none()
    }

    /// The tags the last composed builder carried, as raw string lists, so a
    /// `Then` can compare them against what came back off the wire without
    /// either side going through a typed tag vocabulary NMP does not have.
    pub fn last_builder_tags(&self) -> Vec<Vec<String>> {
        self.composed
            .last()
            .expect("nmp-bdd: nothing was composed in this scenario")
            .builder
            .tags
            .iter()
            .map(|tag| tag.clone().to_vec())
            .collect()
    }

    /// Every status the `n`th composed publish has reported, waited out to
    /// its full settle window so a negative claim is not vacuous.
    pub fn composed_statuses(&mut self, index: usize) -> Vec<WriteStatus> {
        let receipt = self.composed_receipt(index);
        receipt.eventually_within(NEVER, |_| false);
        receipt.seen.clone()
    }

    /// `Then the write is accepted` / `Then both events are accepted`.
    pub fn composed_accepted(&mut self, index: usize) -> bool {
        self.composed_receipt(index)
            .eventually_within(EVENTUALLY, |seen| {
                seen.iter().any(|s| matches!(s, WriteStatus::Accepted))
            })
    }

    /// The id the engine froze for the `n`th composed publish, waited for.
    /// `WriteStatus::Signed(id)` is the only place an app learns it, and it
    /// is what makes "the published event" a thing a `Then` can point at even
    /// when two publishes say the same words.
    pub fn composed_event_id(&mut self, index: usize) -> Option<EventId> {
        let receipt = self.composed_receipt(index);
        receipt.eventually_within(EVENTUALLY, |seen| {
            seen.iter().any(|s| matches!(s, WriteStatus::Signed(_)))
        });
        receipt.seen.iter().find_map(|s| match s {
            WriteStatus::Signed(id) => Some(*id),
            _ => None,
        })
    }

    /// The world's receipt for the `n`th COMPOSED publish -- which is not
    /// necessarily its `n`th publish, since a `Given` may have published
    /// fixture state first.
    fn composed_receipt(&mut self, index: usize) -> &mut ReceiptState {
        let ordinal = self
            .composed
            .get(index)
            .unwrap_or_else(|| panic!("nmp-bdd: this scenario published no {index}th event"))
            .receipt;
        &mut self.receipts[ordinal]
    }

    /// The event the `n`th composed publish put on the wire, read off the
    /// relay that received it -- the only place an app can point at "the
    /// published event" and mean the thing the world actually got.
    pub fn composed_event(&mut self, index: usize) -> Event {
        let id = self
            .composed_event_id(index)
            .unwrap_or_else(|| panic!("nmp-bdd: the {index}th publish never reported a frozen id"));
        self.admitted_event_with_id(id).unwrap_or_else(|| {
            panic!("nmp-bdd: no relay in this world ever received the event {id}")
        })
    }

    /// The admitted event with exactly this id, waited for (bounded).
    pub fn admitted_event_with_id(&mut self, id: EventId) -> Option<Event> {
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            let hit = self
                .relays
                .values()
                .flat_map(|relay| relay.admitted_events())
                .find(|event| event.id == id);
            if hit.is_some() {
                return hit;
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // ---- publishing an already-signed event ------------------------------

    /// `Given that note is the signed event "<label>"` -- binds the word a
    /// scenario uses for it to the event a `Given` staged. A `.feature`
    /// cannot spell a real event id (one only exists after signing), so the
    /// word is a binding and every later step naming it compares against what
    /// the event actually got.
    pub fn bind_signed_event_label(&mut self, label: &str) {
        let event = self
            .pending_signed_notes
            .last()
            .map(|(_, event)| event.clone())
            .or_else(|| {
                let mut all = self.signed_notes.values();
                let only = all.next().cloned();
                all.next().is_none().then_some(only).flatten()
            })
            .expect("nmp-bdd: no note was staged for this label to name");
        self.signed_by_label.insert(label.to_string(), event);
    }

    /// `Given the signed event "<label>" has had one byte of its content
    /// altered` -- a REAL forgery: the bytes are changed and the signature is
    /// left alone, which is exactly the payload the acceptance boundary has
    /// to catch.
    pub fn tamper_signed_event(&mut self, label: &str) {
        let event = self.signed_event_by_label(label);
        // Every other field -- id and signature included -- is carried over
        // untouched. That is what makes this a forgery rather than a
        // malformed value: the payload is perfectly well-formed and its
        // signature simply no longer commits to its content.
        let tampered = Event::new(
            event.id,
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags.clone().to_vec(),
            format!("{}!", event.content),
            event.sig,
        );
        self.signed_by_label.insert(label.to_string(), tampered);
    }

    /// `When I publish the signed event "<label>" as-is to "<relay>"` and its
    /// `naming identity "<hex>"` form.
    ///
    /// Never routed `Auto`: the archive case is an app naming the one relay
    /// it wants the bytes to reach, and a signed event's author has no
    /// relation to the publishing app's own outbox.
    pub async fn publish_signed_event(&mut self, label: &str, relay: &str, identity: Identity) {
        if !self.relay_configs.contains_key(relay) {
            self.register_bystander_relay(relay);
        }
        self.ensure_started().await;
        let event = self.signed_event_by_label(label);
        let routing = WriteRouting::Explicit([self.relay_url(relay)].into_iter().collect());
        self.handed_over = Some(event.clone());
        self.republished = Some(event.clone());
        let statuses = self
            .handle()
            .publish(WriteIntent {
                payload: WritePayload::Signed(event),
                durability: Durability::Durable,
                routing,
                identity,
                correlation: None,
            })
            .expect("BDD receipt correlation namespace must be available");
        self.last_publish_was_auto = false;
        self.last_receipt_text = None;
        self.receipts.push(ReceiptState::new(statuses));
    }

    /// The one id word this scenario bound, for the `When I publish IT as-is`
    /// form -- the pronoun only means anything when there is exactly one.
    pub fn only_signed_event_label(&self) -> String {
        let mut labels = self.signed_by_label.keys();
        let only = labels
            .next()
            .cloned()
            .expect("nmp-bdd: no signed event was named for 'it' to refer back to");
        assert!(
            labels.next().is_none(),
            "nmp-bdd: 'it' is ambiguous -- this scenario named more than one signed event"
        );
        only
    }

    /// The event bound to a scenario's id word.
    pub fn signed_event_by_label(&self, label: &str) -> Event {
        self.signed_by_label
            .get(label)
            .cloned()
            .unwrap_or_else(|| panic!("nmp-bdd: no signed event is bound to {label:?}"))
    }

    /// The event a `When I publish ... as-is` actually handed the door.
    pub fn handed_over_event(&self) -> Event {
        self.handed_over
            .clone()
            .expect("nmp-bdd: this scenario handed no signed event over")
    }

    /// `Then "<relay>" received exactly the bytes I handed over` -- compared
    /// as canonical JSON, which is every field including the signature.
    pub fn relay_received_handed_over_bytes(&mut self, relay: &str) -> bool {
        let expected = self.handed_over_event();
        let Some(received) = self.await_admitted_event_at(relay, expected.id) else {
            return false;
        };
        received.as_json() == expected.as_json()
    }

    /// The event with this id that reached `relay`, waited for (bounded).
    pub fn await_admitted_event_at(&mut self, relay: &str, id: EventId) -> Option<Event> {
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            let hit = self
                .relays
                .get(relay)
                .and_then(|r| r.admitted_events().into_iter().find(|e| e.id == id));
            if hit.is_some() {
                return hit;
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // ---- how many times one relay was OFFERED one event -------------------

    /// `Then "<relay>" was offered the note exactly once` / `is offered the
    /// note again`.
    ///
    /// An OFFER, not a delivery: a relay that already holds an event
    /// deduplicates it and its stored copy count says nothing about how many
    /// times the client sent it. `ScriptedRelay`'s admitted log is the write
    /// policy's own record of every EVENT frame it was handed, which is
    /// exactly the bandwidth the design bounds.
    pub fn offers_of(&mut self, relay: &str, id: EventId) -> usize {
        self.relays
            .get(relay)
            .unwrap_or_else(|| panic!("nmp-bdd: relay {relay:?} does not exist in this world"))
            .admitted_events()
            .iter()
            .filter(|event| event.id == id)
            .count()
    }

    /// Bounded wait for the FIRST offer -- the precondition of every count
    /// above, and it has to be a wait rather than a read.
    ///
    /// A receipt beat and a socket write are not the same instant. Most
    /// scenarios here say `the receipt reports the note acked by "<relay>"`
    /// first, which incidentally guarantees the frame already landed; one
    /// that asserts on the ROUTE instead ("the receipt reports exactly one
    /// destination") reaches this while the write is still in flight, and a
    /// one-shot read then reports zero offers -- truthfully, and about
    /// nothing. Waiting is also strictly safer for the count that follows: it
    /// gives a second offer more time to show up, never less.
    pub async fn wait_for_offer(&mut self, relay: &str, id: EventId) -> bool {
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            if self.offers_of(relay, id) > 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// How many DISTINCT copies of that event this relay holds -- the other
    /// half of "offered twice, holds one".
    pub fn copies_held_by(&mut self, relay: &str, id: EventId) -> usize {
        self.relays
            .get(relay)
            .unwrap_or_else(|| panic!("nmp-bdd: relay {relay:?} does not exist in this world"))
            .admitted_events()
            .iter()
            .filter(|event| event.id == id)
            .map(|event| event.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    /// The id of the note the world's one "the publish" stream froze.
    pub fn last_published_id(&mut self) -> Option<EventId> {
        self.receipt_eventually(|seen| seen.iter().any(|s| matches!(s, WriteStatus::Signed(_))));
        self.receipt_statuses().iter().find_map(|s| match s {
            WriteStatus::Signed(id) => Some(*id),
            _ => None,
        })
    }

    /// `Then the event was not re-signed as "<hex>"` -- nothing anywhere in
    /// this world carries that key as the author of these bytes.
    pub fn nothing_was_authored_by(&mut self, label: &str) -> bool {
        let pubkey = self.person(label).public_key();
        let content = self.handed_over_event().content;
        !self
            .relays
            .values()
            .flat_map(|relay| relay.admitted_events())
            .any(|event| event.pubkey == pubkey && event.content == content)
    }
}
