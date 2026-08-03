//! The global "is anything quietly stuck" read-out, and the two acts a
//! scenario about it performs: publishing to a relay that does not exist,
//! and READING diagnostics -- repeatedly, on purpose, to prove that reading
//! is not part of the system it describes.
//!
//! Its own module rather than a corner of `observe` because the population
//! it describes is the write plane's, not the read plane's: a stalled write
//! is an obligation nobody is holding a receipt for, which is exactly the
//! thing every other observable in this world reaches through a receipt.

use nmp::mechanism::core::{DiagnosticsSnapshot, StalledWrite};
use nmp_grammar::{Durability, EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_router::RelayUrl;

use super::budgets::EVENTUALLY;
use super::observe::ReceiptState;
use super::NmpWorld;

impl NmpWorld {
    /// `Given I am told to publish a note to exactly "<url>"` -- a LITERAL
    /// URL, deliberately never bound to a scripted relay.
    ///
    /// Every other relay this world names is started on a real port before
    /// anything is published, which is what makes them reachable. This one
    /// must not be: the whole point of the case is a destination that is
    /// precisely the one the user named and that nothing in the world
    /// answers for. Registering it as a bystander relay would give it a real
    /// socket and quietly delete the scenario.
    pub fn told_to_publish_to(&mut self, url: &str) {
        let parsed =
            RelayUrl::parse(url).unwrap_or_else(|_| panic!("nmp-bdd: {url:?} is not a relay URL"));
        assert!(
            !self.relay_configs.contains_key(url),
            "nmp-bdd: {url:?} is a relay this world starts, so it cannot stand for one that \
             does not exist"
        );
        self.told_route = vec![parsed];
    }

    /// `When I publish that note` -- the note the step above told this world
    /// where to send.
    pub async fn publish_told_note(&mut self) {
        assert!(
            !self.told_route.is_empty(),
            "nmp-bdd: nothing told this world where to publish"
        );
        self.publish_signed_note_to_urls("nowhere in particular")
            .await;
    }

    /// `Given a note saying "<text>" was published and signed` -- accepted,
    /// signed, and routed, so whatever happens next is a DELIVERY fact and
    /// never a signing or routing one.
    pub async fn publish_and_await_signature(&mut self, text: &str) {
        self.publish_note(text).await;
        let signed = self.receipt_eventually(|seen| {
            seen.iter().any(|status| {
                matches!(
                    status,
                    nmp::mechanism::publish_queue::WriteStatus::Signed(_)
                )
            })
        });
        assert!(
            signed,
            "nmp-bdd: expected the note to be signed; receipt showed {:?}",
            self.receipt_statuses()
        );
    }

    async fn publish_signed_note_to_urls(&mut self, text: &str) {
        self.ensure_started().await;
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: publishing a note needs a logged-in account");
        let _ = self.person(&me);
        self.snapshot_relay_contacts();
        let rx = self
            .handle()
            .publish(WriteIntent {
                payload: WritePayload::Event(
                    EventBuilder::new(nostr::Kind::TextNote).content(text),
                ),
                durability: Durability::Durable,
                routing: WriteRouting::Explicit(self.told_route.clone()),
                identity: Identity::Active,
                correlation: None,
            })
            .expect("BDD receipt correlation namespace must be available");
        self.receipts.push(ReceiptState::new(rx));
    }

    /// `When I read diagnostics` -- bounded-wait until the snapshot has
    /// something to say about stalled writes, then keep it as "the list"
    /// every following `Then` reads.
    ///
    /// The wait is what makes the read honest rather than lucky: diagnostics
    /// is a latest-wins stream, so a read taken the instant after a publish
    /// would describe a world that has not finished connecting yet.
    pub fn read_stalled_writes(&mut self) {
        let snapshot = self
            .diagnostics_matching(|snapshot| !snapshot.stalled_writes.is_empty())
            .or_else(|| self.diagnostics_matching(|_| true));
        self.last_diagnostics = snapshot;
    }

    /// The same read, bounded the other way: wait until the list is EMPTY of
    /// everything it named before, which is what "leaves the list" means.
    pub fn read_stalled_writes_until_empty(&mut self) {
        let snapshot = self
            .diagnostics_matching(|snapshot| snapshot.stalled_writes.is_empty())
            .or_else(|| self.diagnostics_matching(|_| true));
        self.last_diagnostics = snapshot;
    }

    /// `When I read diagnostics <n> times` -- and keep every answer, so a
    /// `Then` can prove they were all the same rather than merely that the
    /// last one looked fine.
    pub fn read_diagnostics_repeatedly(&mut self, times: usize) {
        // One settled read first: everything after it is compared against
        // this, so it has to describe a world that has stopped moving for
        // reasons of its own rather than one still starting up. The contact
        // baseline is taken AFTER it, for the same reason -- a count sampled
        // while start-up traffic is still in flight is one short, and the
        // reads below would be blamed for it.
        self.read_stalled_writes();
        self.snapshot_relay_contacts();
        let mut seen = Vec::new();
        for _ in 0..times {
            let snapshot = self
                .diagnostics_matching(|_| true)
                .expect("nmp-bdd: diagnostics must have delivered at least one snapshot");
            seen.push(stalled_fingerprint(&snapshot));
        }
        self.repeated_diagnostics = seen;
    }

    /// The bounded detail rows the last read returned.
    pub fn stalled_writes(&self) -> Vec<StalledWrite> {
        self.last_diagnostics
            .as_ref()
            .map(|snapshot| snapshot.stalled_writes.clone())
            .unwrap_or_default()
    }

    /// The exact census behind them.
    pub fn stalled_write_totals(&self) -> nmp::mechanism::core::StalledWriteTotals {
        self.last_diagnostics
            .as_ref()
            .map(|snapshot| snapshot.stalled_write_totals)
            .unwrap_or_default()
    }

    /// Every fingerprint [`Self::read_diagnostics_repeatedly`] collected.
    pub fn repeated_stalled_fingerprints(&self) -> &[Vec<(String, String, u64)>] {
        &self.repeated_diagnostics
    }

    /// Remember the row a scenario just named, so a later step can prove the
    /// SAME row left the list rather than merely that the list shrank.
    pub fn remember_named_stalled_write(&mut self, id: &str) {
        self.named_stalled_write = Some(id.to_string());
    }

    pub fn named_stalled_write(&self) -> Option<&str> {
        self.named_stalled_write.as_deref()
    }

    /// The reader's "now" -- the instant the ENGINE is running at.
    ///
    /// NMP reports WHEN an obligation was accepted and never how long ago, so
    /// the elapsed side of "stalled for about 40 days" belongs to whoever is
    /// reading. That reader has to subtract against the same clock the
    /// acceptance was stamped by, which is the engine's own stated one
    /// (`world::clock`, #1013) and never the host's wall clock -- a scenario
    /// that advanced the stated clock by forty days has not waited forty
    /// days, and reading the real one would report an age of seconds.
    pub fn reader_now(&self) -> u64 {
        self.stated_clock()
            .unwrap_or_else(nostr::Timestamp::now)
            .as_secs()
    }

    /// A bounded settle over the receipt stream, used by the "nothing
    /// changed" assertions: it costs the full negative budget by
    /// construction.
    pub fn receipt_statuses_after_settling(
        &mut self,
    ) -> Vec<nmp::mechanism::publish_queue::WriteStatus> {
        let _ = self.receipt_never(|_| false);
        self.receipt_statuses()
    }

    /// The window every bounded stalled-write read uses. Exposed so a step
    /// can say how long it waited when it fails.
    pub fn stalled_read_budget() -> std::time::Duration {
        EVENTUALLY
    }
}

/// The comparable shape of one snapshot's stalled-write section: every row's
/// descriptor, stage and acceptance instant, in the order delivered.
///
/// Deliberately not the whole snapshot: relay-plane counters move for
/// reasons that have nothing to do with reading, and a "nothing changed"
/// assertion that compared them would be asserting something else.
fn stalled_fingerprint(snapshot: &DiagnosticsSnapshot) -> Vec<(String, String, u64)> {
    snapshot
        .stalled_writes
        .iter()
        .map(|write| {
            (
                write.id.clone(),
                format!("{:?}", write.stage),
                write.stalled_since.as_secs(),
            )
        })
        .collect()
}
