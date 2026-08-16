//! The process boundary: stopping the engine, and reconstructing it over the
//! same durable store.
//!
//! Its own module because a restart is not a claim about identity, routing or
//! payloads -- it is a claim about what SURVIVES, and several feature
//! directories make one. `features/identity/` asks whether a decided author
//! was reloaded rather than re-resolved;
//! `features/routing/idempotent-resends.feature` asks whether an acked
//! destination is left alone across it. Different subjects, one mechanism,
//! and the mechanism is what lives here.
//!
//! It has exactly two rules, and both are what separates a genuine boundary
//! from a handle swap:
//!
//! - everything the APP was holding open is given up FIRST -- a feed, a
//!   diagnostics stream, a receipt stream are all things a process loses --
//!   and the engine is stopped through `staging::stop_engine`, the single
//!   shutdown definition teardown also uses (#977), so the two cannot drift;
//! - the store is the SAME FILE, which is why a scenario that says it
//!   reconstructs its engine runs on a real on-disk one (#974, and
//!   `tests/bdd.rs`'s before-hook, which reads that sentence out of the
//!   scenario itself rather than making a `.feature` name a storage engine).
//!
//! So anything a write still knows about itself on the far side came off the
//! journal, and nothing else.

use std::time::{Duration, Instant};

use nmp_runtime::ReceiptReattachment;

use super::budgets::EVENTUALLY;
use super::observe::{FeedState, ReceiptState};
use super::queries::authored_note_query;
use super::NmpWorld;

impl NmpWorld {
    /// `When I reconstruct the engine from the same durable store` -- a
    /// genuine process boundary: the engine thread is stopped and a fresh
    /// one is built over the SAME on-disk store. Nothing in memory survives,
    /// so anything the write still knows about itself afterwards came off
    /// the journal.
    pub async fn restart_engine(&mut self, active: Option<String>) {
        self.ensure_started().await;
        // A scenario may have said `And the process stops` on the previous
        // line, in which case there is no engine left to read a frozen body
        // out of -- and that is the whole point of having said it.
        if self.handle.is_some() {
            self.last_receipt_body = self.frozen_body_id();
        }
        self.give_up_everything_the_app_held();
        self.receipts_by_text.clear();
        self.restarted_receipt = None;
        self.last_receipt_text = None;
        self.stop_engine();
        self.active_person = active;
        self.spawn_engine().await;
        if let Some(id) = self.last_receipt_id {
            match self.handle().reattach_receipt(id) {
                ReceiptReattachment::Attached { statuses, .. } => {
                    self.restarted_receipt = Some(ReceiptState::new(statuses));
                }
                _ => panic!(
                    "nmp-bdd: a durable receipt must still reattach by its stable id after a \
                     restart -- that is what makes an accepted write survive the process"
                ),
            }
        }
    }

    /// `When the process stops` / `And the process stops with the note
    /// undelivered` -- the same boundary [`Self::restart_engine`] crosses,
    /// said on its own line because several `features/routing/` scenarios
    /// stop and reconstruct as two separate acts.
    ///
    /// The frozen body is read BEFORE the engine goes, because afterwards
    /// there is nothing to read it from -- which is exactly what makes this a
    /// process boundary rather than a handle swap.
    pub async fn stop_process(&mut self) {
        self.ensure_started().await;
        self.last_receipt_body = self.frozen_body_id();
        self.give_up_everything_the_app_held();
        self.stop_engine();
    }

    /// A feed, a set of watches and a diagnostics stream are all things a
    /// process loses when it stops. Given up in one place so a stop and a
    /// restart cannot disagree about what a process boundary costs.
    fn give_up_everything_the_app_held(&mut self) {
        self.feed = None;
        self.watches.clear();
        self.diag = None;
    }

    /// The id of the body the engine FROZE at acceptance, read off the
    /// pending row the engine projects into its own local rows before any
    /// signer answers.
    ///
    /// An event id commits to author, content and timestamp together, so
    /// this one value IS the frozen body: a restart that re-resolved the
    /// identity, restamped the clock, or recomposed anything would produce a
    /// different one. Read through an ordinary subscription rather than the
    /// store, because that is the only body an app can see.
    fn frozen_body_id(&mut self) -> Option<nostr::EventId> {
        let label = self.last_publish_label.clone()?;
        let author = self.person(&label).public_key().to_hex();
        let relay_name = self.write_relay_of(&label).first().cloned()?;
        let relay = self.relay_url(&relay_name);
        let (handle_id, rx) = self
            .handle()
            .subscribe(authored_note_query(&relay, &author, None))
            .expect("BDD subscription construction");
        let mut feed = FeedState::new(handle_id, rx);
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            feed.drain_available();
            if let Some(id) = feed.rows.keys().next().copied() {
                return Some(id);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `Then its frozen body is byte-for-byte what it was before the
    /// restart` -- an unchanged id IS an unchanged body, for the reason
    /// above.
    pub fn frozen_body_unchanged_across_restart(&mut self) -> bool {
        let before = self
            .last_receipt_body
            .expect("nmp-bdd: nothing was frozen before the restart to compare against");
        self.frozen_body_id() == Some(before)
    }
}
