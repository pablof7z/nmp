//! Durable write, receipt, recovery, and retry lifecycle.
//!
//! This module owns acceptance through signing, route snapshots, per-relay
//! attempts and acknowledgements, cancellation/compensation, and boot recovery.

use super::*;

fn public_retry_cause(cause: PublishQueueTransientCause) -> Option<RetryCause> {
    match cause {
        PublishQueueTransientCause::Interrupted => Some(RetryCause::Interrupted),
        PublishQueueTransientCause::AckTimeout => Some(RetryCause::AckTimeout),
        PublishQueueTransientCause::ConnectionLost => Some(RetryCause::ConnectionLost),
        PublishQueueTransientCause::RelayRateLimited => Some(RetryCause::RelayRateLimited),
        PublishQueueTransientCause::RelayError => Some(RetryCause::RelayError),
        PublishQueueTransientCause::AuthRequired => None,
    }
}

fn public_auth_denial_source(source: StoredAuthDenialSource) -> AuthDenialSource {
    match source {
        StoredAuthDenialSource::Policy => AuthDenialSource::Policy,
        StoredAuthDenialSource::Signer => AuthDenialSource::Signer,
        StoredAuthDenialSource::Relay => AuthDenialSource::Relay,
    }
}

/// The frozen body as a signer sees it. Acceptance decided the author and
/// the timestamp, so this is the first point at which a complete unsigned
/// event exists at all — which is exactly why the payload an app hands in
/// is a builder and not one of these.
fn unsigned_from_frozen(frozen: &SignedEvent) -> UnsignedEvent {
    UnsignedEvent {
        id: Some(frozen.id),
        pubkey: frozen.pubkey,
        created_at: frozen.created_at,
        kind: frozen.kind,
        tags: frozen.tags.clone(),
        content: frozen.content.clone(),
    }
}

/// The one refusal reason for an explicit route naming no relays.
///
/// Emptiness is a property of the REQUEST, knowable at the door, so it is
/// refused there — before an intent id, a receipt id, or a journal row
/// exists. (Reachability is a property of the world and is not knowable at
/// the door: a write aimed at a relay that does not exist is accepted,
/// routed, and fails visibly per relay instead.)
pub(super) const EMPTY_EXPLICIT_ROUTE: &str =
    "explicit routing names no relays: a route with nothing in it is refused at the door, never \
     widened to the author's write relays";

/// The read path's 2-relay minimum, reused verbatim on the write side so
/// `fallback_relays`' trigger means the same thing on both
/// (`docs/internals/routing/outbox.md` §6.2).
const COVERAGE_MIN: usize = 2;

/// Which neutral direction one contributing public key uses.
#[derive(Debug, Clone, Copy)]
enum RouteDirection {
    Outbound,
    Inbound,
}

/// One execution of a routing strategy: what it can reach RIGHT NOW, what it
/// is still missing, and whether it can ever change its mind again.
///
/// `complete` is the retirement flag, and it is a statement about KNOWLEDGE
/// EXHAUSTION, never about delivery: an intent can be fully routed with every
/// lane undelivered, and can (transiently) be delivering on some lanes while
/// its routing is still incomplete
/// (`docs/internals/routing/resolution-lifecycle.md` §7.1). Nothing in this
/// struct is ever serialized — the journal stores the strategy label and the
/// committed relay revisions, never a resolution report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RouteAnswer {
    /// Every relay this execution can name. Diffed against the intent's
    /// durable revision union by the caller, so re-running a resolution that
    /// learned nothing costs an empty diff and mints no lane.
    pub(super) relays: BTreeSet<RelayUrl>,
    /// The public keys whose neutral author-route provider must remain live.
    /// Usually these are `Unknown`; a zero-destination answer also retains
    /// its settled contributors because a later positive replacement is the
    /// only fact that can unpark it. These stateless declared needs are
    /// re-derived every pass and unioned across all parked writes.
    pub(super) author_route_needs: BTreeSet<PublicKey>,
    /// True iff nothing is left to learn, so re-executing is pointless and
    /// the `Auto` retires.
    pub(super) complete: bool,
    /// Why this resolution is not complete, in the words the receipt park
    /// carries. A park nobody can read is indistinguishable from data loss.
    pub(super) detail: Option<String>,
}

/// Every pubkey this signed event `p`-tags, in tag order, deduplicated.
///
/// Read off the SIGNED bytes, so the recipient set a resolution is evaluated
/// against is frozen for the life of the intent — which is the other half of
/// why retirement is reachable at all (the answer cannot be reopened by a tag
/// that was never in the event).
fn p_tagged_authors(event: &SignedEvent) -> BTreeSet<PublicKey> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let slice = tag.as_slice();
            let ("p", Some(value)) = (slice.first()?.as_str(), slice.get(1)) else {
                return None;
            };
            PublicKey::parse(value).ok()
        })
        .collect()
}

/// The park reason for a resolution still holding unknowns. Names WHO is
/// missing, because "stuck because X" is the only thing an app or a person
/// can act on, and a bare "stuck" is barely better than losing the write.
fn unresolved_detail(unknown: &BTreeSet<PublicKey>) -> String {
    let mut names = unknown.iter().map(PublicKey::to_hex).collect::<Vec<_>>();
    names.sort();
    format!("author routes are Unknown for {}", names.join(", "))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReceiptReplayFactKey {
    ReceiptStatus,
    AwaitingCapability,
    /// The routing park, keyed by its own reason: a park whose detail
    /// CHANGES (a second unknown settled, one recipient still missing) is a
    /// new fact worth re-emitting, while the same reason repeated on every
    /// tick is not.
    Destinations,
    Attempt {
        relay: RelayUrl,
        key: ReceiptAttemptReplayKey,
    },
    Lane {
        relay: RelayUrl,
        revision: u64,
    },
    /// A persistence stall, keyed by the relay AND which durable fact failed.
    ///
    /// Both halves are load-bearing. One relay can stall on BOTH its
    /// append-only route revision and its attempt log, and those are two
    /// different facts with two different recovery stories — whether the
    /// resolved URL survives a crash. Keying on the relay alone silently
    /// swallows whichever arrives second under paged reattachment, which is
    /// exactly the class of loss a durable receipt exists to prevent.
    PersistenceStalled(RelayUrl, PersistenceStallKind),
}

/// Which durable fact a persistence stall failed to commit. Not public
/// vocabulary — the app-facing shape stays one `PersistenceStalled { detail }`
/// per #1237 — purely the replay cursor's dedup discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PersistenceStallKind {
    /// The attempt log: recovery still rediscovers this exact relay from its
    /// committed route revision.
    Attempt,
    /// The route revision itself: this exact URL is not claimed to survive.
    Route,
}

impl ReceiptReplayCursor {
    fn contains(&self, key: &ReceiptReplayFactKey, status: &WriteFact) -> bool {
        match key {
            ReceiptReplayFactKey::ReceiptStatus => {
                self.state.receipt_status.as_ref() == Some(status)
            }
            ReceiptReplayFactKey::AwaitingCapability => self.state.awaiting_capability,
            ReceiptReplayFactKey::Destinations => self.state.destinations,
            ReceiptReplayFactKey::Attempt { relay, key } => self
                .state
                .attempts
                .get(relay)
                .is_some_and(|delivered| delivered >= key),
            ReceiptReplayFactKey::Lane { relay, revision } => self
                .state
                .lane_revisions
                .get(relay)
                .is_some_and(|delivered| delivered >= revision),
            ReceiptReplayFactKey::PersistenceStalled(relay, kind) => self
                .state
                .persistence_stalled
                .contains(&(relay.clone(), *kind)),
        }
    }

    fn advance(&mut self, key: ReceiptReplayFactKey, status: WriteFact) {
        match key {
            ReceiptReplayFactKey::ReceiptStatus => self.state.receipt_status = Some(status),
            ReceiptReplayFactKey::AwaitingCapability => self.state.awaiting_capability = true,
            ReceiptReplayFactKey::Destinations => self.state.destinations = true,
            ReceiptReplayFactKey::Attempt { relay, key } => {
                self.state.attempts.insert(relay, key);
            }
            ReceiptReplayFactKey::Lane { relay, revision } => {
                self.state.lane_revisions.insert(relay, revision);
            }
            ReceiptReplayFactKey::PersistenceStalled(relay, kind) => {
                self.state.persistence_stalled.insert((relay, kind));
            }
        }
    }
}

impl<S: EventStore> EngineCore<S> {
    /// Record an ingest/read persistence failure (issue #122) without
    /// panicking: latch the first error message (read-only degrade) and push
    /// a fresh diagnostics snapshot so an observer sees the degraded state
    /// immediately. Idempotent — a later failure keeps the first message.
    pub(super) fn degrade_store(&mut self, err: PersistenceError, effects: &mut Vec<Effect>) {
        if self.store_degraded.is_none() {
            self.store_degraded = Some(err.to_string());
        }
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
    }

    /// Mint the next [`AttemptCorrelation`] (issue #93). Checked, typed
    /// exhaustion: no id is reused or fabricated.
    pub(super) fn alloc_attempt_correlation(
        &mut self,
    ) -> Result<AttemptCorrelation, AttemptCorrelationExhausted> {
        let id = self
            .next_attempt_correlation
            .ok_or(AttemptCorrelationExhausted)?;
        self.next_attempt_correlation = id.checked_add(1);
        Ok(AttemptCorrelation(id))
    }

    /// O(1) via `intent_receipts` (epic #507 finding E5) -- this door used
    /// to be a full `self.pending` linear scan, run once per due deadline in
    /// `consume_due_publish_queue_deadlines`.
    pub(super) fn receipt_for_intent(&self, intent_id: IntentId) -> Option<ReceiptId> {
        self.intent_receipts.get(&intent_id).copied()
    }

    /// Remove a permanently-discarded pending write's entries from the
    /// `intent_receipts` and `receipts_by_lane_relay` indexes (epic #507
    /// finding E5). Call this at every REAL removal from `self.pending` --
    /// never at `fail_and_compensate`'s transient remove-then-reinsert
    /// (`CompensateOutcome::NotFound`/`Err`), which must leave both indexes
    /// untouched because the obligation and its lanes are still live.
    pub(super) fn forget_pending_indexes(&mut self, id: ReceiptId, pending: &PendingWrite) {
        self.intent_receipts.remove(&pending.intent_id);
        // A removed write owns no projection to reconcile, so its bootstrap
        // gap is closed by the removal. Leaving the entry would keep
        // rearming a deadline for a receipt that can never bootstrap again
        // and, for a blind gap, would suppress worker reconciliation forever.
        self.lane_bootstrap_retries.remove(&id);
        for relay in &pending.lane_projection.persisted {
            if let Some(receipts) = self.receipts_by_lane_relay.get_mut(relay) {
                receipts.remove(&id);
                if receipts.is_empty() {
                    self.receipts_by_lane_relay.remove(relay);
                }
            }
        }
    }

    /// Where one open obligation is stuck, if it is stuck at all (#756/#968).
    ///
    /// Read entirely off the reducer state that already owns this intent's
    /// canonical facts — no store read, no second retry ledger, and no
    /// re-derivation of anything the write plane did not already commit. The
    /// three stages are asked in lifecycle order, because a write with no
    /// signature has no route to be missing and a write with no route has no
    /// destination to be unreachable.
    fn stalled_write_stage(&self, pending: &PendingWrite) -> Option<(StalledWriteStage, String)> {
        if pending.event_id.is_none() && !pending.already_signed {
            // A signer request still outstanding is work in progress, not a
            // stall. Only the durable `AwaitingCapability` park -- request
            // answered "no capability", nothing left running -- is stuck, and
            // it names the FROZEN author rather than whoever is active now.
            if pending.sign_request_in_flight {
                return None;
            }
            return Some((
                StalledWriteStage::Unsignable,
                format!(
                    "no signer is registered for {}",
                    pending.signing_pubkey.to_hex()
                ),
            ));
        }

        if pending.durable_routes.is_empty() && pending.route_blocked_relays.is_empty() {
            // Parked with nothing resolved. This is the ONE stall that no
            // clock may ever end (#1136): "we have not learned where this
            // goes" is ignorance, and a deadline over ignorance is a verdict.
            // It is reported so an operator can see it, never so anything
            // can abandon it.
            return Some((
                StalledWriteStage::Unroutable,
                "no destination has been resolved yet".to_string(),
            ));
        }

        // Destinations exist. Stuck iff nothing is in flight and not one of
        // them is a relay this process currently holds a session to -- the
        // `wss://non-existent.example` case, and every ordinary outage.
        if !pending.pending_relays.is_empty() {
            return None;
        }
        let access = AccessContext::Nip42(pending.signing_pubkey);
        let live: BTreeSet<&RelayUrl> = pending
            .lane_projection
            .required_relays()
            .chain(&pending.unstarted_relays)
            .chain(&pending.route_blocked_relays)
            .collect();
        if live.is_empty() {
            return None;
        }
        if live.iter().any(|relay| {
            self.connected_relays
                .contains(&RelaySessionKey::new((*relay).clone(), access))
        }) {
            return None;
        }
        let named = live
            .iter()
            .map(|relay| relay.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Some((
            StalledWriteStage::Undeliverable,
            format!("no destination is reachable: {named}"),
        ))
    }

    /// Which obligations are stalled, and at which stage — the allocation-
    /// light half of [`Self::stalled_write_stage`], used to decide whether a
    /// turn changed anything an observer of this section would notice.
    ///
    /// Deliberately not the detail strings or the descriptors: this runs on
    /// every write-plane turn, and a change detector that formatted a
    /// sentence and hashed two ids per obligation to decide whether to do
    /// nothing would cost more than the snapshot it was avoiding.
    pub(super) fn stalled_write_census(&self) -> Vec<(ReceiptId, StalledWriteStage)> {
        let mut census: Vec<(ReceiptId, StalledWriteStage)> = self
            .pending
            .iter()
            .filter_map(|(id, pending)| {
                self.stalled_write_stage(pending)
                    .map(|(stage, _)| (*id, stage))
            })
            .collect();
        census.sort();
        census
    }

    /// The bounded stalled-write section of [`Self::diagnostics_snapshot`].
    ///
    /// One pass over the reducer's own open obligations produces both the
    /// exact totals and the detail window, so a row outside the window still
    /// counts — a bound on bytes is never allowed to become a lie about how
    /// much is stuck. Ordering is (stage, acceptance instant, descriptor):
    /// a documented display order, independent of map iteration and of
    /// anything the scheduler reads.
    pub(super) fn stalled_write_projection(&self) -> (Vec<StalledWrite>, StalledWriteTotals) {
        let mut totals = StalledWriteTotals {
            detail_limit: u64::try_from(STALLED_WRITE_DETAIL_LIMIT).unwrap_or(u64::MAX),
            ..StalledWriteTotals::default()
        };
        let mut rows = Vec::new();
        for pending in self.pending.values() {
            let Some((stage, detail)) = self.stalled_write_stage(pending) else {
                continue;
            };
            let counter = match stage {
                StalledWriteStage::Unroutable => &mut totals.unroutable,
                StalledWriteStage::Unsignable => &mut totals.unsignable,
                StalledWriteStage::Undeliverable => &mut totals.undeliverable,
            };
            *counter = counter.saturating_add(1);
            let intent_id = pending.intent_id;
            rows.push(StalledWrite {
                id: stalled_write_id(intent_id.0, &pending.frozen.id),
                stage,
                detail,
                stalled_since: pending.accepted_at,
            });
        }
        rows.sort_by(|a, b| {
            a.stage
                .cmp(&b.stage)
                .then(a.stalled_since.cmp(&b.stalled_since))
                .then_with(|| a.id.cmp(&b.id))
        });
        let total = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        rows.truncate(STALLED_WRITE_DETAIL_LIMIT);
        totals.omitted_details =
            total.saturating_sub(u64::try_from(rows.len()).unwrap_or(u64::MAX));
        (rows, totals)
    }

    /// Deliver one fact about a write.
    ///
    /// A persistence stall is LATCHED onto the pending write as well as
    /// emitted. mosaico's `persistence_blockage_remains_visible_after_later_ack`
    /// is the specification: it observes the fault in the stream and records
    /// it in a different field from success, then asserts a later ack does
    /// not erase it. A purely inspectable field would lose a blockage that
    /// arose and resolved before the app looked, so the fault is BOTH — a
    /// fact on the stream and a field on the queue entry that nothing later
    /// clears. An operator must not lose the only signal that the local disk
    /// is failing because a relay acked afterwards.
    pub(super) fn emit_write_fact(
        &mut self,
        id: ReceiptId,
        fact: WriteFact,
        effects: &mut Vec<Effect>,
    ) {
        if let WriteFact::Relay {
            state: RelayState::Waiting(RelayWaiting::PersistenceStalled { detail }),
            ..
        } = &fact
        {
            if let Some(pending) = self.pending.get_mut(&id) {
                if pending.persistence_fault.is_none() {
                    pending.persistence_fault = Some(detail.clone());
                }
            }
        }
        effects.push(Effect::EmitReceipt(id, fact));
    }

    /// One lane attempt ended in a way that PERMITS another try; the ceiling
    /// decides whether one happens (#1031).
    ///
    /// [`EngineConfig::max_publish_attempts`](crate::EngineConfig) counts
    /// OBSERVATIONS, never wall-clock. Spending one takes a completed attempt
    /// ordinal, so a lane that sat a week disconnected, or parked on AUTH,
    /// consumed nothing and is never given up on for having waited — offline
    /// time is not evidence. Once the destination IS known and this many
    /// attempts have each come back failing, further attempts stop being
    /// evidence-gathering and the lane terminalises at `GaveUp`. Per relay:
    /// three relays published and one given up on is a success with a
    /// footnote, not a failed write.
    ///
    /// Returns `false` when the durable fact could not be committed.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn retry_or_give_up(
        &mut self,
        id: Option<ReceiptId>,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        now: Timestamp,
        cause: PublishQueueTransientCause,
        reason: String,
        effects: &mut Vec<Effect>,
    ) -> bool {
        if ordinal >= self.max_publish_attempts {
            if self
                .commit_lane_attempt_finish(
                    key,
                    revision,
                    ordinal,
                    PublishQueueAttemptOutcome::GaveUp,
                    now,
                )
                .is_err()
            {
                return false;
            }
            if let Some(id) = id {
                self.remove_active_lane(id, &key.relay);
                self.emit_write_fact(
                    id,
                    WriteFact::Relay {
                        relay: key.relay.clone(),
                        state: RelayState::GaveUp,
                    },
                    effects,
                );
                self.close_if_all_lanes_terminal(id, effects);
            }
            return true;
        }
        let eligible_at = now + retry_delay_secs(key, ordinal);
        if self
            .commit_lane_transient(
                key,
                revision,
                ordinal,
                eligible_at,
                cause,
                Some(reason.clone()),
            )
            .is_err()
        {
            return false;
        }
        if let Some(id) = id {
            self.remove_active_lane(id, &key.relay);
            self.emit_write_fact(
                id,
                WriteFact::Relay {
                    relay: key.relay.clone(),
                    state: RelayState::Waiting(RelayWaiting::BackingOff {
                        attempt: ordinal,
                        eligible_at,
                        cause: public_retry_cause(cause).expect("AUTH-required has its own class"),
                        detail: Some(reason),
                    }),
                },
                effects,
            );
        }
        true
    }

    pub(super) fn remove_active_lane(&mut self, id: ReceiptId, relay: &RelayUrl) {
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.pending_relays.remove(relay);
            pending.attempt_ordinals.remove(relay);
        }
    }

    /// Close a write whose destination set is closed and whose every lane is
    /// terminal, and SAY SO.
    ///
    /// The settlement fact is the whole point: without it a receipt stream
    /// simply stops, and an app cannot distinguish a finished write from a
    /// dropped subscription. That silence is the original defect this
    /// vocabulary exists to remove.
    pub(super) fn close_if_all_lanes_terminal(&mut self, id: ReceiptId, effects: &mut Vec<Effect>) {
        let Some((intent_id, event_id)) = self
            .pending
            .get(&id)
            .filter(|pending| {
                // Routing that can still change its mind is the reason a
                // parked write is HELD rather than dropped: an intent whose
                // strategy has unknowns left owns no lane at all, so every
                // lane it has is trivially terminal, and closing on that
                // would delete the exact obligation the queue rewriter is
                // waiting to complete. Nothing auto-abandons.
                pending.route_complete
                    && pending.route_blocked_relays.is_empty()
                    && pending.lane_projection.can_close()
            })
            .map(|pending| (pending.intent_id, pending.event_id))
        else {
            return;
        };
        let Ok(CloseIntentOutcome::Closed | CloseIntentOutcome::AlreadyClosed) =
            self.commit_terminal_close(intent_id)
        else {
            return;
        };
        if let Some(pending) = self.pending.remove(&id) {
            self.forget_pending_indexes(id, &pending);
        }
        if let Some(event_id) = event_id {
            if let Some(receipts) = self.event_to_receipts.get_mut(&event_id) {
                receipts.remove(&id);
                if receipts.is_empty() {
                    self.event_to_receipts.remove(&event_id);
                }
            }
        }
        effects.push(Effect::EmitReceipt(
            id,
            WriteFact::Outcome(WriteOutcome::Settled),
        ));
    }

    #[cfg(test)]
    pub(super) fn set_next_attempt_correlation_for_test(&mut self, next: Option<u64>) {
        self.next_attempt_correlation = next;
    }

    /// Consume the one, ever, typed transport handoff for an exact persisted
    /// lane ordinal. The next lane fact commits before any receipt claim or
    /// subsequent wire effect: transport never becomes a second retry owner.
    pub(super) fn on_event_handoff(
        &mut self,
        correlation: AttemptCorrelation,
        result: HandoffResult,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(target) = self.attempt_correlations.remove(&correlation) else {
            return effects;
        };

        let Some((intent_id, ordinal)) = target.lane else {
            return effects;
        };

        let key = PublishQueueLaneKey {
            intent_id,
            relay: target.session.relay.clone(),
        };
        let Ok(Some(lane)) = self
            .resolver
            .store()
            .recover_publish_queue_lanes(intent_id)
            .map(|lanes| lanes.into_iter().find(|lane| lane.key == key))
        else {
            return effects;
        };
        if !matches!(
            lane.state,
            PublishQueueLaneState::InFlight {
                ordinal: current,
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
            } if current == ordinal
        ) {
            return effects;
        }

        let detail = PublishQueueAttemptHandoff {
            at: self.clock,
            result: match result {
                HandoffResult::NotHandedOff => HandoffEvidence::NotHandedOff,
                HandoffResult::Written => HandoffEvidence::Written,
                HandoffResult::Ambiguous => HandoffEvidence::Ambiguous,
            },
        };
        // An ambiguous handoff is not a fact about the write, so it produces
        // none: the lane simply waits for ACK/timeout exactly as a proven
        // write does. Retrying resends the IDENTICAL frozen event — same id,
        // never re-signed — so a relay that did receive it dedupes.
        let next = match result {
            HandoffResult::NotHandedOff => PublishQueuePostHandoffState::WaitingConnection,
            HandoffResult::Written | HandoffResult::Ambiguous => {
                PublishQueuePostHandoffState::AwaitingAck {
                    deadline: self.clock + ACK_TIMEOUT_SECS,
                }
            }
        };
        if self
            .commit_lane_handoff(&key, lane.revision, ordinal, detail, next)
            .is_err()
        {
            return effects;
        }

        match result {
            HandoffResult::Written => {
                self.emit_write_fact(
                    target.receipt,
                    WriteFact::Relay {
                        relay: target.session.relay,
                        state: RelayState::Sent {
                            attempt: ordinal,
                            written_at: self.clock,
                        },
                    },
                    &mut effects,
                );
            }
            HandoffResult::NotHandedOff => {
                self.remove_active_lane(target.receipt, &target.session.relay);
                self.connected_relays.remove(&target.session);
                self.emit_write_fact(
                    target.receipt,
                    WriteFact::Relay {
                        relay: target.session.relay.clone(),
                        state: RelayState::Waiting(RelayWaiting::NotConnected),
                    },
                    &mut effects,
                );
                effects.push(Effect::EnsureWriteRelay(target.session));
            }
            HandoffResult::Ambiguous => {}
        }
        effects.extend(self.schedule_ready(self.clock));
        effects
    }

    /// Full O(pending) re-read of every outstanding write's lanes.
    /// `schedule_ready` still needs the complete durable attempt-ordinal and
    /// cap-accounting state. Worker ownership no longer calls this door:
    /// #985 projects exact nonterminal lane demand in reducer memory.
    /// `wake_relay_lanes` narrows ordinary relay events through
    /// `receipts_by_lane_relay`, except in its degraded fallback.
    pub(super) fn recover_all_lanes(
        &self,
    ) -> Result<Vec<(ReceiptId, PublishQueueLane)>, PersistenceError> {
        let mut lanes = Vec::new();
        for (id, pending) in &self.pending {
            let intent_id = pending.intent_id;
            lanes.extend(
                self.resolver
                    .store()
                    .recover_publish_queue_lanes(intent_id)?
                    .into_iter()
                    .map(|lane| (*id, lane)),
            );
        }
        lanes.sort_by(|(_, left), (_, right)| left.key.cmp(&right.key));
        Ok(lanes)
    }

    /// The only path that allocates durable attempt ordinals. Eligibility is
    /// persisted first; this reducer then applies stable ordering and the
    /// ratified 32-global/1-per-relay caps before committing Started.
    pub(super) fn schedule_ready(&mut self, now: Timestamp) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Ok(lanes) = self.recover_all_lanes() else {
            self.retry_scheduler_blocked = true;
            return effects;
        };

        let mut in_flight_relays = BTreeSet::new();
        let mut in_flight = 0usize;
        let mut eligible = Vec::new();
        for (id, lane) in lanes {
            match lane.state {
                PublishQueueLaneState::InFlight { .. } => {
                    in_flight = in_flight.saturating_add(1);
                    in_flight_relays.insert(lane.key.relay.clone());
                }
                PublishQueueLaneState::Eligible { since } => eligible.push((since, id, lane)),
                _ => {}
            }
        }
        eligible.sort_by(|(at_a, _, lane_a), (at_b, _, lane_b)| {
            at_a.cmp(at_b).then_with(|| lane_a.key.cmp(&lane_b.key))
        });

        for (_, id, lane) in eligible {
            // The write plane's connectivity check is against the lane's
            // identity-scoped authenticated session (#8 U2: a write rides
            // `Nip42(signing pubkey)`, never the relay's Public read
            // session). A lane whose receipt has no live pending entry has
            // nothing to schedule.
            let Some(pending) = self.pending.get(&id) else {
                continue;
            };
            let session = RelaySessionKey::new(
                lane.key.relay.clone(),
                AccessContext::Nip42(pending.signing_pubkey),
            );
            if !self.connected_relays.contains(&session) {
                if self
                    .commit_lane_waiting(&lane.key, lane.revision, false)
                    .is_ok()
                {
                    self.emit_write_fact(
                        id,
                        WriteFact::Relay {
                            relay: lane.key.relay.clone(),
                            state: RelayState::Waiting(RelayWaiting::NotConnected),
                        },
                        &mut effects,
                    );
                    effects.push(Effect::EnsureWriteRelay(session));
                } else {
                    self.retry_scheduler_blocked = true;
                }
                continue;
            }
            // The AUTH gate: a lane parks before an attempt ordinal is
            // allocated while (a) this exact generation's bounded initial
            // AUTH-discovery observation is still pending, or (b) the relay
            // has actually REQUIRED auth for this session (challenge,
            // auth-required write ack, or restricted close — all of which
            // insert `auth_required_sessions`) and the exact current
            // generation has not completed AUTH. An unchallenged ordinary
            // relay proceeds after its probe releases: a relay that never
            // challenges must not wedge every write, and one that only
            // reveals auth-requirement via `OK false auth-required:` still
            // parks through `handle_write_ack`'s `RelayAckClass::WaitingAuth`
            // path.
            if self.auth_probe_sessions.contains_key(&session)
                || (self.auth_required_sessions.contains(&session)
                    && !self.auth_ready_sessions.contains_key(&session))
            {
                if self
                    .commit_lane_waiting(&lane.key, lane.revision, true)
                    .is_ok()
                {
                    self.emit_write_fact(
                        id,
                        WriteFact::Relay {
                            relay: lane.key.relay.clone(),
                            state: RelayState::Waiting(RelayWaiting::NeedsAuth),
                        },
                        &mut effects,
                    );
                } else {
                    self.retry_scheduler_blocked = true;
                }
                continue;
            }
            if in_flight >= MAX_GLOBAL_ATTEMPTS || in_flight_relays.contains(&lane.key.relay) {
                continue;
            }
            let Some(event) = self.pending.get(&id).map(|pending| pending.frozen.clone()) else {
                continue;
            };
            let Ok(correlation) = self.alloc_attempt_correlation() else {
                continue;
            };
            let (attempt, advanced) = match self.commit_lane_attempt_start(
                &lane.key,
                lane.revision,
                event.clone(),
                now,
            ) {
                Ok(result) => result,
                Err(_) => {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.unstarted_relays.insert(lane.key.relay.clone());
                    }
                    self.emit_write_fact(
                        id,
                        WriteFact::Relay {
                            relay: lane.key.relay,
                            state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
                                detail: ATTEMPT_STALL_DETAIL.to_string(),
                            }),
                        },
                        &mut effects,
                    );
                    continue;
                }
            };
            debug_assert_eq!(
                advanced.state,
                PublishQueueLaneState::InFlight {
                    ordinal: attempt.ordinal,
                    phase: PublishQueueInFlightPhase::AwaitingHandoff,
                }
            );
            if let Some(pending) = self.pending.get_mut(&id) {
                pending.unstarted_relays.remove(&lane.key.relay);
                pending.pending_relays.insert(lane.key.relay.clone());
                pending
                    .attempt_ordinals
                    .insert(lane.key.relay.clone(), attempt.ordinal);
            }
            self.event_to_receipts
                .entry(event.id)
                .or_default()
                .insert(id);
            self.attempt_correlations.insert(
                correlation,
                AttemptCorrelationTarget {
                    receipt: id,
                    session: session.clone(),
                    lane: Some((lane.key.intent_id, attempt.ordinal)),
                },
            );
            effects.push(Effect::PublishEvent(session, event, correlation));
            in_flight += 1;
            in_flight_relays.insert(lane.key.relay);
        }
        effects
    }

    /// Wake every `WaitingConnection` (or, if `auth_only`, `WaitingAuth`)
    /// lane on `session` -- called on every relay connect/disconnect/auth
    /// event. Before epic #507 finding E5, this ran `recover_all_lanes` (a
    /// full `O(pending)` store re-read) and then filtered down to one
    /// relay, TWICE over per event (once here, once again inside
    /// `schedule_ready` at the end). The non-degraded path below instead
    /// narrows via `receipts_by_lane_relay` to exactly the receipts that
    /// actually own a lane on `session.relay`, re-reading only those
    /// intents. (`receipts_by_lane_relay`/`PublishQueueLaneKey` stay URL-keyed in the
    /// store — only the SESSION comparison below, derived per lane from its
    /// pending write's signing identity, decides whether a lane belongs to
    /// THIS session.)
    ///
    /// While `lane_relay_index_degraded`, this falls back to the OLD full
    /// scan, unchanged: the index cannot be trusted to be a superset of
    /// live lanes right now, and guessing wrong here means a lane never
    /// wakes -- a permanently wedged durable write, the worst bug class in
    /// this codebase (see the idle-barrier missed-wakeup fix, d755f39, and
    /// #507's own missed-wakeup finding). A missed wakeup is never an
    /// acceptable price for narrower reads.
    pub(super) fn wake_relay_lanes(
        &mut self,
        session: &RelaySessionKey,
        auth_only: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();

        if self.lane_relay_index_degraded {
            let Ok(lanes) = self.recover_all_lanes() else {
                self.retry_scheduler_blocked = true;
                return effects;
            };
            self.apply_relay_wake(session, auth_only, lanes, &mut effects);
            effects.extend(self.schedule_ready(self.clock));
            return effects;
        }

        // Clone the candidate receipt set first: the loop below needs a
        // mutable borrow of `self` (store reads, `retry_scheduler_blocked`),
        // so it cannot hold a live borrow of `self.receipts_by_lane_relay`
        // at the same time.
        let candidates: Vec<ReceiptId> = self
            .receipts_by_lane_relay
            .get(&session.relay)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let mut lanes: Vec<(ReceiptId, PublishQueueLane)> = Vec::new();
        for id in candidates {
            let Some(intent_id) = self.pending.get(&id).map(|pending| pending.intent_id) else {
                continue;
            };
            match self.resolver.store().recover_publish_queue_lanes(intent_id) {
                Ok(recovered) => lanes.extend(
                    recovered
                        .into_iter()
                        .filter(|lane| lane.key.relay == session.relay)
                        .map(|lane| (id, lane)),
                ),
                Err(_) => {
                    // A transient read failure for this one receipt, not an
                    // indexing gap -- the established `retry_scheduler_blocked`
                    // idiom (a later engine message retries) applies exactly
                    // as it does everywhere else this door is read, without
                    // needing to distrust the whole index.
                    self.retry_scheduler_blocked = true;
                }
            }
        }
        // Same deterministic order `recover_all_lanes` produces (by
        // `lane.key`): order affects effect emission order, and this must be
        // indistinguishable from the old full-scan behavior for a given
        // input, not merely equivalent in aggregate.
        lanes.sort_by(|(_, left), (_, right)| left.key.cmp(&right.key));

        self.apply_relay_wake(session, auth_only, lanes, &mut effects);
        effects.extend(self.schedule_ready(self.clock));
        effects
    }

    /// The exact per-lane wake body `wake_relay_lanes` ran inline before
    /// epic #507 finding E5, shared now by both its indexed fast path and
    /// its degraded full-scan fallback so the two are behaviorally
    /// identical for a given input. `lanes` is assumed pre-sorted by
    /// `lane.key` (both callers already do this); it need NOT be pre-
    /// filtered to `session` -- the loop below still filters, since the
    /// degraded fallback hands it every pending intent's lanes unfiltered
    /// (exactly as the old, pre-#507 `wake_relay_lanes` body did). A lane
    /// whose receipt has no pending entry is skipped: without a live pending
    /// write there is nothing to wake. Since the AUTH-reducer wave (#8 U2)
    /// the write plane rides the lane's identity-scoped authenticated
    /// session, so a lane belongs to `RelaySessionKey::new(lane.key.relay,
    /// Nip42(pending.signing_pubkey))`.
    pub(super) fn apply_relay_wake(
        &mut self,
        session: &RelaySessionKey,
        auth_only: bool,
        lanes: Vec<(ReceiptId, PublishQueueLane)>,
        effects: &mut Vec<Effect>,
    ) {
        for (id, lane) in lanes {
            let Some(signing_pubkey) = self.pending.get(&id).map(|pending| pending.signing_pubkey)
            else {
                continue;
            };
            if RelaySessionKey::new(lane.key.relay.clone(), AccessContext::Nip42(signing_pubkey))
                != *session
            {
                continue;
            }
            let should_wake = if auth_only {
                matches!(lane.state, PublishQueueLaneState::WaitingAuth)
            } else {
                matches!(lane.state, PublishQueueLaneState::WaitingConnection)
            };
            if !should_wake {
                continue;
            }
            let retry_detail = (!auth_only && lane.last_ordinal > 0)
                .then(|| {
                    self.resolver
                        .store()
                        .recover_attempt_details(lane.key.intent_id)
                        .ok()?
                        .into_iter()
                        .find(|detail| {
                            detail.relay == lane.key.relay && detail.ordinal == lane.last_ordinal
                        })?
                        .transient
                })
                .flatten()
                .and_then(|transient| {
                    public_retry_cause(transient.cause).map(|cause| (cause, transient.raw_reason))
                });
            if self
                .commit_lane_eligible(&lane.key, lane.revision, self.clock)
                .is_err()
            {
                self.retry_scheduler_blocked = true;
            } else if let Some((cause, detail)) = retry_detail {
                self.emit_write_fact(
                    id,
                    WriteFact::Relay {
                        relay: lane.key.relay,
                        state: RelayState::Waiting(RelayWaiting::BackingOff {
                            attempt: lane.last_ordinal,
                            eligible_at: self.clock,
                            cause,
                            detail,
                        }),
                    },
                    effects,
                );
            }
        }
    }

    pub(super) fn consume_due_publish_queue_deadlines(&mut self, now: Timestamp) -> Vec<Effect> {
        let mut effects = Vec::new();
        loop {
            let due = match self
                .resolver
                .store()
                .due_publish_queue_deadlines(now, DEADLINE_READ_BATCH)
            {
                Ok(due) => due,
                Err(_) => {
                    self.retry_scheduler_blocked = true;
                    break;
                }
            };
            if due.is_empty() {
                break;
            }
            for deadline in due {
                let id = self.receipt_for_intent(deadline.key.intent_id);
                let lane = self
                    .resolver
                    .store()
                    .recover_publish_queue_lanes(deadline.key.intent_id)
                    .ok()
                    .and_then(|lanes| {
                        lanes.into_iter().find(|lane| {
                            lane.key == deadline.key && lane.revision == deadline.lane_revision
                        })
                    });
                let Some(lane) = lane else {
                    self.retry_scheduler_blocked = true;
                    continue;
                };
                match (deadline.kind, lane.state.clone()) {
                    (
                        PublishQueueDeadlineKind::RetryEligible,
                        PublishQueueLaneState::Transient { .. },
                    ) => {
                        if self
                            .commit_lane_eligible(&lane.key, lane.revision, deadline.at)
                            .is_err()
                        {
                            self.retry_scheduler_blocked = true;
                        }
                    }
                    (
                        PublishQueueDeadlineKind::AckTimeout,
                        PublishQueueLaneState::InFlight {
                            ordinal,
                            phase: PublishQueueInFlightPhase::AwaitingAck { .. },
                        },
                    ) => {
                        if !self.retry_or_give_up(
                            id,
                            &lane.key.clone(),
                            lane.revision,
                            ordinal,
                            now,
                            PublishQueueTransientCause::AckTimeout,
                            "ack timeout".to_string(),
                            &mut effects,
                        ) {
                            self.retry_scheduler_blocked = true;
                        }
                    }
                    _ => self.retry_scheduler_blocked = true,
                }
            }
            if self.retry_scheduler_blocked {
                break;
            }
        }
        effects.extend(self.schedule_ready(now));
        effects
    }

    /// Rebuild volatile ownership from the journal without reinserting a
    /// single row. Called exactly once by the runtime before its first
    /// command. Retry clocks are reconstructed only from persisted lane facts.
    pub fn recover_on_boot(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        // #790: the journal is now allowed to say "unreadable" instead of
        // panicking the host mid-boot. An `Err` here is NOT "nothing is
        // open": the durable obligation set could not be proven, so this
        // fabricates nothing from it -- no receipt, no lane, no signer
        // request, no route resolution, no wire effect -- and leaves
        // `pending`/`lane_relay_index_degraded` in the untrustworthy state
        // they must be in for a set that was never rebuilt. The one-shot
        // #122 degradation is the whole visible outcome.
        let recovered = match self.resolver.store().recover_publish_queue() {
            Ok(recovered) => recovered,
            Err(error) => {
                self.lane_relay_index_degraded = true;
                // Nothing was rebuilt, so there is no intent to retry a
                // bootstrap for: this gap is closable only by another boot.
                self.lane_projection_unprovable = true;
                self.degrade_store(error, &mut effects);
                return effects;
            }
        };
        let mut recovered_ids = Vec::new();
        // This is the one deterministic, from-scratch rebuild of `pending`
        // (and, with it, every index derived from `pending`) -- the exact
        // moment `receipts_by_lane_relay` can be trusted again regardless of
        // what happened in a prior process (epic #507 finding E5).
        self.lane_relay_index_degraded = false;
        self.lane_projection_unprovable = false;
        // Every gap recorded against the previous `pending` set refers to
        // receipt ids this rebuild is about to re-derive from the store.
        // Carrying them across would retry on behalf of a projection that no
        // longer exists; the rebuild below re-registers whatever still fails.
        self.lane_bootstrap_retries.clear();

        for intent in recovered {
            if intent.frozen.kind == nostr::Kind::Authentication {
                let id = ReceiptId(intent.receipt_id);
                let reason = "recovered kind:22242 ordinary write quarantined from AUTH ownership"
                    .to_string();
                self.quarantined_auth_receipts.insert(
                    id,
                    QuarantinedWrite {
                        intent_id: intent.intent_id,
                        frozen: intent.frozen.clone(),
                    },
                );
                effects.push(Effect::EmitReceipt(
                    id,
                    WriteFact::Signing(SigningState::Refused { reason }),
                ));
                continue;
            }
            let parsed_routing = Self::parse_routing_snapshot(&intent.routing);
            let routing_valid = parsed_routing.is_some();
            // An unreadable row is retained exactly as written and never
            // resolved (`routing_valid == false` gates every send path). The
            // in-memory stand-in is the one value that cannot contact a
            // relay even if that gate were ever bypassed — guessing `Auto`
            // here would republish an old obligation to relays nobody chose
            // for it.
            let routing = parsed_routing.unwrap_or(WriteRouting::Explicit(Vec::new()));
            let id = ReceiptId(intent.receipt_id);
            let already_signed = intent.sig_state == IntentSigState::Signed;
            self.pending.insert(
                id,
                PendingWrite {
                    routing,
                    routing_valid,
                    intent_id: intent.intent_id,
                    // The DURABLE acceptance instant, replayed verbatim. It is
                    // what makes a stalled-write projection identical either
                    // side of a restart: nothing here is a process-local
                    // stopwatch that a reopen would reset to zero.
                    destinations_reported: false,
                    persistence_fault: None,
                    accepted_at: intent.accepted_at,
                    signing_pubkey: intent.expected_pubkey,
                    frozen: intent.frozen.clone(),
                    already_signed,
                    sign_request_in_flight: false,
                    sign_generation: 0,
                    event_id: already_signed.then_some(intent.frozen.id),
                    pending_relays: BTreeSet::new(),
                    unstarted_relays: BTreeSet::new(),
                    route_blocked_relays: BTreeSet::new(),
                    attempt_ordinals: BTreeMap::new(),
                    lane_projection: LaneWorkerProjection::default(),
                    durable_routes: BTreeSet::new(),
                    route_complete: false,
                    route_needs: BTreeSet::new(),
                },
            );
            self.intent_receipts.insert(intent.intent_id, id);
            recovered_ids.push(id);

            if !already_signed {
                continue;
            }
            self.event_to_receipts
                .entry(intent.frozen.id)
                .or_default()
                .insert(id);

            let revisions = match self
                .resolver
                .store()
                .recover_route_revisions(intent.intent_id)
            {
                Ok(revisions) => revisions,
                Err(_) => {
                    // This intent may already own real persisted lanes from
                    // before this boot; skipping straight to the next intent
                    // (as below) means `bootstrap_publish_queue_lanes` never runs
                    // for it this boot, so the reverse index can never learn
                    // those lanes -- an unprovable gap, so degrade rather
                    // than silently under-index (epic #507 finding E5).
                    self.lane_relay_index_degraded = true;
                    // The durable route set is exactly what could not be
                    // read, so nothing can be held as `uncertain` and the
                    // projection reports unavailable. Register the gap so a
                    // later tick can bootstrap this intent for real instead
                    // of disabling worker reconciliation for the whole
                    // process (#1000).
                    self.schedule_lane_bootstrap_retry(intent.intent_id, None);
                    continue;
                }
            };
            let mut durable_relays = revisions
                .iter()
                .flat_map(|revision| revision.relays.iter().cloned())
                .collect::<BTreeSet<_>>();

            // Resolution moment TWO: every crash-survivor is re-resolved
            // against the directory THIS process holds, and the revision log
            // absorbs only the delta. The strategy, not the answer, is what
            // survived the crash — so a relay learned while the process was
            // down gets a lane, and a relay the intent already reached is
            // left completely alone.
            if routing_valid {
                let answer = self.resolve_routes(&self.pending[&id].routing, &intent.frozen);
                let new_routes = answer
                    .relays
                    .difference(&durable_relays)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if !new_routes.is_empty() {
                    if self
                        .commit_route_revision(intent.intent_id, answer.relays.clone())
                        .is_err()
                    {
                        if let Some(pending) = self.pending.get_mut(&id) {
                            pending.route_blocked_relays.extend(new_routes);
                        }
                    } else {
                        durable_relays.extend(answer.relays.iter().cloned());
                    }
                }
                if let Some(pending) = self.pending.get_mut(&id) {
                    pending.durable_routes = durable_relays.clone();
                    pending.route_complete = answer.complete;
                    // Needs are STATELESS: nothing about them was recovered
                    // from the journal, they were simply re-derived by the
                    // resolution above. That is what makes a crash cost a
                    // declared need nothing.
                    pending.route_needs = answer.author_route_needs;
                }
            }

            let lanes =
                match self.bootstrap_projected_lanes(intent.intent_id, Some(&durable_relays)) {
                    Ok(lanes) => lanes,
                    Err(_) => {
                        // Same reasoning as the `recover_route_revisions`
                        // error above: this is the sole call that teaches the
                        // reverse index this intent's lanes, so a failure
                        // here is an audit hole, not a "no lanes" fact --
                        // degrade rather than guess (epic #507 finding E5).
                        // The projection door has already recorded the
                        // retryable gap that gets this intent out of its
                        // conservative retention (#1000).
                        self.lane_relay_index_degraded = true;
                        continue;
                    }
                };
            self.open_bootstrapped_lanes(id, intent.expected_pubkey, lanes, &mut effects);
        }

        self.retry_scheduler_blocked = false;
        let due = self.consume_due_publish_queue_deadlines(self.clock);
        effects.extend(due);
        for id in recovered_ids {
            self.close_if_all_lanes_terminal(id, &mut effects);
        }
        // `pending` started empty in this process, so every need rebuilt
        // above is new to the protocol assembly even if the prior process
        // had already queried it. Needs themselves are deliberately
        // stateless; replay the rebuilt set through the same typed effect
        // live rewrites use so NIP-65 can reopen discovery after a crash.
        let recovered_route_needs = self.author_route_needs();
        if !recovered_route_needs.is_empty() {
            effects.push(Effect::AuthorRouteNeedsChanged(recovered_route_needs));
        }
        effects
    }

    /// Drive one intent's freshly established lane set back into ordinary
    /// write-plane work.
    ///
    /// Shared by boot recovery and the bootstrap retry below, because a lane
    /// set established late is indistinguishable from one established at
    /// boot: both may hold an attempt interrupted by a previous process, a
    /// generation-scoped AUTH park that cannot survive, or a lane simply
    /// waiting for its session. `Eligible` and `Transient` lanes need only
    /// their session; the ordinary scheduler and deadline sweep drive them
    /// from there.
    fn open_bootstrapped_lanes(
        &mut self,
        _id: ReceiptId,
        signing_pubkey: PublicKey,
        lanes: Vec<PublishQueueLane>,
        effects: &mut Vec<Effect>,
    ) {
        for lane in lanes {
            // The recovered write lane's worker demand is the intent's
            // identity-scoped authenticated session (#8 U2); recovery
            // redials exactly the session the lane will publish on. The
            // signing identity was frozen at acceptance, never re-read from
            // the mutable active account.
            let session =
                RelaySessionKey::new(lane.key.relay.clone(), AccessContext::Nip42(signing_pubkey));
            match lane.state {
                PublishQueueLaneState::InFlight {
                    ordinal,
                    phase: PublishQueueInFlightPhase::AwaitingHandoff,
                } => {
                    // An attempt in flight across a process loss is simply
                    // retried: the resend is the IDENTICAL frozen event, so a
                    // relay that already took it dedupes on id.
                    let eligible_at = self.clock;
                    let _ = self.commit_lane_transient(
                        &lane.key,
                        lane.revision,
                        ordinal,
                        eligible_at,
                        PublishQueueTransientCause::Interrupted,
                        Some("process restarted before handoff resolved".to_string()),
                    );
                }
                PublishQueueLaneState::WaitingConnection
                | PublishQueueLaneState::Eligible { .. }
                | PublishQueueLaneState::Transient { .. } => {
                    effects.push(Effect::EnsureWriteRelay(session));
                }
                PublishQueueLaneState::InFlight {
                    phase: PublishQueueInFlightPhase::AwaitingAck { .. },
                    ..
                } => {
                    effects.push(Effect::EnsureWriteRelay(session));
                }
                PublishQueueLaneState::WaitingAuth => {
                    // A `WaitingAuth` park never survives a restart: its
                    // authenticated grant was generation-scoped to a socket
                    // this process no longer holds. Recover it as
                    // `WaitingConnection` so the post-connect
                    // `wake_relay_lanes(.., auth_only=false)` re-drives it;
                    // leaving it `WaitingAuth` would strand it forever
                    // (its only wake, `finish_auth_ok`, needs a fresh
                    // client-provoked challenge that boot alone can't cause).
                    // Fail-safe like the disconnect arm: a swallowed reset
                    // failure would silently re-strand the lane — exactly
                    // the missed-wakeup class this guards — so on error mark
                    // recovery degraded (this function's own untrustworthy-
                    // recovery signal) rather than warm a connection that
                    // cannot wake a still-`WaitingAuth` lane.
                    if self
                        .commit_lane_waiting(&lane.key, lane.revision, false)
                        .is_ok()
                    {
                        effects.push(Effect::EnsureWriteRelay(session));
                    } else {
                        self.lane_relay_index_degraded = true;
                    }
                }
                PublishQueueLaneState::Terminal { .. } => {}
            }
        }
    }

    /// Re-run every due lane bootstrap that previously failed to commit.
    ///
    /// This is the whole way OUT of the conservative retention a failed
    /// bootstrap takes (#1000). `uncertain` is cleared only by a committed
    /// `PublishQueueLane` for that exact relay, and an intent whose bootstrap
    /// failed owns NO lane rows — so `schedule_ready`, the deadline sweep and
    /// the wake index all find nothing for it and no committed lane fact can
    /// ever arrive on its own. Without this door a single transient store
    /// error pins the intent's relay workers and parks its receipt in
    /// `pending` for the life of the process.
    ///
    /// It is emphatically not a scan: `lane_bootstrap_retries` is empty in
    /// steady state, so the ordinary tick pays one empty-map probe and
    /// worker demand keeps reading zero lanes (#985).
    pub(super) fn retry_lane_bootstraps(&mut self, now: Timestamp) -> Vec<Effect> {
        let due: Vec<ReceiptId> = self
            .lane_bootstrap_retries
            .iter()
            .filter(|(_, retry)| retry.due <= now)
            .map(|(id, _)| *id)
            .collect();
        let mut effects = Vec::new();
        for id in due {
            self.retry_lane_bootstrap(id, &mut effects);
        }
        effects
    }

    fn retry_lane_bootstrap(&mut self, id: ReceiptId, effects: &mut Vec<Effect>) {
        let Some((intent_id, signing_pubkey)) = self
            .pending
            .get(&id)
            .map(|p| (p.intent_id, p.signing_pubkey))
        else {
            // The write left `pending` (closed, cancelled or compensated)
            // while its gap was outstanding, so there is no projection left
            // to reconcile and nothing retains a worker on its behalf.
            self.lane_bootstrap_retries.remove(&id);
            return;
        };
        let candidates = self
            .lane_bootstrap_retries
            .get(&id)
            .and_then(|retry| retry.candidates.clone());
        // On failure the projection door re-arms this entry with the next
        // backoff, so the gap stays owned and retention stays conservative.
        let Ok(lanes) = self.bootstrap_projected_lanes(intent_id, candidates.as_ref()) else {
            return;
        };
        // Committed: the exact rebuild has replaced every conservative guess
        // and the door has dropped the retry entry.
        let connected: BTreeSet<RelaySessionKey> = lanes
            .iter()
            .map(|lane| {
                RelaySessionKey::new(lane.key.relay.clone(), AccessContext::Nip42(signing_pubkey))
            })
            .filter(|session| self.connected_relays.contains(session))
            .collect();
        self.open_bootstrapped_lanes(id, signing_pubkey, lanes, effects);
        // Boot can assume nothing is connected yet, but a retry runs
        // mid-process: a lane whose session is ALREADY live would sit in
        // `WaitingConnection` forever waiting for a `RelayConnected` that
        // has already happened. Replay the exact wake that connection would
        // have delivered.
        for session in connected {
            let woken = self.wake_relay_lanes(&session, false);
            effects.extend(woken);
        }
    }

    /// its retained facts. Unknown ids do not create state.
    pub(super) fn retained_receipt_fact(
        receipt: &nmp_store::PublishQueueReceipt,
    ) -> Option<WriteFact> {
        match receipt.state {
            // Acceptance is not a fact — it is what `publish()` returning
            // `Ok` already said. A receipt that has only been accepted has
            // nothing yet to replay.
            ReceiptState::Accepted => None,
            ReceiptState::Signed => Some(WriteFact::Signing(SigningState::Signed {
                event_id: receipt.frozen_id,
            })),
            // Compensation is the store half of a whole-write failure; the
            // caller-visible reason rode the signing refusal that caused it.
            ReceiptState::Compensated => Some(WriteFact::Signing(SigningState::Refused {
                reason: "write compensated".to_string(),
            })),
            ReceiptState::Cancelled => Some(WriteFact::Outcome(WriteOutcome::NotSent(
                NotSentReason::Cancelled,
            ))),
            ReceiptState::Superseded => Some(WriteFact::Outcome(WriteOutcome::NotSent(
                NotSentReason::Superseded,
            ))),
            ReceiptState::Refused(reason) => {
                Some(WriteFact::Outcome(WriteOutcome::Refused(reason)))
            }
            ReceiptState::NoDestination => Some(WriteFact::Outcome(WriteOutcome::NoDestination)),
        }
    }

    pub fn reattach_receipt(&mut self, id: ReceiptId) -> ReceiptReplayPage {
        self.reattach_receipt_page(id, None, usize::MAX)
    }

    /// Reconstruct one finite page of a receipt's durable prefix.
    ///
    /// The opaque cursor records fact identity independently for each relay
    /// lane, so a newly persisted fact on an earlier-sorted relay cannot
    /// shift another relay's continuation. Core performs no delivery or live
    /// registration; runtime joins a caught-up page to its mailbox registry
    /// while the serialized engine loop still owns the command.
    pub fn reattach_receipt_page(
        &mut self,
        id: ReceiptId,
        cursor: Option<ReceiptReplayCursor>,
        limit: usize,
    ) -> ReceiptReplayPage {
        let mut cursor = match cursor {
            Some(cursor) if cursor.state.receipt_id == id => cursor,
            Some(_) => {
                return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable)
            }
            None => ReceiptReplayCursor::new(id),
        };
        if self.quarantined_auth_receipts.contains_key(&id) {
            return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable);
        }
        let receipt = match self.resolver.store().reattach_receipt(id.0) {
            Ok(Some(receipt)) => receipt,
            Ok(None) => return ReceiptReplayPage::unavailable(ReattachOutcome::NotFound),
            Err(_) => {
                return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable)
            }
        };
        if self
            .pending
            .get(&id)
            .is_some_and(|pending| !pending.routing_valid)
        {
            // Boot retained the obligation but could not interpret its
            // frozen routing policy. Replaying even the readable receipt
            // prefix would falsely imply that this observer is attached to
            // actionable live work, and registering it would leak later
            // signer facts from an obligation whose destination is unknown.
            return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable);
        }
        let (attempts, details, lanes) = match receipt.intent_id {
            Some(intent_id) => {
                let attempts = match self.resolver.store().recover_attempts(intent_id) {
                    Ok(attempts) => attempts,
                    Err(_) => {
                        return ReceiptReplayPage::unavailable(
                            ReattachOutcome::RetainedButUnreadable,
                        )
                    }
                };
                let details = match self.resolver.store().recover_attempt_details(intent_id) {
                    Ok(details) => details,
                    Err(_) => {
                        return ReceiptReplayPage::unavailable(
                            ReattachOutcome::RetainedButUnreadable,
                        )
                    }
                };
                let lanes = match self.resolver.store().recover_publish_queue_lanes(intent_id) {
                    Ok(lanes) => lanes,
                    Err(_) => {
                        return ReceiptReplayPage::unavailable(
                            ReattachOutcome::RetainedButUnreadable,
                        )
                    }
                };
                if self
                    .resolver
                    .store()
                    .recover_route_revisions(intent_id)
                    .is_err()
                {
                    return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable);
                }
                (attempts, details, lanes)
            }
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let mut replay = Vec::new();
        if let Some(status) = Self::retained_receipt_fact(&receipt) {
            replay.push((ReceiptReplayFactKey::ReceiptStatus, status));
        }
        // A reattaching app is told which of the two unsigned states this
        // obligation is in, exactly as the queue projection reports it
        // (#1261): a signer holding the request is not a signer nobody has.
        if receipt.state == ReceiptState::Accepted
            && self
                .pending
                .get(&id)
                .is_some_and(|pending| !pending.already_signed)
        {
            replay.push((
                ReceiptReplayFactKey::AwaitingCapability,
                WriteFact::Signing(Self::signing_park(
                    receipt.expected_pubkey,
                    self.pending.get(&id),
                )),
            ));
        }
        // The routing park is retained and replayed the same way the signer
        // park is. An app that restarts, reattaches to an id it persisted,
        // and is told nothing has learned nothing -- a park nobody can see
        // again is indistinguishable from data loss.
        if self
            .pending
            .get(&id)
            .is_some_and(|pending| pending.durable_routes.is_empty() && !pending.route_complete)
        {
            replay.push((
                ReceiptReplayFactKey::Destinations,
                WriteFact::Destinations {
                    relays: BTreeSet::new(),
                    complete: false,
                },
            ));
        }
        if receipt.intent_id.is_some() {
            let mut details_by_attempt = details
                .into_iter()
                .map(|detail| ((detail.relay.clone(), detail.ordinal), detail))
                .collect::<BTreeMap<_, _>>();
            let mut awaiting_relay = BTreeSet::new();
            let mut awaiting_auth = BTreeSet::new();
            let mut retry_eligible = BTreeSet::new();
            for attempt in attempts {
                let replay_relay = attempt.relay.clone();
                let replay_ordinal = attempt.ordinal;
                let replay_key = |phase| ReceiptReplayFactKey::Attempt {
                    relay: replay_relay.clone(),
                    key: ReceiptAttemptReplayKey {
                        ordinal: replay_ordinal,
                        phase,
                    },
                };
                if let Some(detail) =
                    details_by_attempt.remove(&(attempt.relay.clone(), attempt.ordinal))
                {
                    if let Some(handoff) = detail.handoff {
                        match handoff.result {
                            HandoffEvidence::NotHandedOff => {
                                awaiting_relay.insert((attempt.relay.clone(), attempt.ordinal));
                                replay.push((
                                    replay_key(ReceiptAttemptReplayPhase::Handoff),
                                    WriteFact::Relay {
                                        relay: attempt.relay.clone(),
                                        state: RelayState::Waiting(RelayWaiting::NotConnected),
                                    },
                                ));
                            }
                            HandoffEvidence::Written => replay.push((
                                replay_key(ReceiptAttemptReplayPhase::Handoff),
                                WriteFact::Relay {
                                    relay: attempt.relay.clone(),
                                    state: RelayState::Sent {
                                        attempt: attempt.ordinal,
                                        written_at: handoff.at,
                                    },
                                },
                            )),
                            // An ambiguous handoff is not a fact about the
                            // write: the lane waited for ACK/timeout exactly
                            // as a proven write does, so there is nothing to
                            // replay.
                            HandoffEvidence::Ambiguous => {}
                        }
                    }
                    if let Some(transient) = detail.transient {
                        if transient.cause == PublishQueueTransientCause::AuthRequired {
                            awaiting_auth.insert((attempt.relay.clone(), attempt.ordinal));
                            replay.push((
                                replay_key(ReceiptAttemptReplayPhase::Transient),
                                WriteFact::Relay {
                                    relay: attempt.relay.clone(),
                                    state: RelayState::Waiting(RelayWaiting::NeedsAuth),
                                },
                            ));
                        } else {
                            retry_eligible.insert((
                                attempt.relay.clone(),
                                attempt.ordinal,
                                transient.eligible_at,
                            ));
                            replay.push((
                                replay_key(ReceiptAttemptReplayPhase::Transient),
                                WriteFact::Relay {
                                    relay: attempt.relay.clone(),
                                    state: RelayState::Waiting(RelayWaiting::BackingOff {
                                        attempt: attempt.ordinal,
                                        eligible_at: transient.eligible_at,
                                        cause: public_retry_cause(transient.cause)
                                            .expect("AuthRequired handled above"),
                                        detail: transient.raw_reason,
                                    }),
                                },
                            ));
                        }
                    }
                }
                let status = match attempt.outcome {
                    // Started is only the crash-safe pre-wire fact. #93
                    // deliberately moved Sent to the later transport
                    // Written result, so replaying Started as Sent would
                    // recreate the exact false claim this seam removes.
                    PublishQueueAttemptOutcome::Started => continue,
                    PublishQueueAttemptOutcome::Acked => WriteFact::Relay {
                        relay: attempt.relay,
                        state: RelayState::Published,
                    },
                    PublishQueueAttemptOutcome::Rejected(reason) => WriteFact::Relay {
                        relay: attempt.relay,
                        state: RelayState::Rejected { reason },
                    },
                    PublishQueueAttemptOutcome::GaveUp => WriteFact::Relay {
                        relay: attempt.relay,
                        state: RelayState::GaveUp,
                    },
                };
                replay.push((replay_key(ReceiptAttemptReplayPhase::Outcome), status));
            }
            if !details_by_attempt.is_empty() {
                return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable);
            }
            for lane in lanes {
                let replay_key = ReceiptReplayFactKey::Lane {
                    relay: lane.key.relay.clone(),
                    revision: lane.revision,
                };
                match lane.state {
                    PublishQueueLaneState::WaitingConnection
                        if !awaiting_relay
                            .contains(&(lane.key.relay.clone(), lane.last_ordinal)) =>
                    {
                        replay.push((
                            replay_key,
                            WriteFact::Relay {
                                relay: lane.key.relay,
                                state: RelayState::Waiting(RelayWaiting::NotConnected),
                            },
                        ));
                    }
                    PublishQueueLaneState::WaitingAuth
                        if !awaiting_auth
                            .contains(&(lane.key.relay.clone(), lane.last_ordinal)) =>
                    {
                        replay.push((
                            replay_key,
                            WriteFact::Relay {
                                relay: lane.key.relay,
                                state: RelayState::Waiting(RelayWaiting::NeedsAuth),
                            },
                        ));
                    }
                    PublishQueueLaneState::Terminal {
                        outcome: PublishQueueTerminalOutcome::AuthDenied(denial),
                        ..
                    } => {
                        replay.push((
                            replay_key,
                            WriteFact::Relay {
                                relay: lane.key.relay,
                                state: RelayState::AuthFailed {
                                    pubkey: receipt.expected_pubkey,
                                    source: public_auth_denial_source(denial.source),
                                    reason: denial.reason,
                                },
                            },
                        ));
                    }
                    PublishQueueLaneState::Transient {
                        ordinal,
                        eligible_at,
                        cause,
                        raw_reason,
                    } if cause != PublishQueueTransientCause::AuthRequired
                        && !retry_eligible.contains(&(
                            lane.key.relay.clone(),
                            ordinal,
                            eligible_at,
                        )) =>
                    {
                        replay.push((
                            replay_key,
                            WriteFact::Relay {
                                relay: lane.key.relay,
                                state: RelayState::Waiting(RelayWaiting::BackingOff {
                                    attempt: ordinal,
                                    eligible_at,
                                    cause: public_retry_cause(cause)
                                        .expect("AuthRequired excluded by guard"),
                                    detail: raw_reason,
                                }),
                            },
                        ));
                    }
                    _ => {}
                }
            }
        }
        if let Some(pending) = self.pending.get(&id) {
            for relay in &pending.unstarted_relays {
                replay.push((
                    ReceiptReplayFactKey::PersistenceStalled(
                        relay.clone(),
                        PersistenceStallKind::Attempt,
                    ),
                    WriteFact::Relay {
                        relay: relay.clone(),
                        state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
                            detail: ATTEMPT_STALL_DETAIL.to_string(),
                        }),
                    },
                ));
            }
            for relay in &pending.route_blocked_relays {
                replay.push((
                    ReceiptReplayFactKey::PersistenceStalled(
                        relay.clone(),
                        PersistenceStallKind::Route,
                    ),
                    WriteFact::Relay {
                        relay: relay.clone(),
                        state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
                            detail: ROUTE_STALL_DETAIL.to_string(),
                        }),
                    },
                ));
            }
        }
        if limit == 0 {
            return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable);
        }
        let page = replay
            .iter()
            .filter(|(key, status)| !cursor.contains(key, status))
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let mut facts = Vec::with_capacity(page.len());
        let input_cursor = cursor.clone();
        let mut isolated_fact_cursors = Vec::with_capacity(page.len());
        for (key, status) in page {
            let mut isolated = input_cursor.clone();
            isolated.advance(key.clone(), status.clone());
            isolated_fact_cursors.push(isolated);
            cursor.advance(key, status.clone());
            facts.push(status);
        }

        // Re-check the complete current evidence against the advanced,
        // identity-stable cursor. This detects unseen facts on every relay,
        // including facts that sort before a different relay's prior page.
        let unseen = replay
            .iter()
            .any(|(key, status)| !cursor.contains(key, status));
        let page_full = facts.len() == limit;
        let next_cursor = (unseen || page_full).then_some(cursor.clone());
        ReceiptReplayPage {
            outcome: ReattachOutcome::Attached,
            facts,
            next_cursor,
            end_cursor: Some(cursor),
            isolated_fact_cursors,
        }
    }

    /// #961: advance one runtime registration's durable checkpoint for one mailbox-
    /// accepted live fact. The cursor moves only for a matching retained fact;
    /// transient live-only statuses deliberately leave it unchanged.
    pub(crate) fn receipt_cursor_after_status(
        &mut self,
        id: ReceiptId,
        cursor: &ReceiptReplayCursor,
        status: &WriteFact,
    ) -> Option<ReceiptReplayCursor> {
        let page = self.reattach_receipt_page(id, Some(cursor.clone()), usize::MAX);
        if page.outcome != ReattachOutcome::Attached {
            return None;
        }
        page.facts
            .iter()
            .position(|candidate| candidate == status)
            .and_then(|index| page.isolated_fact_cursors.get(index).cloned())
    }

    pub(crate) fn receipt_is_live(&self, id: ReceiptId) -> bool {
        self.pending.contains_key(&id)
    }

    /// #591: recover a receipt id from a caller-generated correlation token
    /// -- the door a client uses after a crash that happened BEFORE it
    /// could durably record the `Receipt.id` `publish_tracked` returned.
    /// A resolved token is translated to its receipt id and handed straight
    /// to [`Self::reattach_receipt`], reusing its exact finite replay
    /// behavior unchanged: no new outcome enum, no separate machinery. The
    /// resolved [`ReceiptId`] is returned alongside the outcome (`Some` iff
    /// `Attached`) purely so the caller -- who by construction does NOT
    /// already know it, unlike a plain [`Self::reattach_receipt`] caller --
    /// can learn it.
    pub fn reattach_by_correlation(
        &mut self,
        token: String,
    ) -> (ReceiptReplayPage, Option<ReceiptId>) {
        self.reattach_by_correlation_page(token, None, usize::MAX)
    }

    pub fn reattach_by_correlation_page(
        &mut self,
        token: String,
        cursor: Option<ReceiptReplayCursor>,
        limit: usize,
    ) -> (ReceiptReplayPage, Option<ReceiptId>) {
        match self.resolver.store().lookup_correlation(&token) {
            Ok(Some(receipt_id)) => {
                let id = ReceiptId(receipt_id);
                (self.reattach_receipt_page(id, cursor, limit), Some(id))
            }
            Ok(None) => (
                ReceiptReplayPage::unavailable(ReattachOutcome::NotFound),
                None,
            ),
            Err(_) => (
                ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable),
                None,
            ),
        }
    }

    // ---- publish queue (D: intent -> signed -> routed -> sent -> acked) --

    /// `Publish` (issues #2/#3 U3): enter durable/at-most-once writes through
    /// `resolver.accept_local` exactly once. The store allocates both ids
    /// and commits the canonical pending row, obligation and receipt before
    /// `Accepted` is observable. Ephemeral uses the distinct receipt-only
    /// door: no pending row and no retry obligation, but still a stable,
    /// reattachable receipt as required by the promoted VISION.
    ///
    /// A `Signed` payload is verified here, at the acceptance boundary,
    /// BEFORE `WriteFact::Accepted` is ever emitted (#52 Q2). This is the
    /// only publish path in the crate — `Handle::publish` is the sole entry
    /// point regardless of caller (FFI, direct-Rust, `nmp-bdd`'s
    /// `EngineThread`) — so verifying here, rather than at each caller,
    /// makes "a forged `Signed` event can never be published" true
    /// unconditionally instead of entry-point-dependent. A failed verify is
    /// a whole-intent terminal (`WriteFact::Failed`): no `Accepted`, no
    /// pending write recorded, no `Effect::PublishEvent`.
    ///
    /// Identity resolution (#47): a builder payload carries no author, so
    /// the identity SELECTS one and there is nothing to compare it against
    /// — `Identity::Active` resolves the CURRENT active account (fail
    /// closed pre-acceptance when none is active, since nothing is pinned
    /// so nothing may park), `Identity::Explicit(pk)` stamps `pk`
    /// regardless of the active account, including while logged out. A
    /// `Signed` payload states its author in its own bytes, so there the
    /// identity may only RESTATE it: `Explicit(pk)` naming that author is a
    /// harmless restatement of consent and naming anybody else fails closed
    /// with no `Accepted`, while `Active` means the event's own author and
    /// imposes no active-account requirement at all. Acceptance pins the
    /// resolved key (`expected_pubkey` /
    /// `signing_identity_ref`), so everything downstream — the frozen body,
    /// `RequestSign`, the `SignerAttached` re-arm, restart replay — targets
    /// that one identity forever; a later `set_active_account` cannot
    /// retarget it, and an `Explicit` identity with no registered
    /// capability parks durably as `AwaitingCapability` rather than failing
    /// or drifting.
    pub(super) fn on_publish(&mut self, intent: WriteIntent) -> Vec<Effect> {
        let WriteIntent {
            payload,
            routing,
            identity,
            correlation,
        } = intent;

        // The empty explicit route is refused FIRST, ahead of every other
        // door check: "reject it immediately". Nothing durable may exist for
        // it — no intent, no journal row, no receipt lifecycle, no signer
        // request, no correlation lookup — and it never degrades into `Auto`,
        // because sending a write to relays the caller did not choose is the
        // failure this refusal exists to prevent.
        if matches!(&routing, WriteRouting::Explicit(relays) if relays.is_empty()) {
            return self.refuse_publish(PublishError::EmptyExplicitRoute);
        }

        // An exact route the app named for this write is the app describing
        // its own network (#1251). Routing already executes it verbatim; the
        // socket has to agree, or an app that publishes to its own LAN relay
        // is told the write was routed and then cannot reach it.
        if let WriteRouting::Explicit(relays) = &routing {
            self.heed_relays(relays.iter().cloned());
        }

        // #591: a token that already resolves to a previously-accepted
        // receipt REATTACHES that existing obligation -- this call enqueues
        // no second write, and `payload`/`durability`/`routing`/
        // `identity` above are discarded entirely without so much
        // as a body comparison (a legitimately re-composed draft with a
        // fresh `created_at` is the exact scenario the token exists for).
        // The lookup runs inside this single-threaded reducer step, before
        // any store mutation for THIS call -- TOCTOU-free by construction
        // (no concurrent `&mut self` call can be interleaved).
        if let Some(token) = &correlation {
            match self.resolver.store().lookup_correlation(token.as_ref()) {
                Ok(Some(existing_receipt_id)) => {
                    let receipt_id = ReceiptId(existing_receipt_id);
                    let page = self.reattach_receipt(receipt_id);
                    // A repeated durable correlation is a finite replay of
                    // the existing obligation, not a second write. Keep that
                    // replay distinct from a new live fact so runtime can
                    // prime only this publisher's fresh mailbox before it
                    // joins live delivery.
                    if page.outcome == ReattachOutcome::Attached {
                        debug_assert!(!page.facts.is_empty());
                        return vec![Effect::ReplayReceipt(receipt_id, page)];
                    }
                    // Review (#591, PR #604 finding 1): never mask a corrupt
                    // retained identity behind fabricated acceptance.
                    let status = match page.outcome {
                        ReattachOutcome::Attached => unreachable!("handled above"),
                        ReattachOutcome::NotFound => PublishError::PersistenceFailed {
                            reason:
                                "correlation token resolved to a receipt id the store can no longer find"
                                    .to_string(),
                        },
                        ReattachOutcome::RetainedButUnreadable => PublishError::PersistenceFailed {
                            reason: "correlation token resolved to a retained but unreadable receipt"
                                .to_string(),
                        },
                    };
                    debug_assert!(page.facts.is_empty());
                    let _ = page;
                    return self.refuse_publish(status);
                }
                Ok(None) => {}
                Err(err) => {
                    return self.refuse_publish(PublishError::PersistenceFailed {
                        reason: err.to_string(),
                    })
                }
            }
        }

        let replaceable_base = match &payload {
            WritePayload::ReplaceableEdit { expected_base, .. } => Some(*expected_base),
            WritePayload::Event(_) | WritePayload::Signed(_) => None,
        };
        // The store owns this write's timestamp exactly when the app left
        // it unsaid on an edit that names a base: it is the only component
        // that can read the winner and the precondition in one breath. A
        // caller-stated `created_at` is never moved, on any payload —
        // including one that regresses below the winner and loses the
        // replacement race, which stays observable rather than forbidden.
        let monotonic_stamp = matches!(
            &payload,
            WritePayload::ReplaceableEdit { builder, .. } if builder.created_at.is_none()
        );

        let payload_kind = match &payload {
            WritePayload::Event(builder) | WritePayload::ReplaceableEdit { builder, .. } => {
                builder.kind
            }
            WritePayload::Signed(event) => event.kind,
        };
        if payload_kind == nostr::Kind::Authentication {
            return self.refuse_publish(PublishError::ReservedKind {
                kind: payload_kind.as_u16(),
            });
        }

        let signing_pubkey = match &payload {
            // A builder carries no author, so the identity SELECTS one —
            // there is no second source of truth for it to disagree with,
            // and the mismatch class #47 fails closed on is unrepresentable
            // here rather than merely refused.
            WritePayload::Event(_) | WritePayload::ReplaceableEdit { .. } => match identity {
                // Explicit per-write consent to publish as `pk`. The active
                // account is irrelevant (even logged out): acceptance pins
                // `pk` and downstream signing targets it forever.
                Identity::Explicit(pk) => pk,
                // Whoever is active at acceptance. An instruction that
                // cannot resolve is a refusal, not a parked hope — nothing
                // is pinned, so nothing may park.
                Identity::Active => match self.active_pubkey {
                    Some(active) => active,
                    None => return self.refuse_publish(PublishError::NoActiveAccount),
                },
            },
            // Already-signed payloads are verified verbatim and never ask a
            // local signer, so their author is intrinsically frozen. An
            // explicit identity may still name that author (a harmless
            // restatement) — but naming anyone ELSE is a consent/author
            // contradiction and fails closed before acceptance (#47).
            WritePayload::Signed(event) => match identity {
                Identity::Explicit(pk) if pk != event.pubkey => {
                    return self.refuse_publish(PublishError::IdentityContradictsSignedAuthor {
                        identity: pk,
                        author: event.pubkey,
                    });
                }
                Identity::Explicit(_) | Identity::Active => event.pubkey,
            },
        };

        if let WritePayload::Signed(event) = &payload {
            if let Err(err) = event.verify() {
                return self.refuse_publish(PublishError::SignatureInvalid {
                    reason: err.to_string(),
                });
            }
        }

        let mut frozen = Self::freeze_payload(&payload, signing_pubkey, self.clock);

        let (id, intent_id, already_signed, accepted_signed_event, committed, retired_intents) = {
            let accept = AcceptWrite {
                frozen: frozen.clone(),
                replaceable_base,
                monotonic_stamp,
                expected_pubkey: signing_pubkey,
                signing_identity_ref: signing_pubkey.to_hex(),
                routing: Self::routing_snapshot(&routing),
                // Treat an unsigned acceptance as reattachable signer work.
                // If a signer is already present the immediate request below
                // promotes it; if not, restart safely re-requests it.
                sig_state: match payload {
                    WritePayload::Event(_) | WritePayload::ReplaceableEdit { .. } => {
                        IntentSigState::AwaitingSigner
                    }
                    WritePayload::Signed(_) => IntentSigState::Pending,
                },
                accepted_at: self.clock,
                correlation,
            };
            let LocalAcceptResult { outcome, committed } = match self.resolver.accept_local(accept)
            {
                Ok(value) => value,
                // Rule 1: recording anything at all needs the disk that just
                // refused. There is no queue entry to fail into.
                Err(err) => {
                    return self.refuse_publish(PublishError::PersistenceFailed {
                        reason: err.to_string(),
                    })
                }
            };
            let Some(intent_id) = outcome.journaled_intent_id() else {
                let AcceptOutcome::Refused(reason) = outcome else {
                    unreachable!("only Refused omits journal ids")
                };
                // CUSTODY. The store was working and said no, which is an
                // answer the app is entitled to read back — so the refusal
                // becomes a one-row, permanently-failed queue entry rather
                // than an error on the call. `ReplaceableBaseChanged` keeps
                // both event ids, which is what lets an app fetch what is
                // actually there, reapply the user's change and resubmit
                // without ever troubling them.
                return match self.resolver.store_mut().accept_refused(
                    frozen.id,
                    signing_pubkey,
                    reason,
                ) {
                    Ok(receipt_id) => {
                        let id = ReceiptId(receipt_id);
                        vec![
                            Effect::WriteAccepted(id),
                            Effect::EmitReceipt(
                                id,
                                WriteFact::Outcome(WriteOutcome::Refused(reason)),
                            ),
                        ]
                    }
                    Err(err) => self.refuse_publish(PublishError::PersistenceFailed {
                        reason: err.to_string(),
                    }),
                };
            };
            let receipt_id = outcome
                .journaled_receipt_id()
                .expect("journaled intent always has a receipt id");
            // The acceptance transaction may have moved a replaceable
            // edit's `created_at` forward against the row it CAS-ed, which
            // re-derives the id. The body it actually froze is the one
            // everything downstream must target — the signer request, the
            // pending row, the delivered bytes.
            if let Some(row) = outcome.accepted_row() {
                if row.event.id != frozen.id {
                    frozen = SignedEvent::new(
                        row.event.id,
                        row.event.pubkey,
                        row.event.created_at,
                        row.event.kind,
                        row.event.tags.clone(),
                        row.event.content.clone(),
                        sentinel_signature(),
                    );
                }
            }
            let accepted_signed_event = match &outcome {
                AcceptOutcome::Duplicate { row, .. } if row.event.sig != sentinel_signature() => {
                    Some(row.event.clone())
                }
                _ => None,
            };
            let retired_intents = match &outcome {
                AcceptOutcome::Superseded { retired, .. } => retired.clone(),
                _ => Vec::new(),
            };
            (
                ReceiptId(receipt_id),
                Some(intent_id),
                accepted_signed_event.is_some(),
                accepted_signed_event,
                Some(committed),
                retired_intents,
            )
        };

        // Acceptance IS `publish()` returning `Ok`, never a stream item: an
        // app that must ask the stream whether its write was accepted is an
        // app being made to wait on something it already knows.
        let mut effects = vec![Effect::WriteAccepted(id)];

        self.pending.insert(
            id,
            PendingWrite {
                routing,
                routing_valid: true,
                intent_id: intent_id.expect("a journaled acceptance always has an intent id"),
                // Exactly the value handed to `AcceptWrite::accepted_at`
                // above, so the in-process projection and the one a later
                // boot rebuilds from `PublishQueueIntent` are the same instant.
                accepted_at: self.clock,
                destinations_reported: false,
                persistence_fault: None,
                signing_pubkey,
                frozen: frozen.clone(),
                already_signed,
                sign_request_in_flight: false,
                sign_generation: 0,
                event_id: None,
                pending_relays: BTreeSet::new(),
                unstarted_relays: BTreeSet::new(),
                route_blocked_relays: BTreeSet::new(),
                attempt_ordinals: BTreeMap::new(),
                lane_projection: LaneWorkerProjection::default(),
                durable_routes: BTreeSet::new(),
                route_complete: false,
                route_needs: BTreeSet::new(),
            },
        );
        // `intent_id` is `None` only for Ephemeral, which never owns a
        // pending row or a lane -- nothing to index for it (epic #507
        // finding E5).
        if let Some(intent_id) = intent_id {
            self.intent_receipts.insert(intent_id, id);
        }

        if let Some(committed) = committed {
            // A local pending row was committed before Accepted. When it did
            // not alter reactive demand/router shape, expose its exact row
            // facts through the same O(committed delta) projection path as a
            // relay batch. Any demand change keeps the broad refresh oracle.
            self.apply_committed_mutation(committed, &mut effects);
        }

        for retired in retired_intents {
            let retired_id = ReceiptId(retired.receipt_id);
            self.emit_write_fact(
                retired_id,
                WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Superseded)),
                &mut effects,
            );
            if let Some(retired_pending) = self.pending.remove(&retired_id) {
                self.forget_pending_indexes(retired_id, &retired_pending);
                if let Some(event_id) = retired_pending.event_id {
                    if let Some(receipts) = self.event_to_receipts.get_mut(&event_id) {
                        receipts.remove(&retired_id);
                        if receipts.is_empty() {
                            self.event_to_receipts.remove(&event_id);
                        }
                    }
                }
            } else {
                self.intent_receipts.remove(&retired.intent_id);
            }
        }

        match payload {
            WritePayload::Event(_) | WritePayload::ReplaceableEdit { .. } => {
                if already_signed {
                    self.on_signed(
                        id,
                        accepted_signed_event
                            .expect("already-signed acceptance carries its canonical event"),
                        &mut effects,
                    );
                } else {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.sign_request_in_flight = true;
                        pending.sign_generation += 1;
                        let generation = pending.sign_generation;
                        // The signer signs the FROZEN body, never the
                        // builder: the author and the timestamp are decided
                        // by acceptance, so the builder is not a complete
                        // event and by construction never was one.
                        effects.push(Effect::RequestSign(
                            id,
                            generation,
                            unsigned_from_frozen(&pending.frozen),
                        ));
                    }
                }
            }
            WritePayload::Signed(event) => {
                self.on_signed(id, event, &mut effects);
            }
        }
        effects
    }

    /// `SignerCompleted` (plan §3.4 step 2 continuation): the runtime's
    /// signer capability resolved. Explicit rejection and invalid signer
    /// output are whole-intent terminals (`WriteFact::Failed`). Transport
    /// absence, timeout, and disconnect return the retained obligation to
    /// `AwaitingCapability` so the exact frozen identity can be reattached.
    pub(super) fn on_signer_completed(
        &mut self,
        id: ReceiptId,
        generation: u64,
        result: Result<SignedEvent, SignerError>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(pending) = self.pending.get_mut(&id) else {
            return effects;
        };
        if !pending.sign_request_in_flight || pending.sign_generation != generation {
            return effects;
        }
        pending.sign_request_in_flight = false;
        match result {
            Ok(event) => self.on_signed(id, event, &mut effects),
            Err(err) => {
                if err.is_terminal() {
                    self.fail_and_compensate(id, err.to_string(), &mut effects);
                } else if let Some(pending) = self.pending.get_mut(&id) {
                    let signing_pubkey = pending.signing_pubkey;
                    let status = WriteFact::Signing(SigningState::AwaitingSigner {
                        pubkey: signing_pubkey,
                    });
                    effects.push(Effect::EmitReceipt(id, status));
                    effects.push(Effect::RearmSignerIfAvailable(signing_pubkey));
                }
            }
        }
        effects
    }

    pub(super) fn on_signer_unavailable(&mut self, id: ReceiptId, generation: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(pending) = self.pending.get_mut(&id) {
            if !pending.sign_request_in_flight || pending.sign_generation != generation {
                return effects;
            }
            pending.sign_request_in_flight = false;
            let status = WriteFact::Signing(SigningState::AwaitingSigner {
                pubkey: pending.signing_pubkey,
            });
            effects.push(Effect::EmitReceipt(id, status));
        }
        effects
    }

    pub(super) fn on_signer_attached(&mut self, pk: PublicKey) -> Vec<Effect> {
        // Holding this key's signer is what makes its relay list OUR relay
        // list (#1251): we could have signed it, active or not.
        self.attached_signers.insert(pk);
        let mut effects = Vec::new();
        for (id, pending) in &mut self.pending {
            if pending.signing_pubkey == pk
                && pending.event_id.is_none()
                && !pending.already_signed
                && !pending.sign_request_in_flight
            {
                pending.sign_request_in_flight = true;
                pending.sign_generation += 1;
                // The park ENDED, and an app told nothing keeps showing the
                // alarm the park raised (#1261). A park nobody can see end
                // is as misleading as one nobody can see start.
                effects.push(Effect::EmitReceipt(
                    *id,
                    WriteFact::Signing(SigningState::InFlight { pubkey: pk }),
                ));
                effects.push(Effect::RequestSign(
                    *id,
                    pending.sign_generation,
                    unsigned_from_frozen(&pending.frozen),
                ));
            }
        }
        effects
    }

    /// Commit explicit cancellation only while this receipt is still an
    /// accepted unsigned obligation. The synchronous result and emitted
    /// receipt fact come from the same reducer turn.
    pub(super) fn retained_cancel_result(
        id: ReceiptId,
        receipt: &nmp_store::PublishQueueReceipt,
    ) -> Result<CancelWriteOutcome, CancelWriteError> {
        match receipt.state {
            ReceiptState::Cancelled => Ok(CancelWriteOutcome::Cancelled),
            ReceiptState::Signed => Err(CancelWriteError::AlreadySigned {
                receipt_id: id,
                event_id: receipt.frozen_id,
            }),
            ReceiptState::Compensated => {
                Err(CancelWriteError::AlreadyCompensated { receipt_id: id })
            }
            ReceiptState::Superseded => Err(CancelWriteError::AlreadySuperseded { receipt_id: id }),
            ReceiptState::Refused(_) => Err(CancelWriteError::AlreadyRefused { receipt_id: id }),
            // Terminal already: routing finished and named nobody. There is
            // no obligation left to cancel, only an entry to remove.
            ReceiptState::NoDestination => Err(CancelWriteError::AlreadyRefused { receipt_id: id }),
            ReceiptState::Accepted => Err(CancelWriteError::PersistenceFailed {
                receipt_id: id,
                reason: "accepted receipt has no live cancellation owner".to_string(),
            }),
        }
    }

    /// Which unsigned state a still-unsigned obligation is in.
    ///
    /// One signature, one author, one answer — but two ways of not having it
    /// yet, and they are opposite advice to an app (#1261).
    /// [`SigningState::InFlight`] is a signer holding the request right now:
    /// transient, normal, and ended by the signer answering.
    /// [`SigningState::AwaitingSigner`] is nobody answering for that key at
    /// all: no clock ends it, so the app removing the entry is its only
    /// other exit. An obligation with no live row left is not waiting on any
    /// signer.
    fn signing_park(pubkey: PublicKey, pending: Option<&PendingWrite>) -> SigningState {
        match pending {
            Some(pending) if pending.sign_request_in_flight => SigningState::InFlight { pubkey },
            _ => SigningState::AwaitingSigner { pubkey },
        }
    }

    /// Enumerate the app's own publish queue (#1039).
    ///
    /// Every retained receipt, in receipt-id order, with what NMP knows
    /// about it right now: the signing state, the intended destination set
    /// and whether it is closed, per-relay state, the whole-write outcome if
    /// it has one, and any LATCHED persistence fault.
    ///
    /// This is how an app answers "what have I got outstanding, and what
    /// went wrong with it" without having held a receipt stream open since
    /// acceptance. It is INSPECTION: nothing here blocks, and nothing here
    /// waits for settlement.
    ///
    /// It does not fix #46. Retained receipts and correlation tokens still
    /// regrow without bound; this door makes that growth visible, which is
    /// the first thing a retention rule will need.
    pub fn publish_queue_entries(&self) -> Result<Vec<PublishQueueEntry>, PersistenceError> {
        let receipts = self.resolver.store().enumerate_publish_queue_receipts()?;
        let mut entries = Vec::with_capacity(receipts.len());
        for receipt in receipts {
            let id = ReceiptId(receipt.receipt_id);
            let pending = self.pending.get(&id);
            let signing = match receipt.state {
                ReceiptState::Signed => SigningState::Signed {
                    event_id: receipt.frozen_id,
                },
                ReceiptState::Compensated => SigningState::Refused {
                    reason: "write compensated".to_string(),
                },
                // "A signer has this request" and "no signer answers for
                // this key" are different facts and an app acts differently
                // on each (#1261). The obligation itself knows which: an
                // outstanding sign request is exactly what
                // `sign_request_in_flight` tracks, and it is cleared the
                // moment the signer answers or reports itself unavailable.
                _ => Self::signing_park(receipt.expected_pubkey, pending),
            };
            let outcome = match receipt.state {
                ReceiptState::Cancelled => Some(WriteOutcome::NotSent(NotSentReason::Cancelled)),
                ReceiptState::Superseded => Some(WriteOutcome::NotSent(NotSentReason::Superseded)),
                ReceiptState::Refused(reason) => Some(WriteOutcome::Refused(reason)),
                ReceiptState::NoDestination => Some(WriteOutcome::NoDestination),
                ReceiptState::Accepted | ReceiptState::Signed => match pending {
                    // Routing finished and named nobody. Terminal, and the
                    // one terminal that leaves its open-work row behind (see
                    // `apply_route_answer`).
                    Some(pending)
                        if pending.route_complete && pending.durable_routes.is_empty() =>
                    {
                        Some(WriteOutcome::NoDestination)
                    }
                    // Still open work: no outcome yet.
                    Some(_) => None,
                    // The open-work row is gone and every lane finished.
                    None => {
                        (receipt.state == ReceiptState::Signed).then_some(WriteOutcome::Settled)
                    }
                },
                ReceiptState::Compensated => None,
            };
            let relay_states = pending
                .map(|pending| self.relay_states_for(pending))
                .unwrap_or_default();
            entries.push(PublishQueueEntry {
                receipt_id: id,
                event_id: receipt.frozen_id,
                pubkey: receipt.expected_pubkey,
                accepted_at: pending.map_or(Timestamp::from(0u64), |pending| pending.accepted_at),
                signing,
                relays: pending
                    .map(|pending| pending.durable_routes.clone())
                    .unwrap_or_default(),
                route_complete: pending.is_none_or(|pending| pending.route_complete),
                relay_states,
                outcome,
                persistence_fault: pending.and_then(|pending| pending.persistence_fault.clone()),
            });
        }
        Ok(entries)
    }

    fn relay_states_for(&self, pending: &PendingWrite) -> Vec<(RelayUrl, RelayState)> {
        let Ok(lanes) = self
            .resolver
            .store()
            .recover_publish_queue_lanes(pending.intent_id)
        else {
            return Vec::new();
        };
        lanes
            .into_iter()
            .map(|lane| {
                let state = match lane.state {
                    PublishQueueLaneState::WaitingConnection => {
                        RelayState::Waiting(RelayWaiting::NotConnected)
                    }
                    PublishQueueLaneState::WaitingAuth => {
                        RelayState::Waiting(RelayWaiting::NeedsAuth)
                    }
                    PublishQueueLaneState::Transient {
                        ordinal,
                        eligible_at,
                        cause,
                        ref raw_reason,
                    } => match public_retry_cause(cause) {
                        Some(cause) => RelayState::Waiting(RelayWaiting::BackingOff {
                            attempt: ordinal,
                            eligible_at,
                            cause,
                            detail: raw_reason.clone(),
                        }),
                        None => RelayState::Waiting(RelayWaiting::NeedsAuth),
                    },
                    PublishQueueLaneState::Eligible { .. }
                    | PublishQueueLaneState::InFlight { .. } => {
                        RelayState::Waiting(RelayWaiting::NotConnected)
                    }
                    PublishQueueLaneState::Terminal { ref outcome, .. } => match outcome {
                        PublishQueueTerminalOutcome::Acked => RelayState::Published,
                        PublishQueueTerminalOutcome::Rejected(reason) => RelayState::Rejected {
                            reason: reason.clone(),
                        },
                        PublishQueueTerminalOutcome::GaveUp => RelayState::GaveUp,
                        PublishQueueTerminalOutcome::AuthDenied(denial) => RelayState::AuthFailed {
                            pubkey: pending.signing_pubkey,
                            source: match denial.source {
                                StoredAuthDenialSource::Policy => AuthDenialSource::Policy,
                                StoredAuthDenialSource::Signer => AuthDenialSource::Signer,
                                StoredAuthDenialSource::Relay => AuthDenialSource::Relay,
                            },
                            reason: denial.reason.clone(),
                        },
                    },
                };
                (lane.key.relay, state)
            })
            .collect()
    }

    /// Forget one queue entry, releasing whatever obligation it still holds
    /// (#1039, #1269).
    ///
    /// This is a real TERMINATION path, not housekeeping: a write parked
    /// forever on a signer that never attached, and a permanently-failed
    /// refused entry, end no other way.
    ///
    /// What makes an entry removable is a fact about what is IN MOTION, and
    /// never about how long it has waited — a clock could only convert
    /// ignorance into a verdict, which is the failure the whole publish
    /// queue is shaped to avoid:
    ///
    /// - **A signer HAS the request** (`sign_request_in_flight`, reported as
    ///   [`SigningState::InFlight`]). The answer that ends the wait is
    ///   already on its way, so removal would destroy a write that is about
    ///   to succeed. Refused.
    /// - **The write is signed and owns delivery lanes.** A relay may still
    ///   ack it. Refused, and the store's own open-intent check
    ///   ([`RemoveQueueEntryOutcome::StillOpen`]) is the second half of the
    ///   same answer for anything this reducer does not hold in memory.
    /// - **Nothing is in motion.** No signer holds a request and no
    ///   signature exists, so no lane exists and no relay can ever answer —
    ///   [`SigningState::AwaitingSigner`], the park whose own docstring says
    ///   the app removing the entry is its only other exit. Removable, and
    ///   removing it releases the obligation: the optimistic row the write
    ///   promised is compensated in the same step, because a row left behind
    ///   with no obligation under it is a ghost the app can never account
    ///   for.
    ///
    /// Releasing runs the same atomic compensation cancellation uses, then
    /// forgets the receipt. Between those two durable steps the receipt is a
    /// cancelled one, which is the honest thing for a crash to leave behind:
    /// the obligation really was released, and the entry it left is terminal
    /// and removable, so the app's next attempt finishes the job.
    pub fn remove_publish_queue_entry(
        &mut self,
        id: ReceiptId,
    ) -> (Result<(), RemoveQueueEntryError>, Vec<Effect>) {
        let mut effects = Vec::new();
        let released = match self.release_removable_obligation(id, &mut effects) {
            Ok(released) => released,
            Err(error) => return (Err(error), effects),
        };
        let result = match self.resolver.store_mut().remove_publish_queue_entry(id.0) {
            Ok(RemoveQueueEntryOutcome::Removed) => Ok(()),
            Ok(RemoveQueueEntryOutcome::NotFound) => {
                Err(RemoveQueueEntryError::UnknownReceipt { receipt_id: id })
            }
            Ok(RemoveQueueEntryOutcome::StillOpen) => {
                Err(RemoveQueueEntryError::StillActive { receipt_id: id })
            }
            Err(error) => Err(RemoveQueueEntryError::PersistenceFailed {
                receipt_id: id,
                reason: error.to_string(),
            }),
        };
        if released && result.is_ok() {
            // The park never produced a whole-write outcome, so without this
            // the receipt stream would simply stop — indistinguishable from
            // a dropped subscription, which is exactly the silence
            // `WriteOutcome` exists to make impossible.
            effects.push(Effect::EmitReceipt(
                id,
                WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Removed)),
            ));
            effects.extend(self.schedule_ready(self.clock));
        }
        (result, effects)
    }

    /// Release the still-open obligation behind a removable entry, so the
    /// entry itself can be forgotten. `Ok(true)` when one was released,
    /// `Ok(false)` when the entry owned none.
    fn release_removable_obligation(
        &mut self,
        id: ReceiptId,
        effects: &mut Vec<Effect>,
    ) -> Result<bool, RemoveQueueEntryError> {
        let Some(pending) = self.pending.get(&id) else {
            return Ok(false);
        };
        if pending.sign_request_in_flight || pending.already_signed || pending.event_id.is_some() {
            return Err(RemoveQueueEntryError::StillActive { receipt_id: id });
        }
        let pending = self.pending.remove(&id).expect("presence just observed");
        match self.resolver.store_mut().cancel_write(pending.intent_id) {
            Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                match self
                    .resolver
                    .react_to_compensation(pending.frozen.clone(), &outcome)
                {
                    Ok(committed) => self.apply_committed_mutation(committed, effects),
                    Err(error) => self.degrade_store(error, effects),
                }
                self.forget_pending_indexes(id, &pending);
                Ok(true)
            }
            // The store promoted a signature this reducer had not seen yet.
            // A signed write owns delivery work by definition.
            Ok(CompensateOutcome::AlreadySigned) => {
                self.pending.insert(id, pending);
                Err(RemoveQueueEntryError::StillActive { receipt_id: id })
            }
            // No open row to release. The receipt below is the whole entry.
            Ok(CompensateOutcome::NotFound) => {
                self.forget_pending_indexes(id, &pending);
                Ok(true)
            }
            Err(error) => {
                self.pending.insert(id, pending);
                Err(RemoveQueueEntryError::PersistenceFailed {
                    receipt_id: id,
                    reason: error.to_string(),
                })
            }
        }
    }

    pub fn cancel_write(
        &mut self,
        id: ReceiptId,
    ) -> (Result<CancelWriteOutcome, CancelWriteError>, Vec<Effect>) {
        let mut effects = Vec::new();
        let Some(pending) = self.pending.remove(&id) else {
            if let Some(quarantined) = self.quarantined_auth_receipts.get(&id).cloned() {
                match self
                    .resolver
                    .store_mut()
                    .cancel_write(quarantined.intent_id)
                {
                    Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                        match self
                            .resolver
                            .react_to_compensation(quarantined.frozen, &outcome)
                        {
                            Ok(committed) => self.apply_committed_mutation(committed, &mut effects),
                            Err(error) => self.degrade_store(error, &mut effects),
                        }
                        self.quarantined_auth_receipts.remove(&id);
                        effects.push(Effect::EmitReceipt(
                            id,
                            WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled)),
                        ));
                        effects.extend(self.schedule_ready(self.clock));
                        return (Ok(CancelWriteOutcome::Cancelled), effects);
                    }
                    Ok(CompensateOutcome::AlreadySigned) => {
                        return (
                            Err(CancelWriteError::AlreadySigned {
                                receipt_id: id,
                                event_id: quarantined.frozen.id,
                            }),
                            effects,
                        );
                    }
                    Ok(CompensateOutcome::NotFound) => {}
                    Err(error) => {
                        return (
                            Err(CancelWriteError::PersistenceFailed {
                                receipt_id: id,
                                reason: error.to_string(),
                            }),
                            effects,
                        );
                    }
                }
            }
            let retained = match self.resolver.store().reattach_receipt(id.0) {
                Ok(Some(receipt)) => receipt,
                Ok(None) => {
                    return (
                        Err(CancelWriteError::UnknownReceipt { receipt_id: id }),
                        effects,
                    )
                }
                Err(error) => {
                    return (
                        Err(CancelWriteError::PersistenceFailed {
                            receipt_id: id,
                            reason: error.to_string(),
                        }),
                        effects,
                    )
                }
            };
            let result = Self::retained_cancel_result(id, &retained);
            if result == Ok(CancelWriteOutcome::Cancelled) {
                self.quarantined_auth_receipts.remove(&id);
            }
            return (result, effects);
        };

        if pending.already_signed || pending.event_id.is_some() {
            let event_id = pending.event_id.unwrap_or(pending.frozen.id);
            self.pending.insert(id, pending);
            return (
                Err(CancelWriteError::AlreadySigned {
                    receipt_id: id,
                    event_id,
                }),
                effects,
            );
        }

        {
            let intent_id = pending.intent_id;
            match self.resolver.store_mut().cancel_write(intent_id) {
                Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                    match self
                        .resolver
                        .react_to_compensation(pending.frozen.clone(), &outcome)
                    {
                        Ok(committed) => self.apply_committed_mutation(committed, &mut effects),
                        Err(error) => self.degrade_store(error, &mut effects),
                    }
                }
                Ok(CompensateOutcome::AlreadySigned) => {
                    let event_id = pending.frozen.id;
                    self.pending.insert(id, pending);
                    return (
                        Err(CancelWriteError::AlreadySigned {
                            receipt_id: id,
                            event_id,
                        }),
                        effects,
                    );
                }
                Ok(CompensateOutcome::NotFound) => {
                    let result = match self.resolver.store().reattach_receipt(id.0) {
                        Ok(Some(receipt)) => Self::retained_cancel_result(id, &receipt),
                        Ok(None) => {
                            self.pending.insert(id, pending);
                            return (
                                Err(CancelWriteError::PersistenceFailed {
                                    receipt_id: id,
                                    reason: "accepted receipt disappeared during cancellation"
                                        .to_string(),
                                }),
                                effects,
                            );
                        }
                        Err(error) => {
                            self.pending.insert(id, pending);
                            return (
                                Err(CancelWriteError::PersistenceFailed {
                                    receipt_id: id,
                                    reason: error.to_string(),
                                }),
                                effects,
                            );
                        }
                    };
                    self.pending.insert(id, pending);
                    return (result, effects);
                }
                Err(error) => {
                    self.pending.insert(id, pending);
                    return (
                        Err(CancelWriteError::PersistenceFailed {
                            receipt_id: id,
                            reason: error.to_string(),
                        }),
                        effects,
                    );
                }
            }
        }

        self.forget_pending_indexes(id, &pending);
        effects.push(Effect::EmitReceipt(
            id,
            WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled)),
        ));
        effects.extend(self.schedule_ready(self.clock));
        (Ok(CancelWriteOutcome::Cancelled), effects)
    }

    /// Shared by the pre-signed (`on_publish`) and signer-completed paths:
    /// `Signed` -> resolve `WriteRouting` -> `Routed` -> `PublishEvent` per
    /// relay -> `Sent` per relay. Route failure (ledger #6) is a whole-
    /// intent `Failed` with NO `PublishEvent` emitted for any relay —
    /// structurally, an unroutable private recipient cannot reach the wire
    /// here because `relays` is never bound in that branch. Every borrow of
    /// `self.pending` below is scoped to its own statement so the map can
    /// be freely read/mutated/removed across steps.
    pub(super) fn on_signed(
        &mut self,
        id: ReceiptId,
        event: SignedEvent,
        effects: &mut Vec<Effect>,
    ) {
        let Some(pending) = self.pending.get(&id) else {
            return; // unknown/already-resolved receipt id.
        };
        if pending.event_id.is_some() {
            return; // duplicate/delayed signer completion after routing.
        }

        if let Err(reason) = Self::validate_signed_template(&pending.frozen, &event) {
            self.fail_and_compensate(id, reason, effects);
            return;
        }

        let mut co_receipts = Vec::new();
        {
            let intent_id = pending.intent_id;
            if !pending.already_signed {
                match self
                    .resolver
                    .store_mut()
                    .promote_signed(intent_id, event.sig)
                {
                    Ok(PromoteOutcome::Promoted { co_signed, .. }) => {
                        // The store atomically promotes every exact-duplicate
                        // co-owner against the same canonical bytes. Advance
                        // each matching in-memory obligation too; otherwise
                        // an offline co-owner could remain stranded forever
                        // behind a row that is already validly signed.
                        for co_intent in co_signed {
                            if let Some((receipt_id, co_pending)) = self
                                .pending
                                .iter_mut()
                                .find(|(_, candidate)| candidate.intent_id == co_intent)
                            {
                                co_pending.already_signed = true;
                                co_receipts.push(*receipt_id);
                            }
                        }
                    }
                    Ok(PromoteOutcome::NotFound) => {
                        self.fail_and_compensate(
                            id,
                            "accepted intent was unavailable for signature promotion".to_string(),
                            effects,
                        );
                        return;
                    }
                    Err(err) => {
                        self.fail_and_compensate(id, err.to_string(), effects);
                        return;
                    }
                }
            }
        }

        for co_receipt in co_receipts {
            self.on_signed(co_receipt, event.clone(), effects);
        }

        if let Some(pending) = self.pending.get_mut(&id) {
            pending.event_id = Some(event.id);
            pending.frozen = event.clone();
        }

        if let Some(pending) = self.pending.get_mut(&id) {
            effects.push(Effect::EmitReceipt(
                id,
                WriteFact::Signing(SigningState::Signed { event_id: event.id }),
            ));
            if !pending.routing_valid {
                return;
            }
        }

        // Resolution moment ONE: the bytes are final, so delivery can begin.
        // It is only the FIRST opportunity, never the only one — an answer
        // that comes up short here parks and is re-executed at every later
        // moment (`resolution-lifecycle.md` §5) rather than killing a
        // durable, already-journaled obligation.
        let Some(answer) = self
            .pending
            .get(&id)
            .map(|pending| self.resolve_routes(&pending.routing, &event))
        else {
            return;
        };

        let Some(intent_id) = self.pending.get(&id).map(|pending| pending.intent_id) else {
            return;
        };
        // A signed intent is addressable by event id whether or not routing
        // could name a single relay yet: an ack can only ever arrive for a
        // lane, and a parked intent has none, but the index must be complete
        // the moment the bytes are final so a LATER resolution's lanes need
        // no second registration step.
        self.event_to_receipts
            .entry(event.id)
            .or_default()
            .insert(id);
        let needs_before = self.author_route_needs();
        self.apply_route_answer(id, intent_id, answer, effects);
        self.resync_route_needs(needs_before, effects);
    }

    /// Turn one intent's freshly-minted lanes into live delivery work
    /// mid-process — the counterpart of `open_bootstrapped_lanes`, which
    /// exists for lanes recovered from a PREVIOUS process and therefore has
    /// to reason about interrupted attempts this one cannot produce.
    ///
    /// A lane minted right now is always `WaitingConnection`: if the session
    /// is already up it goes straight to eligible, otherwise the receipt says
    /// so and the worker is asked for.
    fn open_fresh_lanes(
        &mut self,
        id: ReceiptId,
        signing_pubkey: PublicKey,
        lanes: Vec<PublishQueueLane>,
        effects: &mut Vec<Effect>,
    ) {
        let write_access = AccessContext::Nip42(signing_pubkey);
        for lane in lanes {
            if matches!(lane.state, PublishQueueLaneState::WaitingConnection) {
                // The freshly-bootstrapped lane's connectivity check is
                // against the intent's identity-scoped authenticated
                // session (#8 U2), the exact session `schedule_ready` will
                // publish on.
                let session = RelaySessionKey::new(lane.key.relay.clone(), write_access);
                if self.connected_relays.contains(&session) {
                    let _ = self.commit_lane_eligible(&lane.key, lane.revision, self.clock);
                } else {
                    self.emit_write_fact(
                        id,
                        WriteFact::Relay {
                            relay: lane.key.relay.clone(),
                            state: RelayState::Waiting(RelayWaiting::NotConnected),
                        },
                        effects,
                    );
                    effects.push(Effect::EnsureWriteRelay(session));
                }
            }
        }
        effects.extend(self.schedule_ready(self.clock));
    }

    /// Freeze the body acceptance is about. This is where the fields the
    /// app left unsaid get filled in: `author` comes from identity
    /// resolution (a builder structurally cannot state one), and an unstated
    /// `created_at` is stamped `clock` — the moment the body is frozen,
    /// which is the only moment both after the app finished describing the
    /// event and before anything downstream depends on the bytes. A STATED
    /// `created_at` is kept verbatim; present-then-changed is impossible.
    ///
    /// A replaceable edit's stamp can still move forward from here, but only
    /// inside the store's acceptance transaction and only against the row
    /// that transaction is CAS-ing (`AcceptWrite::monotonic_stamp`).
    pub(super) fn freeze_payload(
        payload: &WritePayload,
        author: PublicKey,
        clock: Timestamp,
    ) -> SignedEvent {
        match payload {
            WritePayload::Event(builder) | WritePayload::ReplaceableEdit { builder, .. } => {
                let created_at = builder.created_at.unwrap_or(clock);
                // Tags reach the wire in the order the app wrote them:
                // nothing here reorders, normalises, or filters them.
                let tags = nostr::Tags::from_list(builder.tags.clone());
                SignedEvent::new(
                    EventId::new(&author, &created_at, &builder.kind, &tags, &builder.content),
                    author,
                    created_at,
                    builder.kind,
                    tags,
                    builder.content.clone(),
                    sentinel_signature(),
                )
            }
            WritePayload::Signed(event) => SignedEvent::new(
                event.id,
                event.pubkey,
                event.created_at,
                event.kind,
                event.tags.clone(),
                event.content.clone(),
                sentinel_signature(),
            ),
        }
    }

    pub(super) fn validate_signed_template(
        frozen: &SignedEvent,
        signed: &SignedEvent,
    ) -> Result<(), String> {
        if signed.id != frozen.id
            || signed.pubkey != frozen.pubkey
            || signed.created_at != frozen.created_at
            || signed.kind != frozen.kind
            || signed.tags != frozen.tags
            || signed.content != frozen.content
        {
            return Err(
                "signer returned an event that does not match the accepted template".into(),
            );
        }
        signed
            .verify()
            .map_err(|err| format!("signer returned an invalid signature: {err}"))
    }

    /// The durable spelling of a routing STRATEGY — never a resolved relay
    /// set. `Auto` journals the label alone; resolution runs fresh at every
    /// send opportunity against whatever the engine knows then.
    pub(super) fn routing_snapshot(routing: &WriteRouting) -> String {
        match routing {
            WriteRouting::Auto => "auto".to_string(),
            WriteRouting::Explicit(relays) => format!(
                "explicit-hex:{}",
                relays
                    .iter()
                    .map(|relay| hex::encode(relay.to_string()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    /// Read back a routing snapshot this build understands, or `None`.
    ///
    /// `None` is not an error path to recover from by guessing: a row spelled
    /// under a routing this build cannot read is retained exactly as written
    /// and never resolved (`routing_valid == false`). Reinterpreting a
    /// removed spelling would republish an old obligation to relays nobody
    /// chose for it, which is strictly worse than leaving it inert. Every
    /// spelling this build's own writer does not produce therefore falls here
    /// deliberately, and this decoder names none of them: asserting a dead
    /// approach is still encoding awareness of it.
    ///
    /// An `explicit-hex:` row with no relays is likewise unreadable: an empty
    /// explicit route is refused at the acceptance door, so no legitimate row
    /// can carry one.
    pub(super) fn parse_routing_snapshot(snapshot: &str) -> Option<WriteRouting> {
        if snapshot == "auto" {
            return Some(WriteRouting::Auto);
        }
        if let Some(encoded) = snapshot.strip_prefix("explicit-hex:") {
            if encoded.is_empty() {
                return None;
            }
            let relays = encoded
                .split(',')
                .map(|part| {
                    let bytes = hex::decode(part).ok()?;
                    let url = String::from_utf8(bytes).ok()?;
                    RelayUrl::parse(&url).ok()
                })
                .collect::<Option<Vec<_>>>()?;
            return Some(WriteRouting::Explicit(relays));
        }
        None
    }

    /// `publish()` refuses. Nothing durable exists and nothing ever will:
    /// there is no queue entry to inspect, nothing to retry and nothing to
    /// remove, so the caller learns it synchronously instead of being handed
    /// a receipt that will never say anything.
    pub(super) fn refuse_publish(&mut self, error: PublishError) -> Vec<Effect> {
        vec![Effect::PublishFailed(error)]
    }

    pub(super) fn fail_and_compensate(
        &mut self,
        id: ReceiptId,
        reason: String,
        effects: &mut Vec<Effect>,
    ) {
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };

        {
            let intent_id = pending.intent_id;
            match self.resolver.store_mut().compensate_write(intent_id) {
                Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                    // The store compensation already committed; reacting only
                    // re-reads to recompute the graph. A read failure here
                    // (issue #122) degrades to read-only rather than panics.
                    match self
                        .resolver
                        .react_to_compensation(pending.frozen.clone(), &outcome)
                    {
                        Ok(committed) => {
                            self.apply_committed_mutation(committed, effects);
                        }
                        Err(e) => self.degrade_store(e, effects),
                    }
                }
                Ok(CompensateOutcome::AlreadySigned | CompensateOutcome::NotFound) => {
                    // Promotion already made the row valid. Never retract a
                    // signed row; cancellation/signing errors arriving late
                    // cannot rewrite cache truth.
                    self.pending.insert(id, pending);
                    return;
                }
                Err(err) => {
                    // Compensation itself failed atomically. Keep the
                    // in-memory obligation so the caller can retry rather
                    // than losing ownership of a still-visible pending row.
                    // Crucially, do NOT emit terminal Failed: persistence
                    // did not commit the terminal transition, so claiming it
                    // did would contradict both the row and journal. U4 owns
                    // durable retry scheduling; a later explicit cancel or
                    // signer completion can re-enter this door.
                    self.pending.insert(id, pending);
                    let _persistence_error = err;
                    return;
                }
            }
        }

        // Reached only when compensation actually committed (a real,
        // permanent removal): both `NotFound`/`Err` arms above reinsert
        // `pending` untouched and return early, so the indexes must stay
        // untouched for those (epic #507 finding E5).
        self.forget_pending_indexes(id, &pending);
        effects.push(Effect::EmitReceipt(
            id,
            WriteFact::Signing(SigningState::Refused { reason }),
        ));
    }

    /// Execute a routing STRATEGY against what the engine knows right now.
    ///
    /// This runs on the SEND path — first attempt, boot recovery, engine
    /// tick, author-route replacement — never at compose time, so a write that parks
    /// while offline is resolved against the directory as it stands when it
    /// finally goes out, not as it stood when the app called `publish`.
    ///
    /// It is TOTAL: there is no error arm, because "the engine has not
    /// learned enough yet" is not an error — it is an `Auto` with unknowns,
    /// which is the normal INITIAL state of the queue rewriter
    /// (`docs/internals/routing/resolution-lifecycle.md` §8). A resolution
    /// that yields nothing yields `RouteAnswer::default()`, whose empty
    /// relay set and `complete == false` park the intent instead of killing
    /// it.
    ///
    /// `Auto` runs the built-in outbox derivation
    /// (`docs/internals/routing/outbox.md` §4): the event author's neutral
    /// outbound relays, operator app relays, and every tagged public key's
    /// neutral inbound relays. A settled `Absent` contributes nothing and
    /// blocks nothing; `Unknown` keeps the obligation live.
    ///
    /// `Explicit` never consults the directory at all: the answer is exactly
    /// the relays the caller named, nothing here adds to them, and it has no
    /// inputs and therefore no unknowns — the rewriter's fixed point,
    /// complete at its first resolution. That is ledger #6's fail-closed
    /// discipline, kept structurally rather than by convention. The empty
    /// case is unreachable from acceptance (`on_publish` refuses it at the
    /// door); it is spelled out here only so the fail-closed answer is the
    /// one this arm can give.
    pub(super) fn resolve_routes(
        &self,
        routing: &WriteRouting,
        event: &SignedEvent,
    ) -> RouteAnswer {
        match routing {
            WriteRouting::Auto => self.resolve_outbox(event),
            WriteRouting::Explicit(relays) => RouteAnswer {
                relays: relays.iter().cloned().collect(),
                // Verbatim execution reads nothing, so nothing can still be
                // unlearned. An accepted `Explicit` is complete the instant
                // it resolves, before any relay is contacted.
                author_route_needs: BTreeSet::new(),
                complete: !relays.is_empty(),
                detail: relays.is_empty().then(|| EMPTY_EXPLICIT_ROUTE.to_string()),
            },
        }
    }

    /// The built-in outbox resolver — what `Auto` falls back to when no
    /// registered strategy claims the kind (`docs/internals/routing/outbox.md`).
    fn resolve_outbox(&self, event: &SignedEvent) -> RouteAnswer {
        let mut answer = RouteAnswer::default();
        let mut thin_recipient = false;

        // 1. the author's own outbox. A write fans out to EVERY write relay
        //    its author has, and one relay of my own is a fact about where I
        //    publish rather than a deficit to repair — so the author's own
        //    thinness never arms the top-up below.
        let author = event.pubkey;
        self.contribute(&author, RouteDirection::Outbound, &mut answer);

        // 2. app relays: every kind, every author, always, additive, and
        //    never counted toward the coverage minimum.
        answer
            .relays
            .extend(self.routing_facts.operator_app_relays());

        // 3. each p-tagged recipient's INBOX (read relays, never write).
        let recipients = p_tagged_authors(event);
        for recipient in &recipients {
            // Only a SETTLED answer can be short: until a recipient's list is
            // looked up to completion nobody knows what their coverage is, and
            // topping up on ignorance would widen every route the first time
            // it runs and then never narrow it. An unknown keeps the
            // resolution open, and the top-up is decided again when it lands.
            if let Some(reach) = self.contribute(recipient, RouteDirection::Inbound, &mut answer) {
                thin_recipient |= reach < COVERAGE_MIN;
            }
        }

        // Operator fallback, adopted from the read path with the read path's
        // own suppression rule: applied only when a p-tagged RECIPIENT's
        // coverage falls under the 2-relay minimum AND no app relay is
        // configured (`app_relays` suppresses fallback entirely, without
        // itself counting as coverage). The failure this closes is a reply to
        // someone whose inbound route fact names exactly one relay reaching nowhere
        // else when that relay is down; the addressee is who the minimum is
        // about.
        if thin_recipient && self.routing_facts.operator_app_relays().is_empty() {
            answer
                .relays
                .extend(self.routing_facts.operator_fallback_relays());
        }

        if answer.relays.is_empty() {
            // `complete` is a statement about KNOWLEDGE EXHAUSTION, never
            // about delivery, and a zero-destination answer is exactly where
            // that distinction earns its keep.
            //
            // Some contributor still `Unknown` means we are STILL LOOKING.
            // Nothing accumulates in that state — another day of not knowing
            // is not more evidence than the first day — so the write parks
            // indefinitely, with no cap of any kind, and every contributing
            // author stays declared as a need because a later positive
            // replacement is the only thing that can unpark it.
            //
            // Every contributor settled, and between them they named nothing,
            // means we have FINISHED looking. There is nowhere to publish,
            // and saying so is a fact rather than a guess — the write
            // terminates as `WriteOutcome::NoDestination` instead of waiting
            // forever on knowledge that has already arrived and said no.
            // (Owner ruling on #1237/#1031; this reverses the older doctrine
            // that a zero-destination answer could never retire.)
            let still_learning = !answer.author_route_needs.is_empty();
            answer.detail = Some(self.no_destination_detail(&author, &recipients));
            answer.complete = !still_learning;
            if still_learning {
                answer.author_route_needs.insert(author);
                answer.author_route_needs.extend(recipients);
            }
            return answer;
        }

        answer.complete = answer.author_route_needs.is_empty();
        if !answer.complete {
            answer.detail = Some(unresolved_detail(&answer.author_route_needs));
        }
        answer
    }

    /// Fold one contributing author's three-valued answer into `answer`.
    ///
    /// `Some(n)` is a SETTLED answer reaching `n` relays (`Some(0)` for a
    /// definitive absence, and for a list that names nothing on the half this
    /// role wants); `None` is ignorance, which has also declared the need.
    /// The caller may only judge coverage against a settled answer — an
    /// unknown recipient has no coverage to be short of yet.
    fn contribute(
        &self,
        author: &PublicKey,
        direction: RouteDirection,
        answer: &mut RouteAnswer,
    ) -> Option<usize> {
        match self.routing_facts.author_routes(author) {
            AuthorRouteState::Present(routes) => {
                let relays = match direction {
                    RouteDirection::Outbound => routes.outbound(),
                    RouteDirection::Inbound => routes.inbound(),
                };
                answer.relays.extend(relays.iter().cloned());
                Some(relays.len())
            }
            // Settled: this input is RESOLVED, contributing nothing. It does
            // not block retirement -- that is exactly what makes the owner's
            // three-p-tag example reachable.
            AuthorRouteState::Absent => Some(0),
            // Not looked up to completion. Declare the need and keep the
            // obligation alive; the engine emits it as a neutral route need.
            AuthorRouteState::Unknown => {
                answer.author_route_needs.insert(*author);
                None
            }
        }
    }

    /// Why a resolution named no destination at all, in the terms that make it
    /// actionable: every exhausted source, and the operator sets that were
    /// empty.
    ///
    /// "Stuck" and "stuck because X" are different messages and only the
    /// second one can be acted on — and here every clause doubles as a way to
    /// fix it, because configuring any single one of them would have produced
    /// a route. It also keeps the two shapes of nothing apart: an author fact
    /// that a protocol coordinator settled as ABSENT is a final answer,
    /// while one nobody has finished looking up is merely young, and an operator reading
    /// "absent" knows waiting will not help.
    fn no_destination_detail(
        &self,
        author: &PublicKey,
        recipients: &BTreeSet<PublicKey>,
    ) -> String {
        let mut parts = vec![self.exhausted_source(author, RouteDirection::Outbound)];
        parts.extend(
            recipients
                .iter()
                .map(|recipient| self.exhausted_source(recipient, RouteDirection::Inbound)),
        );
        if self.routing_facts.operator_app_relays().is_empty() {
            parts.push("no app relays are configured".to_string());
        }
        if self.routing_facts.operator_fallback_relays().is_empty() {
            parts.push("no fallback relays are configured".to_string());
        }
        format!("no destination could be determined: {}", parts.join("; "))
    }

    /// One contributing author's clause of [`Self::no_destination_detail`].
    ///
    /// "Present but empty" has two completely different meanings, and telling
    /// a user with a LAN relay that they have no relays is the defect #1251
    /// exists to close. A list every one of whose relays was refused says so,
    /// names them, and names the config that would re-admit them; a list that
    /// really declared nothing keeps the old wording.
    fn exhausted_source(&self, author: &PublicKey, direction: RouteDirection) -> String {
        let state = self.routing_facts.author_routes(author);
        let author = author.to_hex();
        match state {
            AuthorRouteState::Present(routes) if routes.every_declared_relay_was_refused() => {
                let refused = routes
                    .refused()
                    .iter()
                    .map(RelayUrl::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "{author} declared relays but every one was refused ({refused});                      they are not this app's own and their hosts are neither in                      allowed_local_relay_hosts nor reachable under the declared                      Tor capability"
                )
            }
            AuthorRouteState::Present(_) => match direction {
                RouteDirection::Outbound => {
                    format!("Present outbound routes for {author} are empty")
                }
                RouteDirection::Inbound => {
                    format!("Present inbound routes for {author} are empty")
                }
            },
            AuthorRouteState::Absent => format!("author routes are Absent for {author}"),
            AuthorRouteState::Unknown => format!("author routes are Unknown for {author}"),
        }
    }

    /// Every public key the open intents still need neutral author routes for.
    ///
    /// Read straight off reducer memory (`route_needs`, refreshed by the
    /// last resolution of each intent), so a discovery pass costs no store
    /// read and N intents wanting the same author collapse to one entry by
    /// set union. An optional protocol assembly may turn this set into an
    /// exact query. Nothing here is recovered from the journal: a restart
    /// re-resolves every open intent and re-declares whatever is still
    /// needed, which is why declared needs have no durability story to lose.
    pub(super) fn author_route_needs(&self) -> BTreeSet<PublicKey> {
        self.pending
            .values()
            .filter(|pending| !pending.route_complete)
            .flat_map(|pending| pending.route_needs.iter().copied())
            .collect()
    }

    /// ONE resolution moment for ONE intent: re-execute the strategy against
    /// what the engine knows right now, diff against everything this intent
    /// has ever durably resolved to, append a revision for whatever is new,
    /// and mint lanes from it through the ordinary machinery.
    ///
    /// This is the queue rewriter (`resolution-lifecycle.md` §§1-4). It is
    /// deliberately safe to run at ANY frequency: an execution that learns
    /// nothing costs a directory read and an empty diff, and the
    /// `(intent_id, relay)` lane key makes a re-reported relay collide with
    /// the lane that already exists rather than mint a second delivery
    /// obligation. Correctness never depends on which moment fired.
    ///
    /// Retired intents (`route_complete`) are skipped outright, so an `Auto`
    /// with nothing left to learn costs nothing forever after.
    pub(super) fn rewrite_route(&mut self, id: ReceiptId, effects: &mut Vec<Effect>) {
        let Some(pending) = self.pending.get(&id) else {
            return;
        };
        // Unsigned intents have no frozen recipient set to resolve against
        // yet, an unreadable routing snapshot is never resolved at all, and a
        // retired route can never change its answer again.
        if !pending.routing_valid || pending.event_id.is_none() || pending.route_complete {
            return;
        }
        let intent_id = pending.intent_id;
        let answer = self.resolve_routes(&pending.routing, &pending.frozen);
        self.apply_route_answer(id, intent_id, answer, effects);
    }

    /// Commit one [`RouteAnswer`] against an intent's durable route log and
    /// tell the receipt whatever actually changed.
    ///
    /// Emission is picture-driven, not moment-driven: a `Routed` fact is
    /// pushed only when the relay set or the `complete` flag moved, and a
    /// park is re-emitted only when its REASON changes. So a tick that
    /// learns nothing is silent on the receipt stream even though the
    /// strategy really did re-execute.
    pub(super) fn apply_route_answer(
        &mut self,
        id: ReceiptId,
        intent_id: IntentId,
        answer: RouteAnswer,
        effects: &mut Vec<Effect>,
    ) {
        let Some(pending) = self.pending.get(&id) else {
            return;
        };
        let signing_pubkey = pending.signing_pubkey;
        // Diff-and-append: only relays absent from everything this intent has
        // ever durably resolved to are new, so an acked lane is never
        // re-minted and a resolver repeating itself writes nothing.
        let new_relays = answer
            .relays
            .difference(&pending.durable_routes)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut union = pending.durable_routes.clone();
        let mut blocked = BTreeSet::new();
        let mut committed = false;

        if !new_relays.is_empty() {
            if self
                .commit_route_revision(intent_id, answer.relays.clone())
                .is_err()
            {
                // The route itself did not persist: these exact URLs are not
                // claimed to survive a crash, so they are owned only for this
                // process and no lane is minted for them.
                blocked = new_relays.clone();
                if let Some(pending) = self.pending.get_mut(&id) {
                    pending
                        .route_blocked_relays
                        .extend(new_relays.iter().cloned());
                }
            } else {
                committed = true;
                union.extend(answer.relays.iter().cloned());
            }
        }

        let picture_changed = {
            let Some(pending) = self.pending.get_mut(&id) else {
                return;
            };
            let changed = !pending.destinations_reported
                || pending.durable_routes != union
                || pending.route_complete != answer.complete;
            pending.destinations_reported = true;
            pending.durable_routes = union.clone();
            pending.route_complete = answer.complete;
            pending.route_needs = answer.author_route_needs.clone();
            changed
        };

        // The receipt learns WHERE before it learns what each destination is
        // doing: the routing picture is emitted ahead of any lane fact, so an
        // app never sees a relay it was never told about.
        if union.is_empty() {
            // The two empty-destination situations, kept apart by the one
            // fact that distinguishes them (#1236 dissolves here).
            //
            // `RouteAnswer::complete` is a statement about KNOWLEDGE
            // EXHAUSTION, never about delivery. So:
            //
            // - `!complete` — still learning. PARK, indefinitely, with no
            //   cap of any kind. Nothing expires it, because a user who was
            //   merely offline must never lose a message NMP never proved
            //   undeliverable. It ends when knowledge is exhausted (becoming
            //   the case below), when a route appears, or when the app
            //   removes the entry.
            // - `complete` — knowledge IS exhausted and named zero relays.
            //   There is nowhere to publish, and saying so is a fact rather
            //   than a guess.
            // `picture_changed` is the ONE authority on whether this is news:
            // it was computed against the pending state BEFORE that state was
            // updated, so re-deriving it here would compare the new value
            // with itself and silently suppress every park (the defect the
            // BDD suite caught: nine scenarios saw a receipt containing only
            // `Signing(Signed)` and nothing else).
            if picture_changed {
                self.emit_write_fact(
                    id,
                    WriteFact::Destinations {
                        relays: BTreeSet::new(),
                        complete: answer.complete,
                    },
                    effects,
                );
                if answer.complete {
                    // Knowledge is exhausted and it named nobody. Terminal.
                    //
                    // The open-work row goes with it. This write owns no
                    // lanes at all, which is the exact precondition of
                    // `close_unroutable_intent` (the structural complement
                    // of `close_terminal_intent`'s non-empty all-terminal
                    // set), so the store can check the shape itself rather
                    // than being told a routing verdict. Leaving the row
                    // behind would strand the entry: the removal door
                    // refuses an open intent, cancellation refuses a signed
                    // one, and boot would replay it forever — on the FIRST
                    // publish of a fresh install with no reachable relay
                    // list, which is the commonest path there is.
                    //
                    // The RECEIPT is retained and reattachable either way,
                    // so the app can still read back what happened and why.
                    let closed = self
                        .resolver
                        .store_mut()
                        .close_unroutable_intent(intent_id)
                        .is_ok();
                    self.emit_write_fact(
                        id,
                        WriteFact::Outcome(WriteOutcome::NoDestination),
                        effects,
                    );
                    if closed {
                        if let Some(pending) = self.pending.remove(&id) {
                            self.forget_pending_indexes(id, &pending);
                        }
                    }
                }
            }
        } else {
            if picture_changed {
                self.emit_write_fact(
                    id,
                    WriteFact::Destinations {
                        relays: union.clone(),
                        complete: answer.complete,
                    },
                    effects,
                );
            }
        }

        for relay in blocked {
            self.emit_write_fact(
                id,
                WriteFact::Relay {
                    relay,
                    state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
                        detail: ROUTE_STALL_DETAIL.to_string(),
                    }),
                },
                effects,
            );
        }

        if committed {
            match self.bootstrap_projected_lanes(intent_id, Some(&union)) {
                Ok(lanes) => self.open_fresh_lanes(id, signing_pubkey, lanes, effects),
                Err(_) => {
                    // The sole call that teaches the reverse index this
                    // intent's lanes failed, so the index cannot learn what
                    // may or may not exist -- degrade rather than assume "no
                    // lanes" (epic #507 finding E5).
                    self.lane_relay_index_degraded = true;
                    for relay in &new_relays {
                        self.emit_write_fact(
                            id,
                            WriteFact::Relay {
                                relay: relay.clone(),
                                state: RelayState::Waiting(RelayWaiting::PersistenceStalled {
                                    detail: ATTEMPT_STALL_DETAIL.to_string(),
                                }),
                            },
                            effects,
                        );
                    }
                }
            }
        }
    }

    /// Moment 3/4 of the lifecycle (`resolution-lifecycle.md` §5): re-execute
    /// every intent whose routing is not yet complete.
    ///
    /// Called from the engine tick as a safety net and immediately after a
    /// private author-route replacement as the latency path. The two overlap
    /// by design; because resolution is diff-and-append, running "too often"
    /// is free.
    pub(super) fn rewrite_open_routes(&mut self, effects: &mut Vec<Effect>) {
        let open = self
            .pending
            .iter()
            .filter(|(_, pending)| {
                !pending.route_complete && pending.routing_valid && pending.event_id.is_some()
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if open.is_empty() {
            return;
        }
        let before = self.author_route_needs();
        for id in open {
            self.rewrite_route(id, effects);
            self.close_if_all_lanes_terminal(id, effects);
        }
        self.resync_route_needs(before, effects);
    }

    /// Publish a changed neutral author-route need set.
    ///
    /// A need is not a subscription. Optional protocol assembly reads this
    /// neutral set and owns any exact query it opens.
    pub(super) fn resync_route_needs(
        &mut self,
        before: BTreeSet<PublicKey>,
        effects: &mut Vec<Effect>,
    ) {
        let current = self.author_route_needs();
        if current != before {
            effects.push(Effect::AuthorRouteNeedsChanged(current));
        }
    }

    /// An `OK` frame resolves exactly one (event, relay) pair's pending
    /// ack. An `OK` for an event/relay this reducer isn't tracking (unknown
    /// event id, already-terminal receipt, duplicate OK, or an `Ephemeral`
    /// write that was already forgotten) is silently ignored — it is an
    /// untrusted-network fact, not a caller error.
    pub(super) fn handle_write_ack(
        &mut self,
        event_id: EventId,
        status: bool,
        message: String,
        session: &RelaySessionKey,
        effects: &mut Vec<Effect>,
    ) {
        let Some(ids) = self.event_to_receipts.get(&event_id).cloned() else {
            return;
        };
        let class = classify_relay_ack(status, &message);
        for id in ids {
            let Some(pending) = self.pending.get(&id) else {
                continue;
            };
            let intent_id = pending.intent_id;
            // An OK is only trusted from the exact session this pending write
            // publishes on (#8 U2: the intent's identity-scoped Nip42 write
            // session, frozen at acceptance). An ack arriving on any other
            // context's session for the same URL — including the Public read
            // session — must never advance this write lane.
            let expected_session = RelaySessionKey::new(
                session.relay.clone(),
                AccessContext::Nip42(pending.signing_pubkey),
            );
            if &expected_session != session {
                continue;
            }
            let relay = &session.relay;
            let key = PublishQueueLaneKey {
                intent_id,
                relay: relay.clone(),
            };
            let lane = self
                .resolver
                .store()
                .recover_publish_queue_lanes(intent_id)
                .ok()
                .and_then(|lanes| lanes.into_iter().find(|lane| lane.key == key));
            let Some(lane) = lane else {
                continue;
            };
            let PublishQueueLaneState::InFlight {
                ordinal,
                phase: PublishQueueInFlightPhase::AwaitingAck { .. },
            } = lane.state
            else {
                continue;
            };

            match &class {
                RelayAckClass::Acked => {
                    if self
                        .commit_lane_attempt_finish(
                            &key,
                            lane.revision,
                            ordinal,
                            PublishQueueAttemptOutcome::Acked,
                            self.clock,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_fact(
                            id,
                            WriteFact::Relay {
                                relay: relay.clone(),
                                state: RelayState::Published,
                            },
                            effects,
                        );
                        self.close_if_all_lanes_terminal(id, effects);
                    }
                }
                RelayAckClass::Rejected => {
                    if self
                        .commit_lane_attempt_finish(
                            &key,
                            lane.revision,
                            ordinal,
                            PublishQueueAttemptOutcome::Rejected(message.clone()),
                            self.clock,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_fact(
                            id,
                            WriteFact::Relay {
                                relay: relay.clone(),
                                state: RelayState::Rejected {
                                    reason: message.clone(),
                                },
                            },
                            effects,
                        );
                        self.close_if_all_lanes_terminal(id, effects);
                    }
                }
                RelayAckClass::Transient(cause) => {
                    let now = self.clock;
                    self.retry_or_give_up(
                        Some(id),
                        &key,
                        lane.revision,
                        ordinal,
                        now,
                        *cause,
                        message.clone(),
                        effects,
                    );
                }
                RelayAckClass::WaitingAuth => {
                    self.auth_probe_sessions.remove(session);
                    self.auth_required_sessions.insert(session.clone());
                    if self
                        .commit_lane_suspension(
                            &key,
                            lane.revision,
                            ordinal,
                            self.clock,
                            PublishQueueTransientCause::AuthRequired,
                            Some(message.clone()),
                            true,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_fact(
                            id,
                            WriteFact::Relay {
                                relay: relay.clone(),
                                state: RelayState::Waiting(RelayWaiting::NeedsAuth),
                            },
                            effects,
                        );
                    }
                }
            }
        }
        effects.extend(self.schedule_ready(self.clock));
    }
    pub(super) fn suspend_disconnected_lanes(
        &mut self,
        session: &RelaySessionKey,
        effects: &mut Vec<Effect>,
    ) {
        let Ok(lanes) = self.recover_all_lanes() else {
            self.retry_scheduler_blocked = true;
            return;
        };
        for (id, lane) in lanes {
            // Only lanes riding EXACTLY this session suspend (#8): a different
            // access context's session for the same URL did not drop. Since
            // the AUTH-reducer wave (#8 U2) write lanes ride the intent's
            // identity-scoped Nip42 session; a lane whose receipt has no
            // live pending entry is skipped.
            let Some(signing_pubkey) = self.pending.get(&id).map(|pending| pending.signing_pubkey)
            else {
                continue;
            };
            if RelaySessionKey::new(lane.key.relay.clone(), AccessContext::Nip42(signing_pubkey))
                != *session
            {
                continue;
            }
            let relay = &session.relay;
            match lane.state {
                PublishQueueLaneState::Eligible { .. } => {
                    if self
                        .commit_lane_waiting(&lane.key, lane.revision, false)
                        .is_ok()
                    {
                        self.emit_write_fact(
                            id,
                            WriteFact::Relay {
                                relay: relay.clone(),
                                state: RelayState::Waiting(RelayWaiting::NotConnected),
                            },
                            effects,
                        );
                    }
                }
                PublishQueueLaneState::InFlight {
                    ordinal,
                    phase: PublishQueueInFlightPhase::AwaitingAck { .. },
                } => {
                    let now = self.clock;
                    self.retry_or_give_up(
                        Some(id),
                        &lane.key,
                        lane.revision,
                        ordinal,
                        now,
                        PublishQueueTransientCause::ConnectionLost,
                        "connection lost while awaiting ACK".to_string(),
                        effects,
                    );
                }
                PublishQueueLaneState::WaitingAuth => {
                    // A `WaitingAuth` park is authenticated-generation-scoped:
                    // the relay demanded auth on THIS socket, and that grant
                    // (and any in-flight challenge) died with the disconnect.
                    // Fall the lane back to `WaitingConnection` so the ordinary
                    // reconnect wake (`wake_relay_lanes(.., auth_only=false)`)
                    // re-drives it — a fresh generation re-sends the event,
                    // re-provokes the challenge, re-parks, authenticates, and
                    // finally wakes via `finish_auth_ok`. Leaving it
                    // `WaitingAuth` here would strand it: the ONLY `WaitingAuth`
                    // wake is `finish_auth_ok`, which for a lazy-challenging
                    // relay never fires again without a client-provoked EVENT.
                    if self
                        .commit_lane_waiting(&lane.key, lane.revision, false)
                        .is_ok()
                    {
                        self.emit_write_fact(
                            id,
                            WriteFact::Relay {
                                relay: relay.clone(),
                                state: RelayState::Waiting(RelayWaiting::NotConnected),
                            },
                            effects,
                        );
                    }
                }
                PublishQueueLaneState::WaitingConnection
                | PublishQueueLaneState::Transient { .. }
                | PublishQueueLaneState::InFlight {
                    phase: PublishQueueInFlightPhase::AwaitingHandoff,
                    ..
                }
                | PublishQueueLaneState::Terminal { .. } => {}
            }
        }
    }
}
