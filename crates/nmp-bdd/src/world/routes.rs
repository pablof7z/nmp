//! What the receipt said about WHERE a write goes -- and what it said when the
//! answer was nothing.
//!
//! Split from [`super::outbox`], which STAGES the world an outbox derivation
//! reads, because this is the other end of the same scenario: the derivation's
//! inputs and its answer are separate concerns, and a reader chasing "why did
//! this route come out wrong" wants one or the other, never both at once.
//!
//! Almost everything here reads the receipt's own `WriteStatus::Routed` /
//! `AwaitingRoute`. The two exceptions are named where they appear: a COUNT of
//! what a relay actually admitted, and the engine's PLANNED sessions -- both
//! needed because a relay nobody staged has no name a contact log could be
//! asked about, and "offered exactly once" is not a claim any receipt makes.
//!
//! Two reads here are deliberately waits rather than reads, and both were bugs
//! first. A park's REASON converges -- a write is parked on "nobody has looked
//! yet" before it is parked on "there is nothing to find" -- so reading the
//! first reason to arrive asserts the opposite of what a settled-absence
//! scenario says. And a receipt beat and a socket write are not the same
//! instant, so counting a relay's copies the moment routing named it says only
//! that nothing has arrived YET.

use std::collections::BTreeSet;

use nostr::Timestamp;

use nmp_router::RelayUrl;

use nmp::mechanism::core::{DiagnosticsSnapshot, StalledWriteStage};
use nmp::mechanism::publish_queue::WriteStatus;

use super::budgets::EVENTUALLY;
use super::NmpWorld;

impl NmpWorld {
    // ---- Then: where the write was routed -------------------------------

    /// The relay set the receipt reported, resolved from scenario names.
    fn urls_of(&self, names: &[String]) -> BTreeSet<RelayUrl> {
        names.iter().map(|name| self.relay_url(name)).collect()
    }

    /// `Then the <thing> is routed to exactly <relays>` -- the WHOLE answer,
    /// read off the receipt's own routing picture.
    pub fn routed_exactly(&mut self, names: &[String]) -> bool {
        let wanted = self.urls_of(names);
        self.receipt_eventually(
            |seen| matches!(seen.iter().rev().find(|s| matches!(s, WriteStatus::Routed { .. })), Some(WriteStatus::Routed { relays, .. }) if *relays == wanted),
        )
    }

    /// `Then the <thing> is routed to <relay>` -- a member of the answer,
    /// which is the right claim when a scenario names one source of several.
    pub fn routed_to(&mut self, name: &str) -> bool {
        let url = self.relay_url(name);
        self.receipt_eventually(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { relays, .. } if relays.contains(&url)))
        })
    }

    /// `Then the note is never routed to <relay>` -- costs its full negative
    /// budget, and is a claim about the ROUTE rather than about contact: a
    /// relay may be contacted for a read and still never be a destination.
    pub fn never_routed_to(&mut self, name: &str) -> bool {
        let url = self.relay_url(name);
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { relays, .. } if relays.contains(&url)))
        })
    }

    /// `Then routing is complete` -- zero unknowns remain, so the answer can
    /// never change again.
    pub fn routing_is_complete(&mut self) -> bool {
        self.receipt_eventually(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { complete: true, .. }))
        })
    }

    /// `Then routing is not complete`.
    pub fn routing_stays_open(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { complete: true, .. }))
        })
    }

    /// `Then the note is routed to no relay` -- no destination was ever
    /// named, which is different from one being named and never delivered to.
    pub fn routed_nowhere(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { relays, .. } if !relays.is_empty()))
        })
    }

    /// The FINAL route of a publish named by order -- what "the profile and
    /// the note are routed to the same relays" compares.
    pub fn final_route_at(&mut self, ordinal: usize) -> BTreeSet<RelayUrl> {
        // Wait for a route to exist at all before reading it, or an empty
        // answer would compare equal to another empty answer and prove
        // nothing.
        let _ = self.receipt_eventually_at(ordinal, |seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::Routed { complete: true, .. }))
        });
        self.receipt_statuses_at(ordinal)
            .iter()
            .rev()
            .find_map(|s| match s {
                WriteStatus::Routed { relays, .. } => Some(relays.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    // ---- Then: the refusal ----------------------------------------------

    /// Bounded wait for a park whose reason contains `needle`.
    ///
    /// A wait rather than a read, because a park's REASON converges: the same
    /// write is first parked on "nobody has looked yet" and later, once every
    /// source has finished, on "there is nothing to find". Both are true in
    /// turn and the second is the one a scenario about settled absence means,
    /// so reading the first reason to arrive would assert the opposite of
    /// what the scenario says.
    pub fn park_reason_contains(&mut self, needle: &str) -> bool {
        let owned = needle.to_string();
        let matches = move |seen: &[WriteStatus]| {
            seen.iter()
                .filter_map(park_reason)
                .any(|reason| reason.contains(&owned))
        };
        if self.restarted_receipt.is_some() {
            return self.restarted_receipt_eventually(matches);
        }
        self.receipt_eventually(matches)
    }

    /// Every park reason this write has reported so far, newest last -- for
    /// assertion MESSAGES and for the diagnostics cross-check, never as a
    /// substitute for the bounded wait above. Reads the REATTACHED stream
    /// after a restart, because on the far side of a process boundary that is
    /// the only stream that exists.
    pub fn park_reasons(&mut self) -> Vec<String> {
        let reasoned = |seen: &[WriteStatus]| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::AwaitingRoute { .. }))
        };
        if self.restarted_receipt.is_some() {
            let _ = self.restarted_receipt_eventually(reasoned);
            return self
                .restarted_receipt_statuses()
                .iter()
                .filter_map(park_reason)
                .collect();
        }
        let _ = self.receipt_eventually(reasoned);
        self.receipt_statuses()
            .iter()
            .filter_map(park_reason)
            .collect()
    }

    /// `Then the publish reports no routing problem` -- the negative form,
    /// costing its own budget: nothing ever parked this write.
    pub fn never_parked(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteStatus::AwaitingRoute { .. }))
        })
    }

    /// `Then the note is never reported as sent`.
    pub fn never_sent(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter().any(|s| {
                matches!(
                    s,
                    WriteStatus::Sent { .. }
                        | WriteStatus::Acked(_)
                        | WriteStatus::HandoffAmbiguous { .. }
                )
            })
        })
    }

    /// `Then the publish has not failed`.
    pub fn never_failed(&mut self) -> bool {
        self.receipt_never(|seen| seen.iter().any(|s| matches!(s, WriteStatus::Failed(_))))
    }

    // ---- Then: what reached a socket ------------------------------------

    /// How many copies of `event` the named relay actually admitted -- the
    /// wire-side count behind "offered exactly once".
    pub fn copies_admitted(&self, relay: &str, event: nostr::EventId) -> usize {
        self.relays
            .get(relay)
            .map(|r| {
                r.admitted_events()
                    .into_iter()
                    .filter(|e| e.id == event)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Wait (bounded) for `relay` to admit `event` at all.
    ///
    /// The precondition of every count: a receipt beat and a socket write are
    /// not the same instant, so reading the relay's log the moment routing
    /// reported a destination says only that nothing has arrived YET. Waiting
    /// is also strictly safer for the count that follows -- it gives a second
    /// copy more time to show up, never less.
    pub async fn wait_for_copy(&self, relay: &str, event: nostr::EventId) -> bool {
        let deadline = std::time::Instant::now() + EVENTUALLY;
        loop {
            if self.copies_admitted(relay, event) > 0 {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Wait (bounded) for ANY relay in this world to be contacted -- the
    /// precondition of every "nothing outside X was contacted" claim, which
    /// on an empty world is true and worthless.
    pub async fn wait_any_relay_contacted(&self) -> bool {
        let deadline = std::time::Instant::now() + EVENTUALLY;
        loop {
            if self.any_relay_contacted() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Every relay this world ever admitted the given event at.
    pub fn relays_holding(&self, event: nostr::EventId) -> Vec<String> {
        self.relay_order
            .iter()
            .filter(|name| self.copies_admitted(name, event) > 0)
            .cloned()
            .collect()
    }

    /// Every relay the ENGINE planned a session to, by scenario name where
    /// this world knows one and by URL where it does not. A URL with no name
    /// is a relay nobody configured -- exactly what "no relay outside the
    /// ones configured" is looking for.
    pub fn planned_relays(&mut self) -> Vec<String> {
        let known: Vec<(RelayUrl, String)> = self
            .relay_order
            .iter()
            .filter(|name| self.relays.contains_key(*name))
            .map(|name| (self.relay_url(name), name.clone()))
            .collect();
        let Some(snapshot) = self.diagnostics_matching(|snap| !snap.relays.is_empty()) else {
            return Vec::new();
        };
        snapshot
            .relays
            .iter()
            .map(|row| {
                known
                    .iter()
                    .find(|(url, _)| *url == row.relay)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| row.relay.to_string())
            })
            .collect()
    }

    /// `Then diagnostics reports the note among the stalled writes` -- the
    /// engine-global answer to "is anything quietly stuck", which no single
    /// receipt can give.
    ///
    /// Narrowed to `Unroutable`, which is the only stage a scenario about the
    /// OUTBOX can produce: the write is signed and its author can sign, so it
    /// is neither unsignable nor undeliverable -- it has nowhere to be
    /// delivered TO. Reading every stage would let this pass on a stall that
    /// had nothing to do with routing.
    ///
    /// A bounded WAIT, and named apart from #1025's own `stalled_writes` for
    /// exactly that reason: that one reads the snapshot a scenario explicitly
    /// captured, which is right when the scenario says `When I read the
    /// diagnostics`. An outbox scenario never says that -- it publishes and
    /// asks -- so it has to wait for the census to move on its own.
    pub fn unroutable_writes(&mut self) -> Vec<(String, Timestamp)> {
        let unroutable = |snap: &DiagnosticsSnapshot| {
            snap.stalled_writes
                .iter()
                .any(|stalled| stalled.stage == StalledWriteStage::Unroutable)
        };
        let Some(snapshot) = self.diagnostics_matching(unroutable) else {
            return Vec::new();
        };
        snapshot
            .stalled_writes
            .iter()
            .filter(|stalled| stalled.stage == StalledWriteStage::Unroutable)
            .map(|stalled| (stalled.detail.clone(), stalled.stalled_since))
            .collect()
    }

    /// The engine's clock as this world last saw it before publishing -- the
    /// lower bound a stalled entry's `stalled_since` must fall above, so
    /// "how long it has been so" is a fact the engine recorded rather than a
    /// number it made up.
    pub fn last_publish_at(&self) -> Timestamp {
        self.last_publish_at
            .expect("nmp-bdd: nothing has been published in this scenario")
    }

    /// A bounded wait for the diagnostics stream to say anything at all --
    /// the precondition behind every claim about what it does NOT contain.
    pub fn diagnostics_ran(&mut self) -> bool {
        self.diagnostics_matching(|_| true).is_some()
    }

    /// Give the world one full observation budget of quiet. Used by the
    /// steps whose claim is about the absence of contact, so the world has
    /// had as long to do the wrong thing as any positive assertion allows.
    pub async fn settle(&mut self) {
        tokio::time::sleep(EVENTUALLY).await;
    }
}

fn park_reason(status: &WriteStatus) -> Option<String> {
    match status {
        WriteStatus::AwaitingRoute { detail } => Some(detail.clone()),
        _ => None,
    }
}
