//! The world-side CONTACT WITNESS: what actually reached a relay's socket,
//! and what changed since a marked moment.
//!
//! Deliberately apart from [`super::observe`], which folds the three channels
//! the ENGINE reports on (deltas, receipts, diagnostics). Everything here is
//! read from the scripted relay's own log instead, and that independence is
//! the point: a `must-never` claim like "no relay outside the plan was ever
//! contacted" must not rest solely on the thing under test, or a diagnostics
//! bug could quietly make the claim un-falsifiable. Two witnesses, two
//! modules.
//!
//! The snapshot half exists because "untouched" is a claim about an INTERVAL,
//! not a state: a relay serving somebody else's subscription is contacted all
//! the time, and what a scenario means is that nothing NEW reached it after
//! the moment it names.

use std::collections::BTreeMap;

use nmp_test_support::relays::ScriptedRelay;

use super::NmpWorld;

impl NmpWorld {
    /// Was ANY relay in this world ever contacted? The precondition behind
    /// every "was never contacted" assertion: in an empty world nothing is
    /// contacted, and that must not read as proof.
    pub fn any_relay_contacted(&self) -> bool {
        self.relays.values().any(ScriptedRelay::contacted)
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
}
