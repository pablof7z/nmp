//! What the receipt said about WHERE a write goes -- and what it said when the
//! answer was nothing.
//!
//! Split from [`super::outbox`], which STAGES the world an outbox derivation
//! reads, because this is the other end of the same scenario: the derivation's
//! inputs and its answer are separate concerns, and a reader chasing "why did
//! this route come out wrong" wants one or the other, never both at once.
//!
//! Almost everything here reads the receipt's own `WriteFact::Destinations`
//! -- the intended relay set and whether resolution can still change its
//! mind -- and its terminal `WriteOutcome::NoDestination`. The two exceptions
//! are named where they appear: a COUNT of what a relay actually admitted,
//! and the engine's PLANNED sessions -- both needed because a relay nobody
//! staged has no name a contact log could be asked about, and "offered
//! exactly once" is not a claim any receipt makes.
//!
//! One read here is deliberately a wait rather than a read, and it was a bug
//! first: a receipt beat and a socket write are not the same instant, so
//! counting a relay's copies the moment routing named it says only that
//! nothing has arrived YET.

use std::collections::BTreeSet;

use nostr::{PublicKey, Timestamp};

use nmp_router::RelayUrl;

use nmp_engine::core::{DiagnosticsSnapshot, StalledWriteStage};
use nmp_engine::publish_queue::{RelayState, WriteFact, WriteOutcome};

use super::budgets::EVENTUALLY;
use super::observe::is_failure_fact;
use super::NmpWorld;

/// The receipt's own spelling of a write parked with nowhere to go yet: the
/// destination set is empty and resolution has NOT finished, so nothing
/// expires it and a later fact can still name a relay.
fn parked_open(fact: &WriteFact) -> bool {
    matches!(
        fact,
        WriteFact::Destinations {
            relays,
            complete: false,
            ..
        } if relays.is_empty()
    )
}

/// WHO an open destination picture is still waiting on, unioned over every
/// such fact the receipt has reported.
///
/// This is the park's REASON, and it is a set of keys rather than a sentence
/// on purpose: a step that could only match prose would pass on any prose
/// (#1236). Unioned across facts because a park re-reports as the answer
/// narrows, and a scenario asking "does it say who" is asking about the whole
/// stream, not about whichever beat arrived last.
fn awaited_authors(seen: &[WriteFact]) -> BTreeSet<PublicKey> {
    seen.iter()
        .filter_map(|fact| match fact {
            WriteFact::Destinations {
                complete: false,
                awaiting_author_routes,
                ..
            } => Some(awaiting_author_routes.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect()
}

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
            |seen| matches!(seen.iter().rev().find(|s| matches!(s, WriteFact::Destinations { .. })), Some(WriteFact::Destinations { relays, .. }) if *relays == wanted),
        )
    }

    /// `Then the <thing> is routed to <relay>` -- a member of the answer,
    /// which is the right claim when a scenario names one source of several.
    pub fn routed_to(&mut self, name: &str) -> bool {
        let url = self.relay_url(name);
        self.receipt_eventually(|seen| {
            seen.iter().any(
                |s| matches!(s, WriteFact::Destinations { relays, .. } if relays.contains(&url)),
            )
        })
    }

    /// `Then the note is never routed to <relay>` -- costs its full negative
    /// budget, and is a claim about the ROUTE rather than about contact: a
    /// relay may be contacted for a read and still never be a destination.
    pub fn never_routed_to(&mut self, name: &str) -> bool {
        let url = self.relay_url(name);
        self.receipt_never(|seen| {
            seen.iter().any(
                |s| matches!(s, WriteFact::Destinations { relays, .. } if relays.contains(&url)),
            )
        })
    }

    /// `Then routing is complete` -- zero unknowns remain, so the answer can
    /// never change again.
    pub fn routing_is_complete(&mut self) -> bool {
        self.receipt_eventually(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteFact::Destinations { complete: true, .. }))
        })
    }

    /// `Then routing is not complete`.
    pub fn routing_stays_open(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteFact::Destinations { complete: true, .. }))
        })
    }

    /// `Then the note is routed to no relay` -- no destination was ever
    /// named, which is different from one being named and never delivered to.
    pub fn routed_nowhere(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter()
                .any(|s| matches!(s, WriteFact::Destinations { relays, .. } if !relays.is_empty()))
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
                .any(|s| matches!(s, WriteFact::Destinations { complete: true, .. }))
        });
        self.receipt_statuses_at(ordinal)
            .iter()
            .rev()
            .find_map(|s| match s {
                WriteFact::Destinations { relays, .. } => Some(relays.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    // ---- Then: the refusal ----------------------------------------------

    /// Bounded wait for the write to be PARKED with nowhere to go: an empty
    /// destination set that resolution has not closed. Reads the REATTACHED
    /// stream after a restart, because on the far side of a process boundary
    /// that is the only stream that exists.
    pub fn parked_without_destination(&mut self) -> bool {
        let matches = |seen: &[WriteFact]| seen.iter().any(parked_open);
        if self.restarted_receipt.is_some() {
            return self.restarted_receipt_eventually(matches);
        }
        self.receipt_eventually(matches)
    }

    /// Bounded wait for the write to END with nowhere to publish: resolution
    /// finished -- knowledge is exhausted -- and named zero relays.
    ///
    /// This is the terminal, and it is what separates "there is nothing to
    /// work with" from "we have not learned where this goes yet"; the latter
    /// parks forever and is [`Self::parked_without_destination`]. Reads the
    /// reattached stream after a restart for the same reason as above.
    pub fn no_destination_settled(&mut self) -> bool {
        let matches = |seen: &[WriteFact]| {
            seen.iter()
                .any(|s| matches!(s, WriteFact::Outcome(WriteOutcome::NoDestination)))
        };
        if self.restarted_receipt.is_some() {
            return self.restarted_receipt_eventually(matches);
        }
        self.receipt_eventually(matches)
    }

    /// Bounded wait for the park to NAME somebody: an open destination
    /// picture whose waiting set is non-empty.
    ///
    /// The claim behind `the receipt says why it is still determining
    /// destinations`. It is a strictly stronger claim than
    /// [`Self::parked_without_destination`] -- a park with an empty waiting
    /// set satisfies that one and fails this one, which is the difference
    /// between proving a park exists and proving it says anything.
    pub fn park_names_an_author(&mut self) -> bool {
        let matches = |seen: &[WriteFact]| !awaited_authors(seen).is_empty();
        if self.restarted_receipt.is_some() {
            return self.restarted_receipt_eventually(matches);
        }
        self.receipt_eventually(matches)
    }

    /// Bounded wait for the park to name ONE specific author -- the form a
    /// scenario uses when it staged exactly whose relay list is missing.
    pub fn park_awaits(&mut self, author: PublicKey) -> bool {
        let matches = move |seen: &[WriteFact]| awaited_authors(seen).contains(&author);
        if self.restarted_receipt.is_some() {
            return self.restarted_receipt_eventually(matches);
        }
        self.receipt_eventually(matches)
    }

    /// Every fact the write under discussion has reported, from whichever
    /// stream is the live one -- for assertion MESSAGES, never as a
    /// substitute for a bounded wait.
    pub fn routing_facts_reported(&mut self) -> Vec<WriteFact> {
        if self.restarted_receipt.is_some() {
            return self.restarted_receipt_statuses();
        }
        self.receipt_statuses()
    }

    /// `Then the publish reports no routing problem` -- the negative form,
    /// costing its own budget: nothing ever parked this write.
    pub fn never_parked(&mut self) -> bool {
        self.receipt_never(|seen| seen.iter().any(parked_open))
    }

    /// `Then the note is never reported as sent`.
    ///
    /// Both halves of "sent" are covered: transport proving a socket write
    /// (`Sent`) and the relay itself acking (`Published`). The old
    /// `HandoffAmbiguous` third arm went with `AtMostOnce` and has no
    /// successor -- there is no longer a state in which NMP does not know
    /// whether a frame left.
    pub fn never_sent(&mut self) -> bool {
        self.receipt_never(|seen| {
            seen.iter().any(|s| {
                matches!(
                    s,
                    WriteFact::Relay {
                        state: RelayState::Sent { .. } | RelayState::Published,
                        ..
                    }
                )
            })
        })
    }

    /// `Then the publish has not failed` -- and, since a refusal at the door
    /// is now the OTHER way a publish fails, the door must have taken it too.
    pub fn never_failed(&mut self) -> bool {
        self.publish_was_accepted() && self.receipt_never(|seen| seen.iter().any(is_failure_fact))
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
