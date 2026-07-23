//! Durable write, receipt, recovery, and retry lifecycle.
//!
//! This module owns acceptance through signing, route snapshots, per-relay
//! attempts and acknowledgements, cancellation/compensation, and boot recovery.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReceiptReplayFactKey {
    ReceiptStatus,
    AwaitingCapability,
    Attempt {
        relay: RelayUrl,
        key: ReceiptAttemptReplayKey,
    },
    Lane {
        relay: RelayUrl,
        revision: u64,
    },
    PersistenceBlocked(RelayUrl),
    RoutePersistenceBlocked(RelayUrl),
}

impl ReceiptReplayCursor {
    fn contains(&self, key: &ReceiptReplayFactKey, status: &WriteStatus) -> bool {
        match key {
            ReceiptReplayFactKey::ReceiptStatus => {
                self.state.receipt_status.as_ref() == Some(status)
            }
            ReceiptReplayFactKey::AwaitingCapability => self.state.awaiting_capability,
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
            ReceiptReplayFactKey::PersistenceBlocked(relay) => {
                self.state.persistence_blocked.contains(relay)
            }
            ReceiptReplayFactKey::RoutePersistenceBlocked(relay) => {
                self.state.route_persistence_blocked.contains(relay)
            }
        }
    }

    fn advance(&mut self, key: ReceiptReplayFactKey, status: WriteStatus) {
        match key {
            ReceiptReplayFactKey::ReceiptStatus => self.state.receipt_status = Some(status),
            ReceiptReplayFactKey::AwaitingCapability => self.state.awaiting_capability = true,
            ReceiptReplayFactKey::Attempt { relay, key } => {
                self.state.attempts.insert(relay, key);
            }
            ReceiptReplayFactKey::Lane { relay, revision } => {
                self.state.lane_revisions.insert(relay, revision);
            }
            ReceiptReplayFactKey::PersistenceBlocked(relay) => {
                self.state.persistence_blocked.insert(relay);
            }
            ReceiptReplayFactKey::RoutePersistenceBlocked(relay) => {
                self.state.route_persistence_blocked.insert(relay);
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
    /// exhaustion -- same discipline as [`Self::alloc_receipt_id`]'s
    /// `next_unaccepted_receipt` counter.
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
    /// `consume_due_outbox_deadlines`.
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
        if let Some(intent_id) = pending.intent_id {
            self.intent_receipts.remove(&intent_id);
        }
        for relay in &pending.lane_relays {
            if let Some(receipts) = self.receipts_by_lane_relay.get_mut(relay) {
                receipts.remove(&id);
                if receipts.is_empty() {
                    self.receipts_by_lane_relay.remove(relay);
                }
            }
        }
    }

    pub(super) fn emit_write_status(
        &mut self,
        id: ReceiptId,
        status: WriteStatus,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(pending) = self.pending.get_mut(&id) {
            Self::notify(pending, status.clone());
        }
        effects.push(Effect::EmitReceipt(id, status));
    }

    pub(super) fn remove_active_lane(&mut self, id: ReceiptId, relay: &RelayUrl) {
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.pending_relays.remove(relay);
            pending.attempt_ordinals.remove(relay);
        }
    }

    pub(super) fn close_if_all_lanes_terminal(&mut self, id: ReceiptId) {
        let Some((intent_id, event_id)) = self
            .pending
            .get(&id)
            .filter(|pending| pending.route_blocked_relays.is_empty())
            .and_then(|pending| Some((pending.intent_id?, pending.event_id)))
        else {
            return;
        };
        let Ok(lanes) = self.resolver.store().recover_outbox_lanes(intent_id) else {
            return;
        };
        if lanes.is_empty()
            || lanes
                .iter()
                .any(|lane| !matches!(lane.state, LaneState::Terminal { .. }))
        {
            return;
        }
        let Ok(CloseIntentOutcome::Closed | CloseIntentOutcome::AlreadyClosed) =
            self.resolver.store_mut().close_terminal_intent(intent_id)
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

        let key = LaneKey {
            intent_id,
            relay: target.session.relay.clone(),
        };
        let Ok(Some(lane)) = self
            .resolver
            .store()
            .recover_outbox_lanes(intent_id)
            .map(|lanes| lanes.into_iter().find(|lane| lane.key == key))
        else {
            return effects;
        };
        if !matches!(
            lane.state,
            LaneState::InFlight {
                ordinal: current,
                phase: InFlightPhase::AwaitingHandoff,
            } if current == ordinal
        ) {
            return effects;
        }

        let durability = self.pending.get(&target.receipt).map(|p| p.durability);
        let detail = AttemptHandoffDetail {
            at: self.clock,
            result: match result {
                HandoffResult::NotHandedOff => HandoffEvidence::NotHandedOff,
                HandoffResult::Written => HandoffEvidence::Written,
                HandoffResult::Ambiguous => HandoffEvidence::Ambiguous,
            },
        };
        let next = match (result, durability) {
            (HandoffResult::NotHandedOff, _) => PostHandoffState::WaitingConnection,
            (HandoffResult::Written, _) | (HandoffResult::Ambiguous, Some(Durability::Durable)) => {
                PostHandoffState::AwaitingAck {
                    deadline: self.clock + ACK_TIMEOUT_SECS,
                }
            }
            (HandoffResult::Ambiguous, Some(Durability::AtMostOnce)) => {
                PostHandoffState::Terminal {
                    outcome: AttemptOutcome::OutcomeUnknown,
                    finished_at: self.clock,
                }
            }
            (HandoffResult::Ambiguous, _) => return effects,
        };
        if self
            .resolver
            .store_mut()
            .record_lane_handoff(&key, lane.revision, ordinal, detail, next)
            .is_err()
        {
            return effects;
        }

        match (result, durability) {
            (HandoffResult::Written, _) => {
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::Sent {
                        relay: target.session.relay,
                        attempt: ordinal,
                        written_at: self.clock,
                    },
                    &mut effects,
                );
            }
            (HandoffResult::Ambiguous, Some(Durability::AtMostOnce)) => {
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::HandoffAmbiguous {
                        relay: target.session.relay.clone(),
                        attempt: ordinal,
                        observed_at: self.clock,
                    },
                    &mut effects,
                );
                self.remove_active_lane(target.receipt, &target.session.relay);
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::OutcomeUnknown(target.session.relay),
                    &mut effects,
                );
                self.close_if_all_lanes_terminal(target.receipt);
            }
            (HandoffResult::NotHandedOff, _) => {
                self.remove_active_lane(target.receipt, &target.session.relay);
                self.connected_relays.remove(&target.session);
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::AwaitingRelay {
                        relay: target.session.relay.clone(),
                    },
                    &mut effects,
                );
                effects.push(Effect::EnsureRelay(target.session));
            }
            (HandoffResult::Ambiguous, Some(Durability::Durable)) => {
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::HandoffAmbiguous {
                        relay: target.session.relay,
                        attempt: ordinal,
                        observed_at: self.clock,
                    },
                    &mut effects,
                );
            }
            (HandoffResult::Ambiguous, _) => {}
        }
        effects.extend(self.schedule_ready(self.clock));
        effects
    }

    /// Full O(pending) re-read of every outstanding write's lanes. This
    /// remains a deliberate architectural stance for `schedule_ready` (its
    /// caller below) and `required_relay_workers`, NOT an oversight (epic
    /// #507 finding E5): both compute durable-cap/attempt-ordinal
    /// accounting, which is defined over ALL outstanding lanes globally --
    /// there is no per-relay narrowing that preserves that meaning, so they
    /// are left unchanged here. `wake_relay_lanes` is the one caller this
    /// full scan was NOT inherent to (a single relay event only ever needs
    /// that relay's own lanes); it now goes through the narrower
    /// `receipts_by_lane_relay` index instead, except in the degraded
    /// fallback which still calls this exact function.
    pub(super) fn recover_all_lanes(
        &self,
    ) -> Result<Vec<(ReceiptId, RecoveredLane)>, PersistenceError> {
        let mut lanes = Vec::new();
        for (id, pending) in &self.pending {
            let Some(intent_id) = pending.intent_id else {
                continue;
            };
            lanes.extend(
                self.resolver
                    .store()
                    .recover_outbox_lanes(intent_id)?
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
                LaneState::InFlight { .. } | LaneState::LegacyInFlight { .. } => {
                    in_flight = in_flight.saturating_add(1);
                    in_flight_relays.insert(lane.key.relay.clone());
                }
                LaneState::Eligible { since } => eligible.push((since, id, lane)),
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
                    .resolver
                    .store_mut()
                    .set_lane_waiting(&lane.key, lane.revision, false)
                    .is_ok()
                {
                    self.emit_write_status(
                        id,
                        WriteStatus::AwaitingRelay {
                            relay: lane.key.relay.clone(),
                        },
                        &mut effects,
                    );
                    effects.push(Effect::EnsureRelay(session));
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
                    .resolver
                    .store_mut()
                    .set_lane_waiting(&lane.key, lane.revision, true)
                    .is_ok()
                {
                    self.emit_write_status(
                        id,
                        WriteStatus::AwaitingAuth {
                            relay: lane.key.relay.clone(),
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
            let (attempt, advanced) = match self.resolver.store_mut().start_lane_attempt(
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
                    self.emit_write_status(
                        id,
                        WriteStatus::PersistenceBlocked(lane.key.relay),
                        &mut effects,
                    );
                    continue;
                }
            };
            debug_assert_eq!(
                advanced.state,
                LaneState::InFlight {
                    ordinal: attempt.ordinal,
                    phase: InFlightPhase::AwaitingHandoff,
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
    /// intents. (`receipts_by_lane_relay`/`LaneKey` stay URL-keyed in the
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

        let mut lanes: Vec<(ReceiptId, RecoveredLane)> = Vec::new();
        for id in candidates {
            let Some(intent_id) = self.pending.get(&id).and_then(|pending| pending.intent_id)
            else {
                continue;
            };
            match self.resolver.store().recover_outbox_lanes(intent_id) {
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
        lanes: Vec<(ReceiptId, RecoveredLane)>,
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
                matches!(lane.state, LaneState::WaitingAuth)
            } else {
                matches!(lane.state, LaneState::WaitingConnection)
            };
            if !should_wake {
                continue;
            }
            if self
                .resolver
                .store_mut()
                .set_lane_eligible(&lane.key, lane.revision, self.clock)
                .is_err()
            {
                self.retry_scheduler_blocked = true;
            } else if lane.last_ordinal > 0 {
                self.emit_write_status(
                    id,
                    WriteStatus::RetryEligible {
                        relay: lane.key.relay,
                        attempt: lane.last_ordinal,
                        eligible_at: self.clock,
                    },
                    effects,
                );
            }
        }
    }

    pub(super) fn consume_due_outbox_deadlines(&mut self, now: Timestamp) -> Vec<Effect> {
        let mut effects = Vec::new();
        loop {
            let due = match self
                .resolver
                .store()
                .due_outbox_deadlines(now, DEADLINE_READ_BATCH)
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
                    .recover_outbox_lanes(deadline.key.intent_id)
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
                    (DeadlineKind::RetryEligible, LaneState::Transient { .. }) => {
                        if self
                            .resolver
                            .store_mut()
                            .set_lane_eligible(&lane.key, lane.revision, deadline.at)
                            .is_err()
                        {
                            self.retry_scheduler_blocked = true;
                        }
                    }
                    (
                        DeadlineKind::AckTimeout,
                        LaneState::InFlight {
                            ordinal,
                            phase: InFlightPhase::AwaitingAck { .. },
                        },
                    ) => {
                        let durability =
                            id.and_then(|id| self.pending.get(&id).map(|p| p.durability));
                        if durability == Some(Durability::AtMostOnce) {
                            if self
                                .resolver
                                .store_mut()
                                .finish_lane_attempt(
                                    &lane.key,
                                    lane.revision,
                                    ordinal,
                                    AttemptOutcome::OutcomeUnknown,
                                    now,
                                )
                                .is_ok()
                            {
                                if let Some(id) = id {
                                    self.remove_active_lane(id, &lane.key.relay);
                                    self.emit_write_status(
                                        id,
                                        WriteStatus::OutcomeUnknown(lane.key.relay.clone()),
                                        &mut effects,
                                    );
                                    self.close_if_all_lanes_terminal(id);
                                }
                            } else {
                                self.retry_scheduler_blocked = true;
                            }
                        } else {
                            let eligible_at = now + retry_delay_secs(&lane.key, ordinal);
                            if self
                                .resolver
                                .store_mut()
                                .set_lane_transient(
                                    &lane.key,
                                    lane.revision,
                                    ordinal,
                                    eligible_at,
                                    TransientCause::AckTimeout,
                                    Some("ack timeout".to_string()),
                                )
                                .is_ok()
                            {
                                if let Some(id) = id {
                                    self.remove_active_lane(id, &lane.key.relay);
                                    self.emit_write_status(
                                        id,
                                        WriteStatus::RetryEligible {
                                            relay: lane.key.relay.clone(),
                                            attempt: ordinal,
                                            eligible_at,
                                        },
                                        &mut effects,
                                    );
                                }
                            } else {
                                self.retry_scheduler_blocked = true;
                            }
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
        let recovered = self.resolver.store().recover_outbox();
        let mut effects = Vec::new();
        let mut recovered_ids = Vec::new();
        // This is the one deterministic, from-scratch rebuild of `pending`
        // (and, with it, every index derived from `pending`) -- the exact
        // moment `receipts_by_lane_relay` can be trusted again regardless of
        // what happened in a prior process (epic #507 finding E5).
        self.lane_relay_index_degraded = false;

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
                effects.push(Effect::EmitReceipt(id, WriteStatus::Failed(reason)));
                continue;
            }
            let parsed_routing = Self::parse_routing_snapshot(&intent.routing);
            let routing_valid = parsed_routing.is_some();
            let routing = parsed_routing.unwrap_or_else(|| {
                WriteRouting::PrivateNarrow(PrivateRoute {
                    relays: NarrowOnly::new(Vec::<RelayUrl>::new()),
                })
            });
            let id = ReceiptId(intent.receipt_id);
            let durability = match intent.durability {
                WriteDurability::Durable => Durability::Durable,
                WriteDurability::AtMostOnce => Durability::AtMostOnce,
            };
            let already_signed = intent.sig_state == IntentSigState::Signed;
            self.pending.insert(
                id,
                PendingWrite {
                    durability,
                    routing,
                    routing_valid,
                    sinks: Vec::new(),
                    intent_id: Some(intent.intent_id),
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
                    lane_relays: BTreeSet::new(),
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
                    // (as below) means `bootstrap_outbox_lanes` never runs
                    // for it this boot, so the reverse index can never learn
                    // those lanes -- an unprovable gap, so degrade rather
                    // than silently under-index (epic #507 finding E5).
                    self.lane_relay_index_degraded = true;
                    continue;
                }
            };
            let durable_relays = revisions
                .iter()
                .flat_map(|revision| revision.relays.iter().cloned())
                .collect::<BTreeSet<_>>();

            if routing_valid {
                let current_routes = self
                    .resolve_routes(&self.pending[&id].routing, &intent.frozen.pubkey.to_hex())
                    .unwrap_or_default();
                let new_routes = current_routes
                    .difference(&durable_relays)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if !new_routes.is_empty()
                    && self
                        .resolver
                        .store_mut()
                        .record_route_revision(intent.intent_id, current_routes)
                        .is_err()
                {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.route_blocked_relays.extend(new_routes);
                    }
                }
            }

            let lanes = match self
                .resolver
                .store_mut()
                .bootstrap_outbox_lanes(intent.intent_id)
            {
                Ok(lanes) => lanes,
                Err(_) => {
                    // Same reasoning as the `recover_route_revisions` error
                    // above: this is the sole call that teaches the reverse
                    // index this intent's lanes, so a failure here is an
                    // audit hole, not a "no lanes" fact -- degrade rather
                    // than guess (epic #507 finding E5).
                    self.lane_relay_index_degraded = true;
                    continue;
                }
            };
            for lane in lanes {
                let relay = lane.key.relay.clone();
                // The recovered write lane's worker demand is the intent's
                // identity-scoped authenticated session (#8 U2); recovery
                // redials exactly the session the lane will publish on. The
                // signing identity was frozen at acceptance
                // (`intent.expected_pubkey`), never re-read from the mutable
                // active account.
                let session = RelaySessionKey::new(
                    lane.key.relay.clone(),
                    AccessContext::Nip42(intent.expected_pubkey),
                );
                if let Some(pending) = self.pending.get_mut(&id) {
                    if pending.lane_relays.insert(relay.clone()) {
                        self.receipts_by_lane_relay
                            .entry(relay)
                            .or_default()
                            .insert(id);
                    }
                }
                match lane.state {
                    LaneState::LegacyInFlight { ordinal }
                    | LaneState::InFlight {
                        ordinal,
                        phase: InFlightPhase::AwaitingHandoff,
                    } => match durability {
                        Durability::Durable => {
                            let eligible_at = self.clock;
                            let _ = self.resolver.store_mut().set_lane_transient(
                                &lane.key,
                                lane.revision,
                                ordinal,
                                eligible_at,
                                TransientCause::Interrupted,
                                Some("process restarted before handoff resolved".to_string()),
                            );
                        }
                        Durability::AtMostOnce => {
                            if self
                                .resolver
                                .store_mut()
                                .finish_lane_attempt(
                                    &lane.key,
                                    lane.revision,
                                    ordinal,
                                    AttemptOutcome::OutcomeUnknown,
                                    self.clock,
                                )
                                .is_ok()
                            {
                                effects.push(Effect::EmitReceipt(
                                    id,
                                    WriteStatus::OutcomeUnknown(lane.key.relay),
                                ));
                            }
                        }
                        Durability::Ephemeral => unreachable!(),
                    },
                    LaneState::WaitingConnection
                    | LaneState::Eligible { .. }
                    | LaneState::Transient { .. } => {
                        effects.push(Effect::EnsureRelay(session));
                    }
                    LaneState::InFlight {
                        phase: InFlightPhase::AwaitingAck { .. },
                        ..
                    } => {
                        effects.push(Effect::EnsureRelay(session));
                    }
                    LaneState::WaitingAuth => {
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
                            .resolver
                            .store_mut()
                            .set_lane_waiting(&lane.key, lane.revision, false)
                            .is_ok()
                        {
                            effects.push(Effect::EnsureRelay(session));
                        } else {
                            self.lane_relay_index_degraded = true;
                        }
                    }
                    LaneState::Terminal { .. } => {}
                }
            }
        }

        self.retry_scheduler_blocked = false;
        effects.extend(self.consume_due_outbox_deadlines(self.clock));
        for id in recovered_ids {
            self.close_if_all_lanes_terminal(id);
        }
        effects
    }
    /// its retained facts. Unknown ids do not create state.
    pub(super) fn retained_receipt_status(receipt: &nmp_store::RecoveredReceipt) -> WriteStatus {
        match receipt.state {
            ReceiptState::Accepted => WriteStatus::Accepted,
            ReceiptState::Signed => WriteStatus::Signed(receipt.frozen_id),
            ReceiptState::Compensated => WriteStatus::Failed("write compensated".to_string()),
            ReceiptState::Cancelled => WriteStatus::Cancelled,
            ReceiptState::Abandoned => {
                WriteStatus::Failed("ephemeral write abandoned after restart".to_string())
            }
        }
    }

    pub fn reattach_receipt(
        &mut self,
        id: ReceiptId,
        sink: Box<dyn ReceiptSink>,
    ) -> ReattachOutcome {
        self.reattach_receipt_page(id, sink, None, usize::MAX).0
    }

    /// Reconstruct one finite page of a receipt's durable prefix.
    ///
    /// The opaque cursor records fact identity independently for each relay
    /// lane, so a newly persisted fact on an earlier-sorted relay cannot
    /// shift another relay's continuation. Only the final page attaches the
    /// sink to live work, atomically with observing that no durable fact is
    /// currently unseen.
    pub fn reattach_receipt_page(
        &mut self,
        id: ReceiptId,
        sink: Box<dyn ReceiptSink>,
        cursor: Option<ReceiptReplayCursor>,
        limit: usize,
    ) -> (ReattachOutcome, Option<ReceiptReplayCursor>) {
        self.reattach_receipt_page_registered(id, sink, cursor, limit, None)
    }

    pub(crate) fn reattach_receipt_page_registered(
        &mut self,
        id: ReceiptId,
        sink: Box<dyn ReceiptSink>,
        cursor: Option<ReceiptReplayCursor>,
        limit: usize,
        registration: Option<ReceiptSinkRegistration>,
    ) -> (ReattachOutcome, Option<ReceiptReplayCursor>) {
        let mut cursor = match cursor {
            Some(cursor) if cursor.state.receipt_id == id => cursor,
            Some(_) => return (ReattachOutcome::RetainedButUnreadable, None),
            None => ReceiptReplayCursor::new(id),
        };
        if self.quarantined_auth_receipts.contains_key(&id) {
            return (ReattachOutcome::RetainedButUnreadable, None);
        }
        let receipt = match self.resolver.store().reattach_receipt(id.0) {
            Ok(Some(receipt)) => receipt,
            Ok(None) => return (ReattachOutcome::NotFound, None),
            Err(_) => return (ReattachOutcome::RetainedButUnreadable, None),
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
            return (ReattachOutcome::RetainedButUnreadable, None);
        }
        let (attempts, details, lanes) = match receipt.intent_id {
            Some(intent_id) => {
                let attempts = match self.resolver.store().recover_attempts(intent_id) {
                    Ok(attempts) => attempts,
                    Err(_) => return (ReattachOutcome::RetainedButUnreadable, None),
                };
                let details = match self.resolver.store().recover_attempt_details(intent_id) {
                    Ok(details) => details,
                    Err(_) => return (ReattachOutcome::RetainedButUnreadable, None),
                };
                let lanes = match self.resolver.store().recover_outbox_lanes(intent_id) {
                    Ok(lanes) => lanes,
                    Err(_) => return (ReattachOutcome::RetainedButUnreadable, None),
                };
                if self
                    .resolver
                    .store()
                    .recover_route_revisions(intent_id)
                    .is_err()
                {
                    return (ReattachOutcome::RetainedButUnreadable, None);
                }
                (attempts, details, lanes)
            }
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let status = Self::retained_receipt_status(&receipt);
        let mut replay = vec![(ReceiptReplayFactKey::ReceiptStatus, status)];
        if receipt.state == ReceiptState::Accepted
            && self
                .pending
                .get(&id)
                .is_some_and(|pending| !pending.already_signed)
        {
            replay.push((
                ReceiptReplayFactKey::AwaitingCapability,
                WriteStatus::AwaitingCapability {
                    pubkey: receipt.expected_pubkey,
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
                                    WriteStatus::AwaitingRelay {
                                        relay: attempt.relay.clone(),
                                    },
                                ));
                            }
                            HandoffEvidence::Written => replay.push((
                                replay_key(ReceiptAttemptReplayPhase::Handoff),
                                WriteStatus::Sent {
                                    relay: attempt.relay.clone(),
                                    attempt: attempt.ordinal,
                                    written_at: handoff.at,
                                },
                            )),
                            HandoffEvidence::Ambiguous => {
                                replay.push((
                                    replay_key(ReceiptAttemptReplayPhase::Handoff),
                                    WriteStatus::HandoffAmbiguous {
                                        relay: attempt.relay.clone(),
                                        attempt: attempt.ordinal,
                                        observed_at: handoff.at,
                                    },
                                ));
                            }
                        }
                    }
                    if let Some(transient) = detail.transient {
                        if transient.cause == TransientCause::AuthRequired {
                            awaiting_auth.insert((attempt.relay.clone(), attempt.ordinal));
                            replay.push((
                                replay_key(ReceiptAttemptReplayPhase::Transient),
                                WriteStatus::AwaitingAuth {
                                    relay: attempt.relay.clone(),
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
                                WriteStatus::RetryEligible {
                                    relay: attempt.relay.clone(),
                                    attempt: attempt.ordinal,
                                    eligible_at: transient.eligible_at,
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
                    AttemptOutcome::Started => continue,
                    AttemptOutcome::Acked => WriteStatus::Acked(attempt.relay),
                    AttemptOutcome::Rejected(reason) => {
                        WriteStatus::Rejected(attempt.relay, reason)
                    }
                    AttemptOutcome::GaveUp => WriteStatus::GaveUp(attempt.relay),
                    AttemptOutcome::OutcomeUnknown => WriteStatus::OutcomeUnknown(attempt.relay),
                };
                replay.push((replay_key(ReceiptAttemptReplayPhase::Outcome), status));
            }
            if !details_by_attempt.is_empty() {
                return (ReattachOutcome::RetainedButUnreadable, None);
            }
            for lane in lanes {
                let replay_key = ReceiptReplayFactKey::Lane {
                    relay: lane.key.relay.clone(),
                    revision: lane.revision,
                };
                match lane.state {
                    LaneState::WaitingConnection
                        if !awaiting_relay
                            .contains(&(lane.key.relay.clone(), lane.last_ordinal)) =>
                    {
                        replay.push((
                            replay_key,
                            WriteStatus::AwaitingRelay {
                                relay: lane.key.relay,
                            },
                        ));
                    }
                    LaneState::WaitingAuth
                        if !awaiting_auth
                            .contains(&(lane.key.relay.clone(), lane.last_ordinal)) =>
                    {
                        replay.push((
                            replay_key,
                            WriteStatus::AwaitingAuth {
                                relay: lane.key.relay,
                            },
                        ));
                    }
                    LaneState::Eligible { since }
                        if lane.last_ordinal > 0
                            && !retry_eligible.contains(&(
                                lane.key.relay.clone(),
                                lane.last_ordinal,
                                since,
                            )) =>
                    {
                        replay.push((
                            replay_key,
                            WriteStatus::RetryEligible {
                                relay: lane.key.relay,
                                attempt: lane.last_ordinal,
                                eligible_at: since,
                            },
                        ));
                    }
                    LaneState::Transient {
                        ordinal,
                        eligible_at,
                        cause,
                        ..
                    } if cause != TransientCause::AuthRequired
                        && !retry_eligible.contains(&(
                            lane.key.relay.clone(),
                            ordinal,
                            eligible_at,
                        )) =>
                    {
                        replay.push((
                            replay_key,
                            WriteStatus::RetryEligible {
                                relay: lane.key.relay,
                                attempt: ordinal,
                                eligible_at,
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
                    ReceiptReplayFactKey::PersistenceBlocked(relay.clone()),
                    WriteStatus::PersistenceBlocked(relay.clone()),
                ));
            }
            for relay in &pending.route_blocked_relays {
                replay.push((
                    ReceiptReplayFactKey::RoutePersistenceBlocked(relay.clone()),
                    WriteStatus::RoutePersistenceBlocked(relay.clone()),
                ));
            }
        }
        if limit == 0 {
            return (ReattachOutcome::RetainedButUnreadable, None);
        }
        let mut live = true;
        let mut delivered = 0usize;
        let page = replay
            .iter()
            .filter(|(key, status)| !cursor.contains(key, status))
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        for (key, status) in page {
            if !sink.on_status(status.clone()) {
                live = false;
                break;
            }
            cursor.advance(key, status);
            delivered += 1;
        }

        // Re-check the complete current evidence against the advanced,
        // identity-stable cursor. This detects unseen facts on every relay,
        // including facts that sort before a different relay's prior page.
        // The cursor acknowledges only sink-accepted facts.
        let unseen = replay
            .iter()
            .any(|(key, status)| !cursor.contains(key, status));
        let page_full = delivered == limit;
        let next_cursor = (unseen || !live || page_full).then_some(cursor);
        if live && !unseen && !page_full {
            if let Some(pending) = self.pending.get_mut(&id) {
                pending.sinks.push(RegisteredReceiptSink {
                    registration,
                    sink: Rc::from(sink),
                });
            }
        }
        (ReattachOutcome::Attached, next_cursor)
    }

    pub(crate) fn register_initial_receipt_sink(
        &mut self,
        id: ReceiptId,
        registration: ReceiptSinkRegistration,
    ) -> bool {
        let Some(pending) = self.pending.get_mut(&id) else {
            return false;
        };
        let Some(sink) = pending
            .sinks
            .iter_mut()
            .find(|sink| sink.registration.is_none())
        else {
            return false;
        };
        sink.registration = Some(registration);
        true
    }

    pub(crate) fn detach_receipt_sink(
        &mut self,
        id: ReceiptId,
        registration: &ReceiptSinkRegistration,
    ) {
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.sinks.retain(|sink| {
                sink.registration
                    .as_ref()
                    .is_none_or(|candidate| !candidate.is_same(registration))
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn receipt_sink_count(&self, id: ReceiptId) -> usize {
        self.pending
            .get(&id)
            .map_or(0, |pending| pending.sinks.len())
    }

    /// #591: recover a receipt id from a caller-generated correlation token
    /// -- the door a client uses after a crash that happened BEFORE it
    /// could durably record the `Receipt.id` `publish_tracked` returned.
    /// A resolved token is translated to its receipt id and handed straight
    /// to [`Self::reattach_receipt`], reusing its EXACT replay/attach
    /// behavior unchanged: no new outcome enum, no separate machinery. The
    /// resolved [`ReceiptId`] is returned alongside the outcome (`Some` iff
    /// `Attached`) purely so the caller -- who by construction does NOT
    /// already know it, unlike a plain [`Self::reattach_receipt`] caller --
    /// can learn it.
    pub fn reattach_by_correlation(
        &mut self,
        token: String,
        sink: Box<dyn ReceiptSink>,
    ) -> (ReattachOutcome, Option<ReceiptId>) {
        let (outcome, id, _) = self.reattach_by_correlation_page(token, sink, None, usize::MAX);
        (outcome, id)
    }

    pub fn reattach_by_correlation_page(
        &mut self,
        token: String,
        sink: Box<dyn ReceiptSink>,
        cursor: Option<ReceiptReplayCursor>,
        limit: usize,
    ) -> (
        ReattachOutcome,
        Option<ReceiptId>,
        Option<ReceiptReplayCursor>,
    ) {
        self.reattach_by_correlation_page_registered(token, sink, cursor, limit, None)
    }

    pub(crate) fn reattach_by_correlation_page_registered(
        &mut self,
        token: String,
        sink: Box<dyn ReceiptSink>,
        cursor: Option<ReceiptReplayCursor>,
        limit: usize,
        registration: Option<ReceiptSinkRegistration>,
    ) -> (
        ReattachOutcome,
        Option<ReceiptId>,
        Option<ReceiptReplayCursor>,
    ) {
        match self.resolver.store().lookup_correlation(&token) {
            Ok(Some(receipt_id)) => {
                let id = ReceiptId(receipt_id);
                let (outcome, next_cursor) =
                    self.reattach_receipt_page_registered(id, sink, cursor, limit, registration);
                (outcome, Some(id), next_cursor)
            }
            Ok(None) => (ReattachOutcome::NotFound, None, None),
            Err(_) => (ReattachOutcome::RetainedButUnreadable, None, None),
        }
    }

    // ---- write outbox (D: intent -> signed -> routed -> sent -> acked) --

    /// `Publish` (issues #2/#3 U3): enter durable/at-most-once writes through
    /// `resolver.accept_local` exactly once. The store allocates both ids
    /// and commits the canonical pending row, obligation and receipt before
    /// `Accepted` is observable. Ephemeral uses the distinct receipt-only
    /// door: no pending row and no retry obligation, but still a stable,
    /// reattachable receipt as required by the promoted VISION.
    ///
    /// A `Signed` payload is verified here, at the acceptance boundary,
    /// BEFORE `WriteStatus::Accepted` is ever emitted (#52 Q2). This is the
    /// only publish path in the crate — `Handle::publish` is the sole entry
    /// point regardless of caller (FFI, direct-Rust, `nmp-bdd`'s
    /// `EngineThread`) — so verifying here, rather than at each caller,
    /// makes "a forged `Signed` event can never be published" true
    /// unconditionally instead of entry-point-dependent. A failed verify is
    /// a whole-intent terminal (`WriteStatus::Failed`): no `Accepted`, no
    /// pending write recorded, no `Effect::PublishEvent`.
    ///
    /// Identity resolution (#47): with `identity_override: None` the
    /// single-identity contract holds verbatim — an unsigned draft must be
    /// authored by the CURRENT active account, else fail closed
    /// pre-acceptance. With `Some(pk)` the caller explicitly consents to
    /// publish this one write as `pk`: `pk` must EQUAL the draft's author
    /// (the reducer never restamps a draft; a mismatch fails closed with no
    /// `Accepted`), and when it does the write is accepted with
    /// `signing_pubkey = pk` regardless of the active account — including
    /// while logged out. Acceptance pins `pk` (`expected_pubkey` /
    /// `signing_identity_ref`), so everything downstream — the frozen body,
    /// `RequestSign`, the `SignerAttached` re-arm, restart replay — targets
    /// the override identity forever; a later `set_active_account` cannot
    /// retarget it, and an override with no registered capability parks
    /// durably as `AwaitingCapability` rather than failing or drifting.
    pub(super) fn on_publish(
        &mut self,
        intent: WriteIntent,
        sink: Box<dyn ReceiptSink>,
    ) -> Vec<Effect> {
        let WriteIntent {
            payload,
            durability,
            routing,
            identity_override,
            correlation,
        } = intent;

        // #591 review (PR #604 finding 2): `Durability::Ephemeral` has no
        // outbox row for a correlation token to name -- `accept_ephemeral`
        // below never receives `correlation`, so a token on an ephemeral
        // write would otherwise be silently dropped (never journaled, no
        // later `reattach_by_correlation` could ever find it) WHILE the
        // pre-lookup just below still ran unconditionally, so an ephemeral
        // publish reusing a token that earlier named a DURABLE receipt
        // would silently reattach that unrelated durable obligation instead
        // of ever creating the ephemeral write the caller asked for. Refuse
        // closed instead of accepting either silent behavior.
        if durability == Durability::Ephemeral && correlation.is_some() {
            return self.fail_unaccepted(
                sink,
                "ephemeral writes cannot carry a correlation token: there is no durable outbox \
                 row for the token to name, and reusing an earlier durable token here would \
                 silently reattach that unrelated obligation instead of accepting this write"
                    .to_string(),
            );
        }

        // #591: a token that already resolves to a previously-accepted
        // receipt REATTACHES that existing obligation -- this call enqueues
        // no second write, and `payload`/`durability`/`routing`/
        // `identity_override` above are discarded entirely without so much
        // as a body comparison (a legitimately re-composed draft with a
        // fresh `created_at` is the exact scenario the token exists for).
        // The lookup runs inside this single-threaded reducer step, before
        // any store mutation for THIS call -- TOCTOU-free by construction
        // (no concurrent `&mut self` call can be interleaved).
        if let Some(token) = &correlation {
            match self.resolver.store().lookup_correlation(token.as_ref()) {
                Ok(Some(existing_receipt_id)) => {
                    let receipt_id = ReceiptId(existing_receipt_id);
                    let outcome = self.reattach_receipt(receipt_id, sink);
                    // `reattach_receipt` already fed `sink` synchronously
                    // when `outcome` is `Attached` (or dropped it, closing
                    // the caller's stream with zero statuses, for the same
                    // rare corrupt/quarantined cases the ordinary by-id
                    // reattach door tolerates -- the sink is gone by this
                    // point either way, so this call cannot redeliver
                    // anything onto it). Review (#591, PR #604 finding 1):
                    // an unconditional `Accepted`-shaped effect here
                    // previously masked a `RetainedButUnreadable`/
                    // `NotFound` outcome behind a synchronous "Accepted"
                    // reply -- surface the typed outcome in the effect
                    // itself instead, so effect-level inspection (tests,
                    // diagnostics) sees the truth rather than a fabricated
                    // success. A caller that needs to keep distinguishing
                    // "retained but unreadable" from "not found" after this
                    // stream goes silent still has `reattach_by_correlation`
                    // as the door that reports the outcome honestly.
                    let status = match outcome {
                        ReattachOutcome::Attached => WriteStatus::Accepted,
                        ReattachOutcome::NotFound => WriteStatus::Failed(
                            "correlation token resolved to a receipt id the store can no longer find"
                                .to_string(),
                        ),
                        ReattachOutcome::RetainedButUnreadable => WriteStatus::Failed(
                            "correlation token resolved to a retained but unreadable receipt"
                                .to_string(),
                        ),
                    };
                    // This effect exists only so `publish_result` can
                    // extract the EXISTING receipt id for the synchronous
                    // `publish_tracked` reply -- `publish_result` matches
                    // any `EmitReceipt` regardless of status.
                    return vec![Effect::EmitReceipt(receipt_id, status)];
                }
                Ok(None) => {}
                Err(err) => return self.fail_unaccepted(sink, err.to_string()),
            }
        }

        let replaceable_base = match &payload {
            WritePayload::UnsignedReplaceableEdit { expected_base, .. } => Some(*expected_base),
            WritePayload::Unsigned(_) | WritePayload::Signed(_) => None,
        };

        let payload_kind = match &payload {
            WritePayload::Unsigned(unsigned)
            | WritePayload::UnsignedReplaceableEdit { unsigned, .. } => unsigned.kind,
            WritePayload::Signed(event) => event.kind,
        };
        if payload_kind == nostr::Kind::Authentication {
            return self.fail_unaccepted(
                sink,
                "kind:22242 is reserved for reducer-owned relay authentication".to_string(),
            );
        }

        if replaceable_base.is_some() && durability == Durability::Ephemeral {
            return self.fail_unaccepted(
                sink,
                "replaceable edits require durable or at-most-once acceptance".to_string(),
            );
        }

        let signing_pubkey = match &payload {
            WritePayload::Unsigned(unsigned)
            | WritePayload::UnsignedReplaceableEdit { unsigned, .. } => match identity_override {
                // #47: explicit per-write consent to publish as `pk`. The
                // override must equal the draft's author — the reducer never
                // restamps a draft to match it — and once it does, the
                // active account is irrelevant (even logged out): acceptance
                // pins `pk` and downstream signing targets it forever.
                Some(pk) if pk == unsigned.pubkey => pk,
                Some(pk) => {
                    return self.fail_unaccepted(
                        sink,
                        format!(
                            "identity override {pk} does not match the unsigned draft author {}",
                            unsigned.pubkey
                        ),
                    );
                }
                // Default single-identity contract, unchanged: the draft's
                // author must be the CURRENT active account, fail closed
                // otherwise.
                None => match self.active_pubkey {
                    Some(active) if active == unsigned.pubkey => active,
                    Some(_) => {
                        return self.fail_unaccepted(
                            sink,
                            "unsigned draft author does not match current active account"
                                .to_string(),
                        );
                    }
                    None => {
                        return self.fail_unaccepted(
                            sink,
                            "unsigned publish requires an active account".to_string(),
                        );
                    }
                },
            },
            // Already-signed payloads are verified verbatim and never ask a
            // local signer, so their author is intrinsically frozen. An
            // explicit override may still name that author (a harmless
            // restatement) — but naming anyone ELSE is a consent/author
            // contradiction and fails closed before acceptance (#47).
            WritePayload::Signed(event) => match identity_override {
                Some(pk) if pk != event.pubkey => {
                    return self.fail_unaccepted(
                        sink,
                        format!(
                            "identity override {pk} does not match the signed event author {}",
                            event.pubkey
                        ),
                    );
                }
                _ => event.pubkey,
            },
        };

        if let WritePayload::Signed(event) = &payload {
            if let Err(err) = event.verify() {
                return self.fail_unaccepted(sink, err.to_string());
            }
        }

        let frozen = match Self::freeze_payload(&payload) {
            Ok(frozen) => frozen,
            Err(reason) => return self.fail_unaccepted(sink, reason),
        };

        let (id, intent_id, already_signed, accepted_signed_event, committed) = if durability
            == Durability::Ephemeral
        {
            match self
                .resolver
                .store_mut()
                .accept_ephemeral(frozen.id, signing_pubkey)
            {
                Ok(receipt_id) => (ReceiptId(receipt_id), None, false, None, None),
                Err(err) => return self.fail_unaccepted(sink, err.to_string()),
            }
        } else {
            let store_durability = match durability {
                Durability::Durable => WriteDurability::Durable,
                Durability::AtMostOnce => WriteDurability::AtMostOnce,
                Durability::Ephemeral => unreachable!("handled above"),
            };
            let accept = AcceptWrite {
                frozen: frozen.clone(),
                replaceable_base,
                expected_pubkey: signing_pubkey,
                signing_identity_ref: signing_pubkey.to_hex(),
                durability: store_durability,
                routing: Self::routing_snapshot(&routing),
                // Treat an unsigned acceptance as reattachable signer work.
                // If a signer is already present the immediate request below
                // promotes it; if not, restart safely re-requests it.
                sig_state: match payload {
                    WritePayload::Unsigned(_) | WritePayload::UnsignedReplaceableEdit { .. } => {
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
                Err(err) => return self.fail_unaccepted(sink, err.to_string()),
            };
            let Some(intent_id) = outcome.journaled_intent_id() else {
                let AcceptOutcome::Refused(reason) = outcome else {
                    unreachable!("only Refused omits journal ids")
                };
                return match reason {
                    nmp_store::RefuseReason::ReplaceableBaseChanged { expected, actual } => self
                        .fail_unaccepted_with_status(
                            sink,
                            WriteStatus::ReplaceableConflict { expected, actual },
                        ),
                    other => self.fail_unaccepted(sink, format!("write refused: {other:?}")),
                };
            };
            let receipt_id = outcome
                .journaled_receipt_id()
                .expect("journaled intent always has a receipt id");
            let accepted_signed_event = match &outcome {
                AcceptOutcome::Duplicate { row, .. } if row.event.sig != sentinel_signature() => {
                    Some(row.event.clone())
                }
                _ => None,
            };
            (
                ReceiptId(receipt_id),
                Some(intent_id),
                accepted_signed_event.is_some(),
                accepted_signed_event,
                Some(committed),
            )
        };

        let mut effects = Vec::new();
        let sink = Rc::<dyn ReceiptSink>::from(sink);
        let sink_live = sink.on_status(WriteStatus::Accepted);
        effects.push(Effect::EmitReceipt(id, WriteStatus::Accepted));

        self.pending.insert(
            id,
            PendingWrite {
                durability,
                routing,
                routing_valid: true,
                sinks: if sink_live {
                    vec![RegisteredReceiptSink {
                        registration: None,
                        sink,
                    }]
                } else {
                    Vec::new()
                },
                intent_id,
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
                lane_relays: BTreeSet::new(),
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

        match payload {
            WritePayload::Unsigned(unsigned)
            | WritePayload::UnsignedReplaceableEdit { unsigned, .. } => {
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
                        effects.push(Effect::RequestSign(id, generation, unsigned));
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
    /// output are whole-intent terminals (`WriteStatus::Failed`). Transport
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
                    let status = WriteStatus::AwaitingCapability {
                        pubkey: signing_pubkey,
                    };
                    Self::notify(pending, status.clone());
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
            let status = WriteStatus::AwaitingCapability {
                pubkey: pending.signing_pubkey,
            };
            Self::notify(pending, status.clone());
            effects.push(Effect::EmitReceipt(id, status));
        }
        effects
    }

    pub(super) fn on_signer_attached(&mut self, pk: PublicKey) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (id, pending) in &mut self.pending {
            if pending.signing_pubkey == pk
                && pending.event_id.is_none()
                && !pending.already_signed
                && !pending.sign_request_in_flight
            {
                pending.sign_request_in_flight = true;
                pending.sign_generation += 1;
                effects.push(Effect::RequestSign(
                    *id,
                    pending.sign_generation,
                    UnsignedEvent {
                        id: Some(pending.frozen.id),
                        pubkey: pending.frozen.pubkey,
                        created_at: pending.frozen.created_at,
                        kind: pending.frozen.kind,
                        tags: pending.frozen.tags.clone(),
                        content: pending.frozen.content.clone(),
                    },
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
        receipt: &nmp_store::RecoveredReceipt,
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
            ReceiptState::Abandoned => Err(CancelWriteError::AlreadyAbandoned { receipt_id: id }),
            ReceiptState::Accepted => Err(CancelWriteError::PersistenceFailed {
                receipt_id: id,
                reason: "accepted receipt has no live cancellation owner".to_string(),
            }),
        }
    }

    pub fn cancel_write(
        &mut self,
        id: ReceiptId,
    ) -> (Result<CancelWriteOutcome, CancelWriteError>, Vec<Effect>) {
        let mut effects = Vec::new();
        let Some(mut pending) = self.pending.remove(&id) else {
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
                        effects.push(Effect::EmitReceipt(id, WriteStatus::Cancelled));
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

        if let Some(intent_id) = pending.intent_id {
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
        } else {
            match self.resolver.store_mut().cancel_ephemeral_receipt(id.0) {
                Ok(CancelEphemeralOutcome::Cancelled) => {}
                Ok(CancelEphemeralOutcome::AlreadyCancelled) => {
                    self.pending.insert(id, pending);
                    return (Ok(CancelWriteOutcome::Cancelled), effects);
                }
                Ok(CancelEphemeralOutcome::AlreadySigned) => {
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
                Ok(CancelEphemeralOutcome::AlreadyAbandoned) => {
                    self.pending.insert(id, pending);
                    return (
                        Err(CancelWriteError::AlreadyAbandoned { receipt_id: id }),
                        effects,
                    );
                }
                Ok(CancelEphemeralOutcome::AlreadyCompensated) => {
                    self.pending.insert(id, pending);
                    return (
                        Err(CancelWriteError::AlreadyCompensated { receipt_id: id }),
                        effects,
                    );
                }
                Ok(CancelEphemeralOutcome::NotFound | CancelEphemeralOutcome::NotEphemeral) => {
                    self.pending.insert(id, pending);
                    return (
                        Err(CancelWriteError::PersistenceFailed {
                            receipt_id: id,
                            reason: "ephemeral cancellation owner does not match retained receipt"
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
            }
        }

        self.forget_pending_indexes(id, &pending);
        Self::notify(&mut pending, WriteStatus::Cancelled);
        effects.push(Effect::EmitReceipt(id, WriteStatus::Cancelled));
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
        if let Some(intent_id) = pending.intent_id {
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
                                .find(|(_, candidate)| candidate.intent_id == Some(co_intent))
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
        } else {
            match self.resolver.store_mut().mark_ephemeral_signed(id.0) {
                Ok(true) => {}
                Ok(false) => {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.sign_request_in_flight = false;
                    }
                    self.degrade_store(
                        PersistenceError(
                            "accepted ephemeral receipt disappeared during signature promotion"
                                .to_string(),
                        ),
                        effects,
                    );
                    return;
                }
                Err(error) => {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.sign_request_in_flight = false;
                    }
                    self.degrade_store(error, effects);
                    return;
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
            Self::notify(pending, WriteStatus::Signed(event.id));
            effects.push(Effect::EmitReceipt(id, WriteStatus::Signed(event.id)));
            if !pending.routing_valid {
                return;
            }
        }

        let author_hex = event.pubkey.to_hex();
        let relays = match self
            .pending
            .get(&id)
            .map(|pending| self.resolve_routes(&pending.routing, &author_hex))
        {
            Some(Ok(relays)) => relays,
            Some(Err(reason)) => {
                if let Some(mut pending) = self.pending.remove(&id) {
                    // No lanes have been bootstrapped for this intent yet at
                    // this point in `on_signed` (that only happens further
                    // below, after routes resolve) -- `lane_relays` is
                    // guaranteed empty, but `intent_receipts` was already
                    // populated at acceptance, so this must still clean it
                    // (epic #507 finding E5).
                    self.forget_pending_indexes(id, &pending);
                    let status = WriteStatus::Failed(reason);
                    Self::notify(&mut pending, status.clone());
                    effects.push(Effect::EmitReceipt(id, status));
                }
                return;
            }
            None => return,
        };

        self.emit_write_status(id, WriteStatus::Routed(relays.clone()), effects);

        if let Some(write_access) = self
            .pending
            .get(&id)
            .filter(|pending| pending.durability == Durability::Ephemeral)
            .map(|pending| AccessContext::Nip42(pending.signing_pubkey))
        {
            for relay in relays {
                let Ok(correlation) = self.alloc_attempt_correlation() else {
                    continue;
                };
                self.attempt_correlations.insert(
                    correlation,
                    AttemptCorrelationTarget {
                        receipt: id,
                        // The ephemeral handoff rides the intent's
                        // identity-scoped authenticated session (#8 U2),
                        // never the relay's Public read session.
                        session: RelaySessionKey::new(relay.clone(), write_access),
                        lane: None,
                    },
                );
                effects.push(Effect::PublishEvent(
                    RelaySessionKey::new(relay, write_access),
                    event.clone(),
                    correlation,
                ));
            }
            // Ephemeral never owns a durable lane (`intent_id` is `None`),
            // so there is nothing for `forget_pending_indexes` to find, but
            // calling it keeps this a single uniform cleanup discipline for
            // every real `pending` removal (epic #507 finding E5).
            if let Some(pending) = self.pending.remove(&id) {
                self.forget_pending_indexes(id, &pending);
            }
            return;
        }

        let Some((intent_id, write_access)) = self.pending.get(&id).and_then(|pending| {
            pending
                .intent_id
                .map(|intent_id| (intent_id, AccessContext::Nip42(pending.signing_pubkey)))
        }) else {
            return;
        };
        if self
            .resolver
            .store_mut()
            .record_route_revision(intent_id, relays.clone())
            .is_err()
        {
            if let Some(pending) = self.pending.get_mut(&id) {
                pending.route_blocked_relays = relays.clone();
            }
            for relay in relays {
                self.emit_write_status(id, WriteStatus::RoutePersistenceBlocked(relay), effects);
            }
            return;
        }

        let lanes = match self.resolver.store_mut().bootstrap_outbox_lanes(intent_id) {
            Ok(lanes) => lanes,
            Err(_) => {
                // This is the sole call that teaches the reverse index this
                // freshly-signed intent's lanes; a failure here means the
                // index cannot learn whatever lanes may (or may not) exist,
                // so degrade rather than assume "no lanes" (epic #507
                // finding E5).
                self.lane_relay_index_degraded = true;
                for relay in relays {
                    self.emit_write_status(id, WriteStatus::PersistenceBlocked(relay), effects);
                }
                return;
            }
        };
        self.event_to_receipts
            .entry(event.id)
            .or_default()
            .insert(id);
        for lane in lanes {
            let lane_relay = lane.key.relay.clone();
            if let Some(pending) = self.pending.get_mut(&id) {
                if pending.lane_relays.insert(lane_relay.clone()) {
                    self.receipts_by_lane_relay
                        .entry(lane_relay)
                        .or_default()
                        .insert(id);
                }
            }
            if matches!(lane.state, LaneState::WaitingConnection) {
                // The freshly-bootstrapped lane's connectivity check is
                // against the intent's identity-scoped authenticated
                // session (#8 U2), the exact session `schedule_ready` will
                // publish on.
                let session = RelaySessionKey::new(lane.key.relay.clone(), write_access);
                if self.connected_relays.contains(&session) {
                    let _ = self.resolver.store_mut().set_lane_eligible(
                        &lane.key,
                        lane.revision,
                        self.clock,
                    );
                } else {
                    self.emit_write_status(
                        id,
                        WriteStatus::AwaitingRelay {
                            relay: lane.key.relay.clone(),
                        },
                        effects,
                    );
                    effects.push(Effect::EnsureRelay(session));
                }
            }
        }
        effects.extend(self.schedule_ready(self.clock));
    }

    pub(super) fn freeze_payload(payload: &WritePayload) -> Result<SignedEvent, String> {
        match payload {
            WritePayload::Unsigned(unsigned)
            | WritePayload::UnsignedReplaceableEdit { unsigned, .. } => {
                let computed = EventId::new(
                    &unsigned.pubkey,
                    &unsigned.created_at,
                    &unsigned.kind,
                    &unsigned.tags,
                    &unsigned.content,
                );
                if let Some(declared) = unsigned.id {
                    if declared != computed {
                        return Err(
                            "unsigned event carries an id that does not match its body".into()
                        );
                    }
                }
                Ok(SignedEvent::new(
                    computed,
                    unsigned.pubkey,
                    unsigned.created_at,
                    unsigned.kind,
                    unsigned.tags.clone(),
                    unsigned.content.clone(),
                    sentinel_signature(),
                ))
            }
            WritePayload::Signed(event) => Ok(SignedEvent::new(
                event.id,
                event.pubkey,
                event.created_at,
                event.kind,
                event.tags.clone(),
                event.content.clone(),
                sentinel_signature(),
            )),
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

    pub(super) fn routing_snapshot(routing: &WriteRouting) -> String {
        match routing {
            WriteRouting::AuthorOutbox => "author-outbox".to_string(),
            WriteRouting::ToInboxes(recipients) => format!(
                "to-inboxes:{}",
                recipients
                    .iter()
                    .map(PublicKey::to_hex)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            WriteRouting::PrivateNarrow(route) => format!(
                "private-narrow-hex:{}",
                route
                    .relays
                    .iter()
                    .map(|relay| hex::encode(relay.to_string()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            WriteRouting::PinnedHost(auth) => {
                format!("pinned-host-hex:{}", hex::encode(auth.host().to_string()))
            }
        }
    }

    pub(super) fn parse_routing_snapshot(snapshot: &str) -> Option<WriteRouting> {
        if snapshot == "author-outbox" {
            return Some(WriteRouting::AuthorOutbox);
        }
        if let Some(keys) = snapshot.strip_prefix("to-inboxes:") {
            let recipients = if keys.is_empty() {
                Vec::new()
            } else {
                keys.split(',')
                    .map(PublicKey::from_hex)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?
            };
            return Some(WriteRouting::ToInboxes(recipients));
        }
        if let Some(encoded) = snapshot.strip_prefix("private-narrow-hex:") {
            let relays = if encoded.is_empty() {
                Vec::new()
            } else {
                encoded
                    .split(',')
                    .map(|part| {
                        let bytes = hex::decode(part).ok()?;
                        let url = String::from_utf8(bytes).ok()?;
                        RelayUrl::parse(&url).ok()
                    })
                    .collect::<Option<Vec<_>>>()?
            };
            return Some(WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new(relays),
            }));
        }
        if let Some(encoded) = snapshot.strip_prefix("pinned-host-hex:") {
            let bytes = hex::decode(encoded).ok()?;
            let url = String::from_utf8(bytes).ok()?;
            let host = RelayUrl::parse(&url).ok()?;
            return Some(WriteRouting::PinnedHost(HostAuthority::from_selected_host(
                host,
            )));
        }
        None
    }

    pub(super) fn fail_unaccepted(
        &mut self,
        sink: Box<dyn ReceiptSink>,
        reason: String,
    ) -> Vec<Effect> {
        self.fail_unaccepted_with_status(sink, WriteStatus::Failed(reason))
    }

    pub(super) fn fail_unaccepted_with_status(
        &mut self,
        sink: Box<dyn ReceiptSink>,
        status: WriteStatus,
    ) -> Vec<Effect> {
        // No store id exists on refusal/persistence failure by contract.
        // This correlation id is stream-local only and never enters the
        // durable receipt namespace.
        let id = match self.alloc_receipt_id() {
            Ok(id) => id,
            Err(err) => return vec![Effect::PublishFailed(err)],
        };
        let _ = sink.on_status(status.clone());
        vec![Effect::EmitReceipt(id, status)]
    }

    pub(super) fn fail_and_compensate(
        &mut self,
        id: ReceiptId,
        reason: String,
        effects: &mut Vec<Effect>,
    ) {
        let Some(mut pending) = self.pending.remove(&id) else {
            return;
        };

        if let Some(intent_id) = pending.intent_id {
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

        // Reached only when `intent_id` was `None` (Ephemeral -- nothing to
        // clean) or compensation actually committed (a real, permanent
        // removal): both `NotFound`/`Err` arms above reinsert `pending`
        // untouched and return early, so the indexes must stay untouched
        // for those (epic #507 finding E5).
        self.forget_pending_indexes(id, &pending);
        Self::notify(&mut pending, WriteStatus::Failed(reason.clone()));
        effects.push(Effect::EmitReceipt(id, WriteStatus::Failed(reason)));
    }

    /// Resolve a `WriteRouting` to a concrete relay set using the SAME
    /// `RelayDirectory` lane facts the read path routes against (plan
    /// §3.4). `AuthorOutbox` reuses the author's NIP-65 write-relay lane
    /// directly (the same fact `nmp_router::route::build_candidates` reads
    /// for outbox coverage-solving, minus the 2-relay-min solver — a write
    /// fans out to every known write relay, it does not need coverage-
    /// solving). `PrivateNarrow` never consults the directory at all — its
    /// relay set is exactly whatever the caller pre-narrowed into the
    /// `NarrowOnly` set, empty or not (ledger #6's fail-closed mechanism).
    ///
    /// `ToInboxes` fans a p-tagged inbox write out to each recipient's
    /// NIP-65 READ-marked relays (`RelayDirectory::read_relays`, lane
    /// `Nip65Read`) — the read side of the SAME kind:10002 winner the read
    /// path consults for authors' write relays (`routing-and-ownership.md`
    /// §2.4). It NEVER consults a recipient's `write_relays`/`extra_relays`:
    /// addressing inbox traffic to a recipient's write relays under-delivers
    /// and leaks metadata (issue #19). A recipient whose read/inbox relays
    /// are unknown — never seen a kind:10002, or one that declares only
    /// write-marked relays — fails the whole intent CLOSED with a typed
    /// `Failed` before any `PublishEvent`, rather than guessing a relay;
    /// recipient discovery rides the existing kind:10002 `sync_discovery`
    /// machinery, so a later winner simply makes the retry routable.
    ///
    /// `PinnedHost` (#115) also never consults the directory — like
    /// `PrivateNarrow`, its one relay is exactly whatever the caller
    /// asserted via `HostAuthority::from_selected_host`. Unlike
    /// `PrivateNarrow`, an empty/unroutable state is structurally
    /// unreachable (`HostAuthority` always carries exactly one well-formed
    /// `RelayUrl`), so this arm is infallible where `PrivateNarrow`'s is
    /// not.
    pub(super) fn resolve_routes(
        &self,
        routing: &WriteRouting,
        author_hex: &str,
    ) -> Result<BTreeSet<RelayUrl>, String> {
        match routing {
            WriteRouting::AuthorOutbox => {
                let author = author_hex.to_string();
                let relays: BTreeSet<RelayUrl> = self
                    .directory
                    .write_relays(&author)
                    .into_iter()
                    .map(|lr| lr.url)
                    .collect();
                if relays.is_empty() {
                    Err(format!("no write relays known for author {author_hex}"))
                } else {
                    Ok(relays)
                }
            }
            WriteRouting::ToInboxes(recipients) => {
                let mut relays = BTreeSet::new();
                for pk in recipients {
                    let hex = pk.to_hex();
                    // Read/inbox relays ONLY (lane `Nip65Read`) — never a
                    // recipient's write/extra relays. Fail CLOSED per
                    // recipient: an unknown or write-only recipient has no
                    // inbox relay, and guessing one would leak/under-deliver.
                    let inbox: Vec<RelayUrl> = self
                        .directory
                        .read_relays(&hex)
                        .into_iter()
                        .map(|lr| lr.url)
                        .collect();
                    if inbox.is_empty() {
                        return Err(format!(
                            "no NIP-65 read/inbox relays known for recipient {hex} -- \
                             inbox route fails closed, never falls back to write relays"
                        ));
                    }
                    relays.extend(inbox);
                }
                if relays.is_empty() {
                    Err("ToInboxes routing has no recipients".to_string())
                } else {
                    Ok(relays)
                }
            }
            WriteRouting::PrivateNarrow(route) => {
                if route.relays.is_empty() {
                    Err(
                        "private route has no narrow relay set -- fails closed, never widens to a public relay"
                            .to_string(),
                    )
                } else {
                    Ok(route.relays.iter().cloned().collect())
                }
            }
            WriteRouting::PinnedHost(auth) => Ok(BTreeSet::from([auth.host()])),
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
            let Some(intent_id) = pending.intent_id else {
                continue;
            };
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
            let key = LaneKey {
                intent_id,
                relay: relay.clone(),
            };
            let lane = self
                .resolver
                .store()
                .recover_outbox_lanes(intent_id)
                .ok()
                .and_then(|lanes| lanes.into_iter().find(|lane| lane.key == key));
            let Some(lane) = lane else {
                continue;
            };
            let LaneState::InFlight {
                ordinal,
                phase: InFlightPhase::AwaitingAck { .. },
            } = lane.state
            else {
                continue;
            };

            match &class {
                RelayAckClass::Acked => {
                    if self
                        .resolver
                        .store_mut()
                        .finish_lane_attempt(
                            &key,
                            lane.revision,
                            ordinal,
                            AttemptOutcome::Acked,
                            self.clock,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_status(id, WriteStatus::Acked(relay.clone()), effects);
                        self.close_if_all_lanes_terminal(id);
                    }
                }
                RelayAckClass::Rejected => {
                    if self
                        .resolver
                        .store_mut()
                        .finish_lane_attempt(
                            &key,
                            lane.revision,
                            ordinal,
                            AttemptOutcome::Rejected(message.clone()),
                            self.clock,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_status(
                            id,
                            WriteStatus::Rejected(relay.clone(), message.clone()),
                            effects,
                        );
                        self.close_if_all_lanes_terminal(id);
                    }
                }
                RelayAckClass::Transient(cause) => {
                    let eligible_at = self.clock + retry_delay_secs(&key, ordinal);
                    if self
                        .resolver
                        .store_mut()
                        .set_lane_transient(
                            &key,
                            lane.revision,
                            ordinal,
                            eligible_at,
                            *cause,
                            Some(message.clone()),
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_status(
                            id,
                            WriteStatus::RetryEligible {
                                relay: relay.clone(),
                                attempt: ordinal,
                                eligible_at,
                            },
                            effects,
                        );
                    }
                }
                RelayAckClass::WaitingAuth => {
                    self.auth_probe_sessions.remove(session);
                    self.auth_required_sessions.insert(session.clone());
                    if self
                        .resolver
                        .store_mut()
                        .suspend_lane_attempt(
                            &key,
                            lane.revision,
                            ordinal,
                            self.clock,
                            TransientCause::AuthRequired,
                            Some(message.clone()),
                            true,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_status(
                            id,
                            WriteStatus::AwaitingAuth {
                                relay: relay.clone(),
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
                LaneState::Eligible { .. } => {
                    if self
                        .resolver
                        .store_mut()
                        .set_lane_waiting(&lane.key, lane.revision, false)
                        .is_ok()
                    {
                        self.emit_write_status(
                            id,
                            WriteStatus::AwaitingRelay {
                                relay: relay.clone(),
                            },
                            effects,
                        );
                    }
                }
                LaneState::InFlight {
                    ordinal,
                    phase: InFlightPhase::AwaitingAck { .. },
                } => {
                    let durability = self.pending.get(&id).map(|pending| pending.durability);
                    if durability == Some(Durability::AtMostOnce) {
                        if self
                            .resolver
                            .store_mut()
                            .finish_lane_attempt(
                                &lane.key,
                                lane.revision,
                                ordinal,
                                AttemptOutcome::OutcomeUnknown,
                                self.clock,
                            )
                            .is_ok()
                        {
                            self.remove_active_lane(id, relay);
                            self.emit_write_status(
                                id,
                                WriteStatus::OutcomeUnknown(relay.clone()),
                                effects,
                            );
                            self.close_if_all_lanes_terminal(id);
                        }
                    } else {
                        let eligible_at = self.clock + retry_delay_secs(&lane.key, ordinal);
                        if self
                            .resolver
                            .store_mut()
                            .set_lane_transient(
                                &lane.key,
                                lane.revision,
                                ordinal,
                                eligible_at,
                                TransientCause::ConnectionLost,
                                Some("connection lost while awaiting ACK".to_string()),
                            )
                            .is_ok()
                        {
                            self.remove_active_lane(id, relay);
                            self.emit_write_status(
                                id,
                                WriteStatus::RetryEligible {
                                    relay: relay.clone(),
                                    attempt: ordinal,
                                    eligible_at,
                                },
                                effects,
                            );
                        }
                    }
                }
                LaneState::WaitingAuth => {
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
                        .resolver
                        .store_mut()
                        .set_lane_waiting(&lane.key, lane.revision, false)
                        .is_ok()
                    {
                        self.emit_write_status(
                            id,
                            WriteStatus::AwaitingRelay {
                                relay: relay.clone(),
                            },
                            effects,
                        );
                    }
                }
                LaneState::WaitingConnection
                | LaneState::Transient { .. }
                | LaneState::InFlight {
                    phase: InFlightPhase::AwaitingHandoff,
                    ..
                }
                | LaneState::LegacyInFlight { .. }
                | LaneState::Terminal { .. } => {}
            }
        }
    }
    pub(super) fn alloc_receipt_id(&mut self) -> Result<ReceiptId, PublishError> {
        const FIRST_UNACCEPTED_ID: u64 = 1u64 << 63;
        let current = self
            .next_unaccepted_receipt
            .ok_or(PublishError::ReceiptCorrelationIdExhausted)?;
        debug_assert!(current >= FIRST_UNACCEPTED_ID);
        self.next_unaccepted_receipt = (current > FIRST_UNACCEPTED_ID).then_some(current - 1);
        Ok(ReceiptId(current))
    }

    #[cfg(test)]
    pub(super) fn set_next_unaccepted_receipt_for_test(&mut self, next: Option<u64>) {
        assert!(next.is_none_or(|id| id >= (1u64 << 63)));
        self.next_unaccepted_receipt = next;
    }

    pub(super) fn notify(pending: &mut PendingWrite, status: WriteStatus) {
        pending
            .sinks
            .retain(|sink| sink.sink.on_status(status.clone()));
    }
}
