//! The observation plane: every channel the world accumulates outcomes on,
//! and every bounded observer a `Then` step is allowed to read them through.
//!
//! Both halves belong together because they are the same contract seen from
//! two sides. `Handle::subscribe`'s wire is DELTAS and `observe_diagnostics`'
//! is a single latest-wins slot, so an assertion can only be written against
//! state something has been folding all along -- the three accumulators here
//! (`FeedState`, `ReceiptState`, `DiagFeed`) are that folding, and the `pub`
//! observers below are the only way out of them. Keeping the accumulator and
//! its observer in one file is what stops a `Then` step from ever reaching
//! past the fold into a raw receiver.
//!
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nostr::EventId;

use nmp_engine::core::{
    AcquisitionEvidence, DiagnosticsSnapshot, PublishError, Row, RowDelta, ShortfallFact,
};
use nmp_engine::publish_queue::{SigningState, WriteFact, WriteOutcome};
use nmp_runtime::{fifo_channel, DiagnosticsHandle, QueryHandle, RowsReceiver};

use super::acquisition::branch_shortfall;
use super::budgets::{EVENTUALLY, NEVER};
use super::NmpWorld;

/// One accumulated feed: folds every `Added`/`SourcesGrew`/`Removed` delta
/// this channel has delivered so far into a live row set + the query's latest
/// acquisition evidence -- exactly what a real app must do
/// (`Handle::subscribe`'s wire is deltas, never snapshots). Persists across
/// multiple `Then` steps in one scenario.
///
/// The fold keeps the WHOLE [`Row`], not just its event, because the row's
/// relay-provenance set is half of what the delta stream carries and an app
/// that dropped it could not tell one relay's copy of an event from two
/// relays' (see [`super::provenance`]).
pub(super) struct FeedState {
    pub(super) handle: QueryHandle,
    pub(super) rx: RowsReceiver,
    pub(super) rows: BTreeMap<EventId, Row>,
    /// Per-BRANCH acquisition evidence in canonical branch order (#1108).
    pub(super) evidence: Vec<AcquisitionEvidence>,
    /// Whether this observation's OWN wire filters are downstream of rows it
    /// has to ingest first (a `Derived` binding), so a wire assertion taken
    /// while it is open must first wait for
    /// [`super::acquisition::every_source_has_proven_its_subtree`] (#1211).
    pub(super) resolves_from_ingest: bool,
}

impl FeedState {
    /// A fresh, empty accumulator over `handle`'s delta stream.
    pub(super) fn new(handle: QueryHandle, rx: RowsReceiver) -> Self {
        Self {
            handle,
            rx,
            rows: BTreeMap::new(),
            evidence: Vec::new(),
            resolves_from_ingest: false,
        }
    }

    /// Mark this observation as one whose outer filters cannot exist until
    /// an INNER demand's rows have been ingested (#1211).
    pub(super) fn resolving_from_ingest(mut self) -> Self {
        self.resolves_from_ingest = true;
        self
    }

    pub(super) fn drain_available(&mut self) {
        while let Ok((deltas, evidence, _execution)) = self.rx.try_recv() {
            self.apply(deltas, evidence);
        }
    }

    fn apply(&mut self, deltas: Vec<RowDelta>, evidence: Vec<AcquisitionEvidence>) {
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    self.rows.insert(row.id(), row);
                }
                RowDelta::Updated(row) => {
                    self.rows.insert(row.id(), row);
                }
                // #105: the event body is unchanged, so this replaces the
                // row's source set and nothing else -- the "whole value, not
                // a patch" shape the delta itself carries. A row this handle
                // has never been `Added` cannot grow sources, so an unknown
                // id is dropped rather than invented.
                RowDelta::SourcesGrew { id, sources } => {
                    if let Some(row) = self.rows.get_mut(&id) {
                        row.sources = sources;
                    }
                }
                RowDelta::Removed(id) => {
                    self.rows.remove(&id);
                }
            }
        }
        self.evidence = evidence;
    }

    /// Block (bounded) until `pred` holds against the accumulated state,
    /// draining every message that arrives in the meantime. Checks the
    /// CURRENT state first (no waiting) since a prior step's activity may
    /// already satisfy `pred`.
    pub(super) fn eventually(&mut self, timeout: Duration, pred: impl Fn(&Self) -> bool) -> bool {
        self.drain_available();
        if pred(self) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.rx.recv_timeout(remaining) {
                Ok((deltas, coverage, _execution)) => {
                    self.apply(deltas, coverage);
                    if pred(self) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// Block the FULL window, returning `true` iff `pred` never held at any
    /// point during it (the settle-window half of "never" -- approach doc
    /// §1.3).
    fn never(&mut self, timeout: Duration, pred: impl Fn(&Self) -> bool) -> bool {
        self.drain_available();
        if pred(self) {
            return false;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            match self.rx.recv_timeout(remaining) {
                Ok((deltas, coverage, _execution)) => {
                    self.apply(deltas, coverage);
                    if pred(self) {
                        return false;
                    }
                }
                Err(_) => return true,
            }
        }
    }
}

/// Does this fact END the write badly -- the write is over and it did not
/// publish?
///
/// Cancellation and supersession are deliberately excluded: the app asked for
/// the first, and the second is the steady state of an app renewing a
/// replaceable coordinate ("not a failure" -- `WriteOutcome::NotSent`'s own
/// doc). What is left is the signer saying no, the store saying no, and
/// routing finishing with nowhere to publish.
pub(super) fn is_failure_fact(fact: &WriteFact) -> bool {
    matches!(
        fact,
        WriteFact::Signing(SigningState::Refused { .. })
            | WriteFact::Outcome(WriteOutcome::Refused(_))
            | WriteFact::Outcome(WriteOutcome::NoDestination)
    )
}

/// Does this fact end the write at all? Exactly one `Outcome` closes every
/// receipt stream, which is what lets a bounded read stop on a FACT rather
/// than on the stream going quiet.
pub(super) fn is_outcome_fact(fact: &WriteFact) -> bool {
    matches!(fact, WriteFact::Outcome(_))
}

/// The receipt stream for the most recent `publish` (the starter catalog
/// only ever names "the receipt", singular -- one implicit publish in
/// flight per scenario).
pub(super) struct ReceiptState {
    pub(super) rx: nmp_runtime::FifoReceiver<WriteFact>,
    pub(super) seen: Vec<WriteFact>,
    /// `Some` when `publish()` itself refused. Custody is what `Ok` means, so
    /// a refusal is not a fact ON a stream -- there is no stream, no receipt
    /// id and no queue entry. Keeping it here rather than on the world is
    /// what lets the identity plane's by-text lookup answer "was THIS write
    /// refused?" the same way it answers everything else.
    pub(super) refusal: Option<PublishError>,
}

impl ReceiptState {
    /// A fresh accumulator over one publish's fact stream.
    pub(super) fn new(rx: nmp_runtime::FifoReceiver<WriteFact>) -> Self {
        Self {
            rx,
            seen: Vec::new(),
            refusal: None,
        }
    }

    /// A publish the door refused. The channel is created already closed (the
    /// sender is dropped immediately), so every bounded read over it returns
    /// at once instead of spending a window waiting for facts that cannot
    /// exist.
    pub(super) fn refused(error: PublishError) -> Self {
        let (_closed, rx) = fifo_channel();
        Self {
            rx,
            seen: Vec::new(),
            refusal: Some(error),
        }
    }

    /// The one place a `publish()` answer becomes an observation. Every
    /// publishing step in this world goes through here, so "the door refused
    /// it" is recorded the same way wherever it happens rather than
    /// `expect`ed away at one call site and handled at another.
    pub(super) fn from_publish(result: Result<nmp::ReceiptStream, PublishError>) -> Self {
        match result {
            Ok(receipt) => Self::new(receipt.statuses),
            Err(error) => Self::refused(error),
        }
    }

    fn drain_available(&mut self) {
        while let Ok(status) = self.rx.try_recv() {
            self.seen.push(status);
        }
    }

    fn eventually(&mut self, timeout: Duration, pred: impl Fn(&[WriteFact]) -> bool) -> bool {
        self.drain_available();
        if pred(&self.seen) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(status) => {
                    self.seen.push(status);
                    if pred(&self.seen) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// The same bounded read as [`Self::eventually`], at a CALLER-chosen
    /// window. `eventually` fixes it at `EVENTUALLY`, which is right for the
    /// world's single "the publish", but the identity plane needs both that
    /// and the shorter `NEVER` settle over receipts it keys by the text a
    /// scenario published.
    pub(super) fn eventually_within(
        &mut self,
        timeout: Duration,
        pred: impl Fn(&[WriteFact]) -> bool,
    ) -> bool {
        self.drain_available();
        if pred(&self.seen) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(status) => {
                    self.seen.push(status);
                    if pred(&self.seen) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }
}

/// Forwards the single-slot `LatestReceiver<DiagnosticsSnapshot>` into a
/// `Condvar`-signalled shared slot so callers can `wait_timeout` on it (the
/// runtime's own `LatestReceiver::recv` has no timeout variant -- see its
/// doc; this is the same "latest wins" idea, made boundedly pollable).
pub(super) struct DiagFeed {
    _handle: DiagnosticsHandle,
    shared: Arc<(Mutex<Option<DiagnosticsSnapshot>>, Condvar)>,
    _forwarder: JoinHandle<()>,
}

impl DiagFeed {
    pub(super) fn new(
        handle: DiagnosticsHandle,
        rx: nmp_runtime::LatestReceiver<DiagnosticsSnapshot>,
    ) -> Self {
        let shared = Arc::new((Mutex::new(None), Condvar::new()));
        let shared2 = Arc::clone(&shared);
        let forwarder = thread::spawn(move || {
            while let Some(snapshot) = rx.recv() {
                let (lock, cvar) = &*shared2;
                *lock.lock().expect("nmp-bdd: diagnostics forwarder lock") = Some(snapshot);
                cvar.notify_all();
            }
        });
        Self {
            _handle: handle,
            shared,
            _forwarder: forwarder,
        }
    }

    /// Block (bounded) until `pred` holds against the latest snapshot,
    /// returning the snapshot that satisfied it (`None` on timeout).
    fn get(
        &self,
        timeout: Duration,
        pred: impl Fn(&DiagnosticsSnapshot) -> bool,
    ) -> Option<DiagnosticsSnapshot> {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().expect("nmp-bdd: diagnostics lock");
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(snapshot) = guard.as_ref() {
                if pred(snapshot) {
                    return Some(snapshot.clone());
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (g, _timeout) = cvar
                .wait_timeout(guard, remaining)
                .expect("nmp-bdd: diagnostics wait");
            guard = g;
        }
    }
}

impl NmpWorld {
    // ---- Then observables ---------------------------------------------

    pub fn feed_eventually(
        &mut self,
        pred: impl Fn(&[&Row], &[AcquisitionEvidence]) -> bool,
    ) -> bool {
        let feed = self.feed.as_mut().expect("nmp-bdd: no feed is open");
        feed.eventually(EVENTUALLY, |f| {
            let rows: Vec<&Row> = f.rows.values().collect();
            pred(&rows, &f.evidence)
        })
    }

    /// How many OPEN WATCHES have been told, in their own acquisition
    /// evidence, that some of what they asked for could not be requested
    /// locally (`ShortfallFact::LocalLimit`).
    ///
    /// This is the app-facing half of a bound subscription budget, and the
    /// half that matters most: a diagnostics count tells an operator, this
    /// tells the subscriber. Bounded-polls until `expected` watches report
    /// it, then returns whatever the count actually is, so a failure message
    /// can say how far off it was.
    pub fn watches_reporting_a_local_limit(&mut self, expected: usize) -> usize {
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            let mut reporting = 0;
            for watch in self.watches.values_mut() {
                watch.drain_available();
                if branch_shortfall(&watch.evidence)
                    .any(|fact| matches!(fact, ShortfallFact::LocalLimit { .. }))
                {
                    reporting += 1;
                }
            }
            if reporting >= expected || Instant::now() >= deadline {
                return reporting;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn feed_never(&mut self, pred: impl Fn(&[&Row]) -> bool) -> bool {
        let feed = self.feed.as_mut().expect("nmp-bdd: no feed is open");
        feed.never(NEVER, |f| {
            let rows: Vec<&Row> = f.rows.values().collect();
            pred(&rows)
        })
    }

    pub fn receipt_eventually(&mut self, pred: impl Fn(&[WriteFact]) -> bool) -> bool {
        let receipt = self
            .receipts
            .last_mut()
            .expect("nmp-bdd: no publish is in flight");
        receipt.eventually(EVENTUALLY, pred)
    }

    /// The same bounded wait against a publish named by ORDER rather than by
    /// recency -- what a scenario needs when one write retires another and
    /// both obligations must be spoken about in the same breath.
    pub fn receipt_eventually_at(
        &mut self,
        ordinal: usize,
        pred: impl Fn(&[WriteFact]) -> bool,
    ) -> bool {
        let receipt = self
            .receipts
            .get_mut(ordinal)
            .unwrap_or_else(|| panic!("nmp-bdd: receipt {} does not exist", ordinal + 1));
        receipt.eventually(EVENTUALLY, pred)
    }

    /// The negative form: true iff `pred` NEVER becomes true within the
    /// window. Costs its full budget by construction -- there is no early
    /// exit from "this did not happen".
    pub fn receipt_never(&mut self, pred: impl Fn(&[WriteFact]) -> bool) -> bool {
        let receipt = self
            .receipts
            .last_mut()
            .expect("nmp-bdd: no publish is in flight");
        !receipt.eventually(NEVER, pred)
    }

    /// The same, for a publish named by ORDER -- what a scenario that
    /// published twice and compares the two answers has to read.
    pub fn receipt_statuses_at(&mut self, ordinal: usize) -> Vec<WriteFact> {
        let Some(receipt) = self.receipts.get_mut(ordinal) else {
            return Vec::new();
        };
        receipt.eventually(Duration::from_millis(0), |_| true);
        receipt.seen.clone()
    }

    /// The bounded wait against the stream REATTACHED after a restart. On the
    /// far side of a process boundary that is the only stream that exists,
    /// and reading the dead one would report what the previous process
    /// happened to have said.
    pub fn restarted_receipt_eventually(&mut self, pred: impl Fn(&[WriteFact]) -> bool) -> bool {
        let receipt = self
            .restarted_receipt
            .as_mut()
            .expect("nmp-bdd: no receipt was reattached after a restart");
        receipt.eventually(EVENTUALLY, pred)
    }

    /// Everything the reattached stream has replayed so far.
    pub fn restarted_receipt_statuses(&mut self) -> Vec<WriteFact> {
        let Some(receipt) = self.restarted_receipt.as_mut() else {
            return Vec::new();
        };
        receipt.eventually(Duration::from_millis(0), |_| true);
        receipt.seen.clone()
    }

    /// Everything the last publish's receipt has reported so far -- for
    /// assertion MESSAGES and for order-sensitive checks ("Failed was
    /// first"), never as a substitute for a bounded wait.
    pub fn receipt_statuses(&mut self) -> Vec<WriteFact> {
        let Some(receipt) = self.receipts.last_mut() else {
            return Vec::new();
        };
        receipt.eventually(Duration::from_millis(0), |_| true);
        receipt.seen.clone()
    }

    /// How many publishes are outstanding. One publish is one obligation and
    /// one receipt stream, so anything other than 1 means a scenario
    /// published a second time.
    pub fn receipt_count(&self) -> usize {
        self.receipts.len()
    }

    /// Was the last publish TAKEN? `publish()` returning `Ok` is acceptance:
    /// the write is durably recorded and whatever becomes of it is recorded
    /// with it. There is no acceptance fact on the stream to wait for, so
    /// this is an immediate read rather than a bounded one.
    pub fn publish_was_accepted(&self) -> bool {
        self.receipts
            .last()
            .is_some_and(|receipt| receipt.refusal.is_none())
    }

    /// The id the publish door answered with, which is what every later fact
    /// correlates to and the only handle a restarted app has. `None` when the
    /// door refused: nothing was taken, so nothing was identified.
    pub fn last_receipt_id(&self) -> Option<nmp_engine::core::ReceiptId> {
        self.last_receipt_id
    }

    /// Why the door refused to take the last publish, if it did. The two
    /// refusal classes are the whole of what `publish()` says no to; anything
    /// else took custody and fails in the queue where the app can see it.
    pub fn publish_refusal(&self) -> Option<String> {
        self.receipts
            .last()
            .and_then(|receipt| receipt.refusal.as_ref())
            .map(ToString::to_string)
    }

    /// Bounded wait for the last publish to END. Exactly one `Outcome` closes
    /// every receipt stream, so a scenario that used to wait for the stream
    /// to go quiet now waits for this.
    pub fn receipt_settled(&mut self) -> bool {
        self.receipt_eventually(|seen| seen.iter().any(is_outcome_fact))
    }

    /// True when the last publish carried `WriteRouting::Auto` -- i.e. the
    /// app named no relay and NMP derived the route itself.
    pub fn last_publish_named_no_relay(&self) -> bool {
        self.last_publish_was_auto
    }

    /// The exact event a person's staged already-signed note carries.
    pub fn staged_signed_event_of(&mut self, person: &str) -> Option<nostr::Event> {
        let pk = self.person(person).public_key();
        self.signed_notes
            .values()
            .find(|event| event.pubkey == pk)
            .cloned()
    }

    /// The event a republish step handed to the engine, for the `Then` that
    /// checks the id and signature came back out untouched.
    pub fn republished_event(&self) -> Option<&nostr::Event> {
        self.republished.as_ref()
    }

    /// The one note staged as already-signed, so a republish step can name
    /// its author without repeating its text.
    pub fn only_staged_signed_note_text(&self) -> Option<String> {
        match self.signed_notes.len() {
            1 => self.signed_notes.keys().next().cloned(),
            _ => None,
        }
    }

    /// How many times a registered signer was actually asked to sign. Zero
    /// is the fact behind "no signer was asked for anything" -- and it is a
    /// real fact rather than a vacuous one, because every scenario that logs
    /// in registers a signer that would have counted.
    pub fn signer_ask_count(&self) -> usize {
        self.signer_asked.load(Ordering::SeqCst)
    }

    pub fn diagnostics_matching(
        &self,
        pred: impl Fn(&DiagnosticsSnapshot) -> bool,
    ) -> Option<DiagnosticsSnapshot> {
        let diag = self.diag.as_ref().expect("nmp-bdd: diagnostics not open");
        diag.get(EVENTUALLY, pred)
    }
}
