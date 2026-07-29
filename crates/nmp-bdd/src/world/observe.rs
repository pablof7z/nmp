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
//! The trailing read accessors (`relay_url`, `pubkey_hex`, ...) are here for
//! the same reason: they are the non-waiting half of the same read surface --
//! plain facts a `Then` step needs to phrase an assertion against a name a
//! scenario used.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nostr::EventId;

use nmp::mechanism::core::{AcquisitionEvidence, DiagnosticsSnapshot, RowDelta, ShortfallFact};
use nmp::mechanism::outbox::WriteStatus;
use nmp::mechanism::runtime::{DiagnosticsHandle, QueryHandle, RowsReceiver};
use nmp_router::RelayUrl;
use nmp_test_support::relays::ScriptedRelay;

use super::budgets::{EVENTUALLY, NEVER};
use super::NmpWorld;

/// One accumulated feed: folds every `Added`/`Removed` delta this channel
/// has delivered so far into a live row set + the query's latest acquisition
/// evidence -- exactly what a real app must do (`Handle::subscribe`'s wire is
/// deltas, never snapshots). Persists across multiple `Then` steps in one
/// scenario.
pub(super) struct FeedState {
    pub(super) handle: QueryHandle,
    pub(super) rx: RowsReceiver,
    pub(super) rows: BTreeMap<EventId, nostr::Event>,
    pub(super) evidence: AcquisitionEvidence,
}

impl FeedState {
    /// A fresh, empty accumulator over `handle`'s delta stream.
    pub(super) fn new(handle: QueryHandle, rx: RowsReceiver) -> Self {
        Self {
            handle,
            rx,
            rows: BTreeMap::new(),
            evidence: AcquisitionEvidence::default(),
        }
    }

    pub(super) fn drain_available(&mut self) {
        while let Ok((deltas, evidence, _execution)) = self.rx.try_recv() {
            self.apply(deltas, evidence);
        }
    }

    fn apply(&mut self, deltas: Vec<RowDelta>, evidence: AcquisitionEvidence) {
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    self.rows.insert(row.event.id, row.event);
                }
                // #105: no scenario in this catalog asserts on relay
                // provenance yet -- the row's event/membership is unchanged,
                // so there is nothing for this world to update.
                RowDelta::SourcesGrew { .. } => {}
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
    fn eventually(&mut self, timeout: Duration, pred: impl Fn(&Self) -> bool) -> bool {
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

/// The receipt stream for the most recent `publish` (the starter catalog
/// only ever names "the receipt", singular -- one implicit publish in
/// flight per scenario).
pub(super) struct ReceiptState {
    pub(super) rx: nmp::mechanism::runtime::FifoReceiver<WriteStatus>,
    pub(super) seen: Vec<WriteStatus>,
}

impl ReceiptState {
    /// A fresh accumulator over one publish's status stream.
    pub(super) fn new(rx: nmp::mechanism::runtime::FifoReceiver<WriteStatus>) -> Self {
        Self {
            rx,
            seen: Vec::new(),
        }
    }

    fn drain_available(&mut self) {
        while let Ok(status) = self.rx.try_recv() {
            self.seen.push(status);
        }
    }

    fn eventually(&mut self, timeout: Duration, pred: impl Fn(&[WriteStatus]) -> bool) -> bool {
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
        pred: impl Fn(&[WriteStatus]) -> bool,
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
        rx: nmp::mechanism::runtime::LatestReceiver<DiagnosticsSnapshot>,
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
        pred: impl Fn(&[nostr::Event], &AcquisitionEvidence) -> bool,
    ) -> bool {
        let feed = self.feed.as_mut().expect("nmp-bdd: no feed is open");
        feed.eventually(EVENTUALLY, |f| {
            let rows: Vec<nostr::Event> = f.rows.values().cloned().collect();
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
                if watch
                    .evidence
                    .shortfall
                    .iter()
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

    pub fn feed_never(&mut self, pred: impl Fn(&[nostr::Event]) -> bool) -> bool {
        let feed = self.feed.as_mut().expect("nmp-bdd: no feed is open");
        feed.never(NEVER, |f| {
            let rows: Vec<nostr::Event> = f.rows.values().cloned().collect();
            pred(&rows)
        })
    }

    pub fn receipt_eventually(&mut self, pred: impl Fn(&[WriteStatus]) -> bool) -> bool {
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
        pred: impl Fn(&[WriteStatus]) -> bool,
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
    pub fn receipt_never(&mut self, pred: impl Fn(&[WriteStatus]) -> bool) -> bool {
        let receipt = self
            .receipts
            .last_mut()
            .expect("nmp-bdd: no publish is in flight");
        !receipt.eventually(NEVER, pred)
    }

    /// The same, for a publish named by ORDER -- what a scenario that
    /// published twice and compares the two answers has to read.
    pub fn receipt_statuses_at(&mut self, ordinal: usize) -> Vec<WriteStatus> {
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
    pub fn restarted_receipt_eventually(&mut self, pred: impl Fn(&[WriteStatus]) -> bool) -> bool {
        let receipt = self
            .restarted_receipt
            .as_mut()
            .expect("nmp-bdd: no receipt was reattached after a restart");
        receipt.eventually(EVENTUALLY, pred)
    }

    /// Everything the reattached stream has replayed so far.
    pub fn restarted_receipt_statuses(&mut self) -> Vec<WriteStatus> {
        let Some(receipt) = self.restarted_receipt.as_mut() else {
            return Vec::new();
        };
        receipt.eventually(Duration::from_millis(0), |_| true);
        receipt.seen.clone()
    }

    /// Everything the last publish's receipt has reported so far -- for
    /// assertion MESSAGES and for order-sensitive checks ("Failed was
    /// first"), never as a substitute for a bounded wait.
    pub fn receipt_statuses(&mut self) -> Vec<WriteStatus> {
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

    /// True when the last publish carried `WriteRouting::Auto` -- i.e. the
    /// app named no relay and NMP derived the route itself.
    pub fn last_publish_named_no_relay(&self) -> bool {
        self.last_publish_was_auto
    }

    /// Was ANY relay in this world ever contacted? The precondition behind
    /// every "was never contacted" assertion: in an empty world nothing is
    /// contacted, and that must not read as proof.
    pub fn any_relay_contacted(&self) -> bool {
        self.relays.values().any(ScriptedRelay::contacted)
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

    /// True iff the named relay's write/query policy has ever been invoked
    /// -- the world-side "was this relay ever contacted" observable
    /// (independent of the engine's own diagnostics self-report; see
    /// `relays.rs`'s doc).
    pub fn relay_contacted(&self, name: &str) -> bool {
        self.relays
            .get(name)
            .map(ScriptedRelay::contacted)
            .unwrap_or(false)
    }

    /// Record the CURRENT contact-count of every known relay -- the
    /// "before" half of an "untouched since this point" assertion.
    pub(super) fn snapshot_relay_contacts(&mut self) {
        self.contact_snapshot = self
            .relays
            .iter()
            .map(|(name, r)| (name.clone(), r.contact_count()))
            .collect();
        self.wire_snapshot = self
            .relays
            .iter()
            .map(|(name, r)| {
                let record = r.wire_record();
                (name.clone(), (record.reqs.len(), record.closes.len()))
            })
            .collect();
        self.connection_snapshot = self
            .relays
            .iter()
            .map(|(name, r)| (name.clone(), r.connection_count()))
            .collect();
        self.admitted_snapshot = self
            .relays
            .iter()
            .map(|(name, r)| (name.clone(), r.admitted_event_count()))
            .collect();
    }

    /// The two numbers [`Self::relay_untouched_since_snapshot`] compares.
    pub fn contact_counts_since_snapshot(&self, name: &str) -> (u64, u64) {
        let before = self.contact_snapshot.get(name).copied().unwrap_or(0);
        let after = self
            .relays
            .get(name)
            .map(ScriptedRelay::contact_count)
            .unwrap_or(0);
        (before, after)
    }

    /// Every REQ/CLOSE `name` received AFTER the last contact snapshot,
    /// rendered for a failure message: which subscription id, whether it
    /// replaced a live one, and the filters verbatim.
    ///
    /// Plus the two things a bare contact count cannot distinguish: how many
    /// times a client CONNECTED (a reconnect replays the whole live req list
    /// -- `apply_replay`), and which EVENT kinds were admitted (the contact
    /// log counts REQ and EVENT alike). A move with no new frame, no new
    /// connection and no new EVENT is the third case: the write/query policy
    /// hook for traffic the tap already saw ran after the snapshot did.
    pub fn touch_report_since_snapshot(&self, name: &str) -> String {
        let (req_base, close_base) = self.wire_snapshot.get(name).copied().unwrap_or((0, 0));
        let conn_before = self.connection_snapshot.get(name).copied().unwrap_or(0);
        let conn_after = self
            .relays
            .get(name)
            .map(ScriptedRelay::connection_count)
            .unwrap_or(0);
        let record = self.wire_record(name);
        // Walk the WHOLE record so a frame after the snapshot can be told
        // apart from the one it repeats: byte-identical filters under the
        // same subscription id is the known redundant-REQ gap, a different
        // filter is a genuine re-scoping of that subscription.
        let mut latest = BTreeMap::new();
        let mut new_reqs: Vec<String> = Vec::new();
        for (i, r) in record.reqs.iter().enumerate() {
            let previous = latest.insert(r.sub_id.as_str(), &r.filters);
            if i < req_base {
                continue;
            }
            let filters: Vec<String> = r.filters.iter().map(ToString::to_string).collect();
            new_reqs.push(format!(
                "REQ {id:?} (replaces_live={replaces}, byte_identical_repeat={same}) {filters}",
                id = r.sub_id,
                replaces = r.replaces,
                same = previous == Some(&r.filters),
                filters = filters.join(" ")
            ));
        }
        let new_closes: Vec<String> = record
            .closes
            .iter()
            .skip(close_base)
            .map(|id| format!("CLOSE {id:?}"))
            .collect();
        let frames = if new_reqs.is_empty() && new_closes.is_empty() {
            "no new REQ/CLOSE".to_string()
        } else {
            new_reqs
                .into_iter()
                .chain(new_closes)
                .collect::<Vec<_>>()
                .join("; ")
        };
        let admitted_base = self.admitted_snapshot.get(name).copied().unwrap_or(0);
        let new_events: Vec<u16> = self
            .relays
            .get(name)
            .map(ScriptedRelay::admitted_event_kinds)
            .unwrap_or_default()
            .into_iter()
            .skip(admitted_base)
            .collect();
        format!(
            "connections {conn_before} -> {conn_after}; frames: {frames}; \
             EVENT kinds admitted since: {new_events:?}"
        )
    }

    /// True iff `name`'s contact-count is EXACTLY what it was at the last
    /// [`Self::snapshot_relay_contacts`] call -- i.e. no NEW REQ/EVENT ever
    /// reached it since then (the "untouched" `Then`).
    pub fn relay_untouched_since_snapshot(&self, name: &str) -> bool {
        let before = self.contact_snapshot.get(name).copied().unwrap_or(0);
        let after = self
            .relays
            .get(name)
            .map(ScriptedRelay::contact_count)
            .unwrap_or(0);
        before == after
    }

    // ---- plain facts about the staged world ----------------------------

    pub fn indexer_names(&self) -> &[String] {
        &self.indexer_names
    }

    pub fn relay_names(&self) -> impl Iterator<Item = &String> {
        self.relay_order.iter()
    }

    pub fn relay_url(&self, name: &str) -> RelayUrl {
        self.relays
            .get(name)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown relay {name:?}"))
            .url
            .clone()
    }

    pub fn write_relay_of(&self, person: &str) -> Vec<String> {
        self.write_relay_of.get(person).cloned().unwrap_or_default()
    }

    pub fn pubkey_hex(&self, person: &str) -> String {
        self.people
            .get(person)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown person {person:?}"))
            .public_key()
            .to_hex()
    }
}
