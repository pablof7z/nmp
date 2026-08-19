//! The write lifecycle: attempt bookkeeping, retry/give-up, handoff, ack scheduling, and lane teardown.

use super::*;

impl CoreState {
    /// Mint the next [`AttemptCorrelation`] (issue #93). Checked, typed
    /// exhaustion: no id is reused or fabricated.
    pub(in crate::core) fn alloc_attempt_correlation(
        &mut self,
    ) -> Result<AttemptCorrelation, AttemptCorrelationExhausted> {
        let id = self
            .next_attempt_correlation
            .ok_or(AttemptCorrelationExhausted)?;
        self.next_attempt_correlation = id.checked_add(1);
        Ok(AttemptCorrelation(id))
    }

    /// Everything a permanently-discarded pending write leaves behind ACROSS
    /// owners, at every REAL removal (epic #507 finding E5, #903).
    ///
    /// The owner's own three indexes are its business and are forgotten
    /// through its one door; what stays here is the state this reducer holds
    /// on the write's behalf but does not keep inside it. Never call this at
    /// `fail_and_compensate`'s transient remove-then-reinsert
    /// (`CompensateOutcome::NotFound`/`Err`), which must leave everything
    /// untouched because the obligation and its lanes are still live.
    pub(in crate::core) fn forget_pending_indexes(
        &mut self,
        id: ReceiptId,
        pending: &PendingWrite,
        effects: &mut Vec<Effect>,
    ) {
        self.pending.forget_indexes(id, pending);
        // A removed write owns no projection to reconcile, so its bootstrap
        // gap is closed by the removal. Leaving the entry would keep
        // rearming a deadline for a receipt that can never bootstrap again
        // and, for a blind gap, would suppress worker reconciliation forever.
        self.release_all_coordinate_coverage(id, effects);
    }

    /// Which receipts this turn could have changed the stalled stage of,
    /// and the census refresh that answers whether any of them did.
    ///
    /// The touched set is derived here, not inside the owner: reading
    /// `Effect::EmitReceipt` as "this obligation's write facts moved" is a
    /// property of how this reducer emits, and the census owner must not
    /// know what an `Effect` is.
    ///
    /// This is also the exact place the change detector's coverage is
    /// visible: `stalled_write_stage` reads connectivity as well as the
    /// obligation, and a connectivity change carries no `EmitReceipt` of its
    /// own. Every path that ends a session drives a receipt-shaped fact for
    /// each lane it interrupts, so the set is currently complete.
    pub(in crate::core) fn refresh_stalled_write_cache_for_effects(
        &mut self,
        effects: &[Effect],
    ) -> bool {
        let touched: BTreeSet<ReceiptId> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::EmitReceipt(id, _) => Some(*id),
                _ => None,
            })
            .collect();
        self.stalled_writes.refresh(
            &touched,
            StalledWriteInputs {
                pending: &self.pending,
                connected: &self.connected_relays,
            },
        )
    }

    /// Rebuild the census from scratch after `pending` was rebuilt from the
    /// store.
    pub(in crate::core) fn rebuild_stalled_write_cache(&mut self) {
        self.stalled_writes.rebuild(StalledWriteInputs {
            pending: &self.pending,
            connected: &self.connected_relays,
        });
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
    pub(in crate::core) fn emit_write_fact(
        &mut self,
        id: ReceiptId,
        fact: WriteFact,
        effects: &mut Vec<Effect>,
    ) {
        let recipients = match &fact {
            WriteFact::Relay { event_id, .. } => self
                .pending
                .receipts_for_event(event_id)
                .cloned()
                .unwrap_or_else(|| BTreeSet::from([id])),
            WriteFact::Destinations { .. }
                if self.pending.get(&id).is_some_and(|pending| {
                    matches!(pending.target, PendingWriteTarget::ReplaceableOperation(_))
                }) =>
            {
                self.pending
                    .get(&id)
                    .and_then(|pending| self.pending.receipts_for_event(&pending.frozen.id))
                    .cloned()
                    .unwrap_or_else(|| BTreeSet::from([id]))
            }
            _ => BTreeSet::from([id]),
        };
        effects.extend(
            recipients
                .into_iter()
                .map(|recipient| Effect::EmitReceipt(recipient, fact.clone())),
        );
    }

    /// One lane attempt ended in a way that PERMITS another try; the ceiling
    /// decides whether one happens (#1031).
    ///
    /// `nmp::EngineConfig::max_publish_attempts` counts
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
    pub(in crate::core) fn retry_or_give_up(
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
                        event_id: key.event_id,
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
                    event_id: key.event_id,
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

    pub(in crate::core) fn remove_active_lane(&mut self, id: ReceiptId, relay: &RelayUrl) {
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
    pub(in crate::core) fn close_if_all_lanes_terminal(
        &mut self,
        id: ReceiptId,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(coordinate) = self
            .pending
            .get(&id)
            .and_then(|pending| match &pending.target {
                PendingWriteTarget::ReplaceableOperation(target) => Some(target.coordinate.clone()),
                PendingWriteTarget::Event => None,
            })
        {
            self.try_close_semantic_cohort(&coordinate, effects);
            return;
        }
        let Some(intent_id) = self
            .pending
            .get(&id)
            .filter(|pending| {
                // Routing that can still change its mind is the reason a
                // parked write is HELD rather than dropped: an intent whose
                // strategy has unknowns left owns no lane at all, so every
                // lane it has is trivially terminal, and closing on that
                // would delete the exact obligation the queue rewriter is
                // waiting to complete. Nothing auto-abandons.
                pending.route_complete && pending.lane_projection.can_close()
            })
            .map(|pending| pending.intent_id)
        else {
            return;
        };
        let Ok(CloseIntentOutcome::Closed | CloseIntentOutcome::AlreadyClosed) =
            self.commit_terminal_close(intent_id)
        else {
            return;
        };
        if let Some(pending) = self.pending.remove(&id) {
            self.forget_pending_indexes(id, &pending, effects);
        }
        effects.push(Effect::EmitReceipt(
            id,
            WriteFact::Outcome(WriteOutcome::Settled),
        ));
    }

    /// Ask the existing atomic store door to close the complete cohort.
    ///
    /// Settlement for a semantic write is a COHORT fact, not a receipt fact.
    /// N contributing intents share one materialized generation and only the
    /// owner -- `generation.members.first()` -- holds routes and lanes; the
    /// other N-1 hold none. Routing them through the ordinary receipt path
    /// above would settle those N-1 immediately and wrongly, because a member
    /// with zero lanes trivially satisfies `lane_projection.can_close()`.
    /// They close together in one redb transaction that also compacts the
    /// replay program and deletes the resource row, or not at all.
    ///
    /// The predicate is exactly the ordinary one, read off the owner: routing
    /// closed and every lane of the current generation terminal. The store
    /// revalidates every route/lane fact itself; the reducer only supplies the
    /// current CAS witnesses and then removes the volatile receipt owners
    /// after a committed close.
    pub(in crate::core) fn try_close_semantic_cohort(
        &mut self,
        coordinate: &Coordinate,
        effects: &mut Vec<Effect>,
    ) {
        let snapshot = match self.store.replaceable_operation_snapshot(coordinate) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(_error) => {
                return;
            }
        };
        let Some(generation) = snapshot.current.generation.as_ref() else {
            return;
        };
        let Some(owner_intent) = generation.members.first().copied() else {
            return;
        };
        let Some(owner_receipt) = self.pending.receipt_for_intent(owner_intent) else {
            return;
        };
        let Some(pending) = self.pending.get(&owner_receipt) else {
            return;
        };
        if !pending.route_complete || !pending.lane_projection.can_close() {
            return;
        }
        let destination = if pending.durable_routes.is_empty() {
            nmp_store::SemanticDestinationPlanClosure::NoDestinations
        } else {
            nmp_store::SemanticDestinationPlanClosure::AllCurrentDestinationsTerminal
        };
        let close = nmp_store::SemanticCohortClose {
            coordinate: coordinate.clone(),
            expected_source_revision: snapshot.current.source_revision,
            expected_program_digest: snapshot.current.program_digest,
            expected_materialization: generation.materialization,
            destination,
        };
        match self.store.close_replaceable_operation_cohort(close) {
            Ok(nmp_store::SemanticCohortCloseOutcome::Closed { members }) => {
                for member in members {
                    let Some(receipt) = self.pending.receipt_for_intent(member) else {
                        continue;
                    };
                    if let Some(pending) = self.pending.remove(&receipt) {
                        self.forget_pending_indexes(receipt, &pending, effects);
                    }
                    effects.push(Effect::EmitReceipt(
                        receipt,
                        WriteFact::Outcome(WriteOutcome::Settled),
                    ));
                }
            }
            Ok(
                nmp_store::SemanticCohortCloseOutcome::DestinationOpen
                | nmp_store::SemanticCohortCloseOutcome::Stale,
            ) => {}
            Err(_error) => {},
        }
    }

    /// Consume the one, ever, typed transport handoff for an exact persisted
    /// lane ordinal. The next lane fact commits before any receipt claim or
    /// subsequent wire effect: transport never becomes a second retry owner.
    pub(in crate::core) fn on_event_handoff(
        &mut self,
        correlation: AttemptCorrelation,
        result: HandoffResult,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(target) = self.attempt_correlations.remove(&correlation) else {
            return effects;
        };

        let Some((key, ordinal)) = target.lane else {
            return effects;
        };
        let intent_id = key.intent_id;
        let Ok(Some(lane)) = self
            .store
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
                        event_id: key.event_id,
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
                // `NotHandedOff` is the transport reporting it has NO session
                // for this relay -- `pool.ensure_session` failed, so nothing
                // was sent and no socket was observed closing. That is a
                // connectivity fact, so the session leaves `connected_relays`.
                //
                // It deliberately does NOT touch `slot_to_relay` or
                // `auth_sessions`, and the resulting two-thirds state is safe
                // in exactly one direction. Every predicate over those three
                // (`exact_current_auth_epoch`, `is_current_transport_session`
                // in `auth_transport.rs`) is a CONJUNCTION, so a missing term
                // can only cause a rejection, never an acceptance: the cost
                // is a discarded AUTH operation, never an accepted stale one.
                //
                // The state repairs itself through the `EnsureWriteRelay`
                // below. A protected reconnect runs `invalidate_auth_epoch`
                // BEFORE re-inserting into `connected_relays`, so the auth
                // entries left behind here are cleared by the same edge that
                // restores connectivity -- in that order, which is what makes
                // it safe.
                //
                // Do not "complete" this by also clearing `slot_to_relay` or
                // `auth_sessions`: this path never observed a generation end,
                // so retiring one here would discard a socket that is still
                // live for reads.
                self.connected_relays.remove(&target.session);
                self.emit_write_fact(
                    target.receipt,
                    WriteFact::Relay {
                        event_id: key.event_id,
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

    /// Recover the complete current scheduler input.
    ///
    /// Ordinary operation reads the exact committed nonterminal rows already
    /// owned by the reducer. A lane-less obligation costs nothing and a large
    /// durable backlog performs no database reads on each healthy publish.
    /// `wake_relay_lanes` narrows ordinary relay events through
    /// `receipts_by_lane_relay`.
    pub(in crate::core) fn recover_all_lanes(
        &self,
    ) -> Result<Vec<(ReceiptId, PublishQueueLane)>, PersistenceError> {
        let mut lanes = Vec::new();
        for (id, pending) in self.pending.iter() {
            lanes.extend(
                pending
                    .lane_projection
                    .current_nonterminal
                    .values()
                    .cloned()
                    .map(|lane| (*id, lane)),
            );
        }
        lanes.sort_by(|(_, left), (_, right)| left.key.cmp(&right.key));
        Ok(lanes)
    }

    /// Whether transport still owes this exact attempt a handoff.
    ///
    /// `attempt_correlations` is bounded by `MAX_GLOBAL_ATTEMPTS`, so this is
    /// a scan of at most 32 entries and never a store read.
    fn handoff_is_outstanding(&self, intent_id: IntentId, ordinal: u64) -> bool {
        self.attempt_correlations.values().any(|target| {
            target
                .lane
                .as_ref()
                .is_some_and(|(key, current)| key.intent_id == intent_id && *current == ordinal)
        })
    }

    /// Give back the relay slot held by an attempt whose handoff can never
    /// arrive. Returns whether the lane actually left flight — `false` both
    /// for an attempt that is genuinely still waiting and for a reclaim that
    /// could not commit, because in either case the lane is honestly still in
    /// flight and still owns its slot.
    ///
    /// This is the one owner of a rule boot recovery used to state on its
    /// own: an `AwaitingHandoff` lane is waiting on exactly one
    /// `EngineMsg::EventHandoff`, and `on_event_handoff` dispatches those by
    /// correlation. No correlation naming this attempt means transport's
    /// one-shot result (#93) has already been consumed — or was never
    /// submitted by this process at all — so the wait has become a hang. A
    /// fresh process is merely the case where that set is empty because the
    /// process is new, which is why generalizing this deleted the boot arm
    /// rather than adding a second copy of it.
    ///
    /// The trigger is a FACT the reducer holds, never elapsed time: nothing
    /// here consults a clock to decide, and `now` is only the instant the
    /// replacement attempt becomes eligible. Resending is safe by
    /// construction — it is the IDENTICAL frozen event, so a relay that did
    /// receive it dedupes on id.
    ///
    /// It emits no receipt fact, deliberately. Nothing was OBSERVED from the
    /// relay — the attempt is being replaced, not reported on — and a live
    /// `BackingOff` claim would contradict a receipt whose own attempt
    /// evidence may be unreadable. The committed transient is the record, and
    /// reattachment replays it from the store like every other attempt fact.
    pub(super) fn reclaim_orphaned_handoff(
        &mut self,
        id: ReceiptId,
        lane: &PublishQueueLane,
        ordinal: u64,
        now: Timestamp,
    ) -> bool {
        // The guard lives HERE, not at the call sites, because both of them
        // can see a live attempt: boot recovery re-opens an intent's
        // whole lane set mid-process, and that set may include a relay whose
        // attempt this process really did submit.
        if self.handoff_is_outstanding(lane.key.intent_id, ordinal) {
            return false;
        }
        if self
            .commit_lane_transient(
                &lane.key,
                lane.revision,
                ordinal,
                now,
                PublishQueueTransientCause::Interrupted,
                Some(ORPHANED_HANDOFF_DETAIL.to_string()),
            )
            .is_err()
        {
            return false;
        }
        self.remove_active_lane(id, &lane.key.relay);
        true
    }

    /// The only path that allocates durable attempt ordinals. Eligibility is
    /// persisted first; this reducer then applies stable ordering and the
    /// ratified 32-global/1-per-relay caps before committing Started.
    pub(in crate::core) fn schedule_ready(&mut self, now: Timestamp) -> Vec<Effect> {
        let mut effects = Vec::new();
        // A lane read that fails costs this pass only; the durable rows are
        // untouched and the next scheduling pass re-reads them.
        let Ok(lanes) = self.recover_all_lanes() else {
            return effects;
        };

        let mut in_flight_relays = BTreeSet::new();
        let mut in_flight = 0usize;
        let mut eligible = Vec::new();
        for (id, lane) in lanes {
            match lane.state {
                // An attempt whose handoff can never arrive gives its relay's
                // one slot back rather than holding it for the life of the
                // process (#1316). One that is genuinely still waiting keeps
                // it — per-relay ordering is the ratified cap, not the bug.
                PublishQueueLaneState::InFlight {
                    ordinal,
                    phase: PublishQueueInFlightPhase::AwaitingHandoff,
                } => {
                    if self.reclaim_orphaned_handoff(id, &lane, ordinal, now) {
                        continue;
                    }
                    in_flight = in_flight.saturating_add(1);
                    in_flight_relays.insert(lane.key.relay.clone());
                }
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
            // `Nip42(signing pubkey)`, never the relay's unbound read
            // session). A lane whose receipt has no live pending entry has
            // nothing to schedule.
            let Some(pending) = self.pending.get(&id) else {
                continue;
            };
            let session = RelaySessionKey::new(
                lane.key.relay.clone(),
                Some(pending.signing_pubkey),
            );
            // Connectivity is process-local, so re-parking the lane records
            // NOTHING durable (#889): an `Eligible` lane WITHOUT this session
            // and a `WaitingConnection` lane already project to the identical
            // `RelayState::Waiting(NotConnected)` at the enumeration door —
            // which is exactly the case this branch is in, because the
            // projection asks `connected_relays` the same question asked
            // immediately below and gets the same answer. And
            // the reverse transition is the one `wake_relay_lanes` performs
            // when a session arrives. Committing it here cost one
            // fsync-durable transaction per eligible lane every time a
            // disconnected engine passed over the queue -- at boot, where
            // NOTHING is connected yet, that is the whole queue. The lane
            // stays `Eligible` and this same loop picks it up on the
            // `schedule_ready` that closes every `wake_relay_lanes`, so
            // nothing is stranded by leaving it alone.
            if !self.connected_relays.contains(&session) {
                // A coordinate answer is a fact about one relay SESSION. The
                // session this lane needs is gone, so whatever it learned
                // about that relay's current value died with it and the lane
                // asks again on the session that replaces it.
                self.release_coordinate_coverage(id, &lane.key.relay, &mut effects);
                effects.push(Effect::EnsureWriteRelay(session));
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
                            event_id: lane.key.event_id,
                            relay: lane.key.relay.clone(),
                            state: RelayState::Waiting(RelayWaiting::NeedsAuth),
                        },
                        &mut effects,
                    );
                }
                continue;
            }
            if in_flight >= MAX_GLOBAL_ATTEMPTS || in_flight_relays.contains(&lane.key.relay) {
                continue;
            }
            // The per-relay coordinate gate (#1631). A delta generation is
            // built from whatever value NMP holds; sending it to a relay
            // that holds a NEWER list would overwrite that list with one
            // derived from an older base. A complete-event write carries no
            // such base and skips the check entirely.
            if let Some(coordinate) =
                self.pending
                    .get(&id)
                    .and_then(|pending| match &pending.target {
                        PendingWriteTarget::ReplaceableOperation(target) => {
                            Some(target.coordinate.clone())
                        }
                        PendingWriteTarget::Event => None,
                    })
            {
                // Which view of the relay to ask for. The AUTH gate above
                // has already established one of exactly two things about
                // this lane: the relay never required AUTH for this
                // identity, in which case its ordinary public read session
                // is the view it serves; or AUTH completed, in which case
                // the authenticated view is both the correct one and
                // actually reachable. Asking on the identity-scoped session
                // unconditionally would park every write to an ordinary
                // relay behind an authenticated READ session that relay
                // will never open.
                let read_session = if self.auth_ready_sessions.contains_key(&session) {
                    session.clone()
                } else {
                    RelaySessionKey::unauthenticated(lane.key.relay.clone())
                };
                if !self.coordinate_is_current_for_lane(
                    id,
                    &coordinate,
                    &read_session,
                    &lane.key.relay,
                    &mut effects,
                ) {
                    continue;
                }
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
                // The attempt did not commit, so this lane does not start on
                // this pass. Nothing durable changed and nothing is reported:
                // the write has not advanced, and the next scheduling pass
                // tries again.
                Err(_) => continue,
            };
            debug_assert_eq!(
                advanced.state,
                PublishQueueLaneState::InFlight {
                    ordinal: attempt.ordinal,
                    phase: PublishQueueInFlightPhase::AwaitingHandoff,
                }
            );
            if let Some(pending) = self.pending.get_mut(&id) {
                pending.pending_relays.insert(lane.key.relay.clone());
                pending
                    .attempt_ordinals
                    .insert(lane.key.relay.clone(), attempt.ordinal);
            }
            self.pending.index_receipt_under_event(event.id, id);
            self.attempt_correlations.insert(
                correlation,
                AttemptCorrelationTarget {
                    receipt: id,
                    session: session.clone(),
                    lane: Some((lane.key.clone(), attempt.ordinal)),
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
    /// `schedule_ready` at the end). There is one path now: it narrows via
    /// `receipts_by_lane_relay` to exactly the receipts that actually own a
    /// lane on `session.relay`, re-reading only those intents.
    /// (`receipts_by_lane_relay`/`PublishQueueLaneKey` stay URL-keyed in the
    /// store — only the SESSION comparison in [`Self::apply_relay_wake`],
    /// derived per lane from its pending write's signing identity, decides
    /// whether a lane belongs to THIS session.)
    ///
    /// A per-receipt store read that fails costs this pass only, and no
    /// wider fallback replaces it: the durable lane rows are untouched, so
    /// the next connect/auth message for the same session re-reads them.
    pub(in crate::core) fn wake_relay_lanes(
        &mut self,
        session: &RelaySessionKey,
        auth_only: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();

        // Take the candidate receipt set by value first: the loop below needs
        // a mutable borrow of `self` for its store reads, so it cannot hold a
        // live borrow of the lane index at the same time.
        let candidates = self.pending.receipts_with_lane_on(&session.relay);

        let mut lanes: Vec<(ReceiptId, PublishQueueLane)> = Vec::new();
        for id in candidates {
            let Some(intent_id) = self.pending.get(&id).map(|pending| pending.intent_id) else {
                continue;
            };
            match self.store.recover_publish_queue_lanes(intent_id) {
                Ok(recovered) => lanes.extend(
                    recovered
                        .into_iter()
                        .filter(|lane| lane.key.relay == session.relay)
                        .map(|lane| (id, lane)),
                ),
                // A read failure for this one receipt costs this pass only:
                // a later engine message retries it, and the durable lane rows
                // are untouched.
                Err(_) => {}
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

    /// The per-lane wake body, split out of `wake_relay_lanes` -- its one
    /// caller -- so the loop reads as one thing. `lanes` is assumed
    /// pre-sorted by `lane.key`, and pre-filtered to `session.relay` but NOT
    /// to `session`. The per-lane check below is the identity half, and is
    /// not redundant with the caller's: the store keys lanes by relay URL
    /// alone, while since the AUTH-reducer wave (#8 U2) the write plane
    /// rides the lane's identity-scoped authenticated session, so a lane
    /// belongs to `RelaySessionKey::new(lane.key.relay,
    /// Nip42(pending.signing_pubkey))` and two identities on one relay are
    /// two distinct sessions. A lane whose receipt has no pending entry is
    /// skipped: without a live pending write there is nothing to wake.
    pub(in crate::core) fn apply_relay_wake(
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
            if RelaySessionKey::new(lane.key.relay.clone(), Some(signing_pubkey))
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
                    self.store
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
                            } else if let Some((cause, detail)) = retry_detail {
                self.emit_write_fact(
                    id,
                    WriteFact::Relay {
                        event_id: lane.key.event_id,
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

    pub(in crate::core) fn cancel_write(
        &mut self,
        id: ReceiptId,
    ) -> (Result<CancelWriteOutcome, CancelWriteError>, Vec<Effect>) {
        let mut effects = Vec::new();
        let Some(pending) = self.pending.remove(&id) else {
            if let Some(quarantined) = self.quarantined_auth_receipts.get(&id).cloned() {
                match self.store.cancel_write(quarantined.intent_id) {
                    Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                        let event_id = quarantined.frozen.id;
                        match self.resolver.react_to_compensation(
                            &self.store,
                            quarantined.frozen,
                            &outcome,
                        ) {
                            Ok(committed) => self.apply_committed_mutation(committed, &mut effects),
                            Err(_error) => {},
                        }
                        self.quarantined_auth_receipts.remove(&id);
                        self.pending.unindex_receipt_from_event(event_id, id);
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
            let retained = match self.store.reattach_receipt(id.0) {
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
            match self.store.cancel_write(intent_id) {
                Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                    match self.resolver.react_to_compensation(
                        &self.store,
                        pending.frozen.clone(),
                        &outcome,
                    ) {
                        Ok(committed) => self.apply_committed_mutation(committed, &mut effects),
                        Err(_error) => {},
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
                    let result = match self.store.reattach_receipt(id.0) {
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

        self.forget_pending_indexes(id, &pending, &mut effects);
        effects.push(Effect::EmitReceipt(
            id,
            WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Cancelled)),
        ));
        effects.extend(self.schedule_ready(self.clock));
        (Ok(CancelWriteOutcome::Cancelled), effects)
    }

    pub(in crate::core) fn fail_and_compensate(
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
            match self.store.compensate_write(intent_id) {
                Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                    // The store compensation already committed; reacting only
                    // re-reads to recompute the graph. A read failure here
                    // (issue #122) degrades to read-only rather than panics.
                    match self.resolver.react_to_compensation(
                        &self.store,
                        pending.frozen.clone(),
                        &outcome,
                    ) {
                        Ok(committed) => {
                            self.apply_committed_mutation(committed, effects);
                        }
                        Err(_e) => {},
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
        self.forget_pending_indexes(id, &pending, effects);
        effects.push(Effect::EmitReceipt(
            id,
            WriteFact::Signing(SigningState::Refused { reason }),
        ));
        effects.push(Effect::EmitReceipt(
            id,
            WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::SignerRefused)),
        ));
    }

    /// An `OK` frame resolves exactly one (event, relay) pair's pending
    /// ack. An `OK` for an event/relay this reducer isn't tracking (unknown
    /// event id, already-terminal receipt, duplicate OK, or an `Ephemeral`
    /// write that was already forgotten) is silently ignored — it is an
    /// untrusted-network fact, not a caller error.
    pub(in crate::core) fn handle_write_ack(
        &mut self,
        event_id: EventId,
        status: bool,
        message: String,
        session: &RelaySessionKey,
        effects: &mut Vec<Effect>,
    ) {
        let Some(ids) = self.pending.receipts_for_event(&event_id).cloned() else {
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
                Some(pending.signing_pubkey),
            );
            if &expected_session != session {
                continue;
            }
            let relay = &session.relay;
            let key = PublishQueueLaneKey {
                intent_id,
                event_id,
                relay: relay.clone(),
            };
            let lane = self
                .store
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
                                event_id,
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
                                event_id,
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
                                event_id,
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

    pub(in crate::core) fn suspend_disconnected_lanes(
        &mut self,
        session: &RelaySessionKey,
        effects: &mut Vec<Effect>,
    ) {
        let Ok(lanes) = self.recover_all_lanes() else {
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
            if RelaySessionKey::new(lane.key.relay.clone(), Some(signing_pubkey))
                != *session
            {
                continue;
            }
            let relay = &session.relay;
            match lane.state {
                // The loss of a socket is process-local knowledge, so the fact
                // is emitted and the lane is left exactly as durable as it was
                // (#889). An eligible lane already reads back as
                // `RelayState::Waiting(NotConnected)` through the enumeration
                // door, so the old `WaitingConnection` rewrite spent one
                // fsync-durable transaction per lane on this relay to change
                // nothing an app or a later boot could observe.
                PublishQueueLaneState::Eligible { .. } => {
                    self.emit_write_fact(
                        id,
                        WriteFact::Relay {
                            event_id: lane.key.event_id,
                            relay: relay.clone(),
                            state: RelayState::Waiting(RelayWaiting::NotConnected),
                        },
                        effects,
                    );
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
                                event_id: lane.key.event_id,
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
