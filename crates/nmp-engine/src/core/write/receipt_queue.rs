//! Receipt replay and publish-queue projection: reattaching a receipt's history and answering queue-entry queries.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReceiptReplayFactKey {
    ReceiptStatus,
    AwaitingCapability,
    /// The routing park. One key for the whole picture: a reattach replays
    /// the park once, carrying whatever it is currently waiting on. Movement
    /// WITHIN the park — a second unknown settling while one recipient is
    /// still missing — is news on the live stream, not on a replay, and
    /// `picture_changed` is what decides it there.
    Destinations,
    Attempt {
        relay: RelayUrl,
        key: ReceiptAttemptReplayKey,
    },
    Lane {
        relay: RelayUrl,
        revision: u64,
    },
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
        }
    }
}

impl CoreState {
    /// One durable receipt's retained fact, if it has one. Unknown ids do not
    /// create state.
    pub(in crate::core) fn retained_receipt_fact(
        receipt: &nmp_store::PublishQueueReceipt,
    ) -> Option<WriteFact> {
        let PublishQueueReceiptPayload::Event { event_id, state } = &receipt.payload else {
            return None;
        };
        match state {
            // Acceptance is not a fact — it is what `publish()` returning
            // `Ok` already said. A receipt that has only been accepted has
            // nothing yet to replay.
            ReceiptState::Accepted => None,
            ReceiptState::Signed => Some(WriteFact::Signing(SigningState::Signed {
                event_id: *event_id,
            })),
            ReceiptState::Compensated => Some(WriteFact::Outcome(WriteOutcome::NotSent(
                NotSentReason::SignerRefused,
            ))),
            ReceiptState::Cancelled => Some(WriteFact::Outcome(WriteOutcome::NotSent(
                NotSentReason::Cancelled,
            ))),
            ReceiptState::Superseded => Some(WriteFact::Outcome(WriteOutcome::Superseded)),
            ReceiptState::Refused(reason) => {
                Some(WriteFact::Outcome(WriteOutcome::Refused(*reason)))
            }
            ReceiptState::NoDestination => Some(WriteFact::Outcome(WriteOutcome::NoDestination)),
        }
    }

    pub(in crate::core) fn reattach_receipt(&mut self, id: ReceiptId) -> ReceiptReplayPage {
        self.reattach_receipt_page(id, None, usize::MAX)
    }

    /// Reconstruct one finite page of a receipt's durable prefix.
    ///
    /// The opaque cursor records fact identity independently for each relay
    /// lane, so a newly persisted fact on an earlier-sorted relay cannot
    /// shift another relay's continuation. Core performs no delivery or live
    /// registration; runtime joins a caught-up page to its mailbox registry
    /// while the serialized engine loop still owns the command.
    pub(in crate::core) fn reattach_receipt_page(
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
        let receipt = match self.store.reattach_receipt(id.0) {
            Ok(Some(receipt)) => receipt,
            Ok(None) => return ReceiptReplayPage::unavailable(ReattachOutcome::NotFound),
            Err(_) => {
                return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable)
            }
        };
        let semantic = match &receipt.payload {
            PublishQueueReceiptPayload::ReplaceableOperation {
                coordinate,
                acceptance: nmp_store::ReplaceableOperationAcceptance::BodyComplete(accepted),
                state:
                    nmp_store::ReplaceableOperationReceiptState::Contributing {
                        current: Some(current),
                    },
            } => Some((coordinate.clone(), *accepted, *current)),
            _ => None,
        };
        let receipt_state = semantic
            .as_ref()
            .map(|(_, _, current)| match current.sig_state {
                IntentSigState::Signed => ReceiptState::Signed,
                IntentSigState::AwaitingSigner | IntentSigState::Pending => ReceiptState::Accepted,
            })
            .or_else(|| receipt.event_state());
        let Some(receipt_state) = receipt_state else {
            // The store can truthfully reattach this ordinary receipt, but
            // #841 has not yet installed the runtime projection for semantic
            // materialization facts. Never reinterpret it as event work.
            return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable);
        };
        let receipt_event_id = semantic
            .as_ref()
            .map(|(_, _, current)| current.materialization.event_id)
            .or_else(|| receipt.event_id())
            .expect("active receipt carries a current event id");
        let evidence_intent = if let Some((coordinate, _, _)) = &semantic {
            match self.store.replaceable_operation_snapshot(coordinate) {
                Ok(Some(snapshot)) => snapshot
                    .current
                    .generation
                    .and_then(|generation| generation.members.first().copied()),
                _ => return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable),
            }
        } else {
            receipt.intent_id
        };
        let projection_id = evidence_intent
            .and_then(|intent| self.pending.receipt_for_intent(intent))
            .unwrap_or(id);
        if self
            .pending
            .get(&projection_id)
            .is_some_and(|pending| !pending.routing_valid)
        {
            // Boot retained the obligation but could not interpret its
            // frozen routing policy. Replaying even the readable receipt
            // prefix would falsely imply that this observer is attached to
            // actionable live work, and registering it would leak later
            // signer facts from an obligation whose destination is unknown.
            return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable);
        }
        let (attempts, details, lanes) = match evidence_intent {
            Some(intent_id) => {
                let attempts = match self.store.recover_attempts(intent_id) {
                    Ok(attempts) => attempts,
                    Err(_) => {
                        return ReceiptReplayPage::unavailable(
                            ReattachOutcome::RetainedButUnreadable,
                        )
                    }
                };
                let details = match self.store.recover_attempt_details(intent_id) {
                    Ok(details) => details,
                    Err(_) => {
                        return ReceiptReplayPage::unavailable(
                            ReattachOutcome::RetainedButUnreadable,
                        )
                    }
                };
                let lanes = match self.store.recover_publish_queue_lanes(intent_id) {
                    Ok(lanes) => lanes,
                    Err(_) => {
                        return ReceiptReplayPage::unavailable(
                            ReattachOutcome::RetainedButUnreadable,
                        )
                    }
                };
                if self.store.recover_route_revisions(intent_id).is_err() {
                    return ReceiptReplayPage::unavailable(ReattachOutcome::RetainedButUnreadable);
                }
                (attempts, details, lanes)
            }
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let mut replay = Vec::new();
        let retained_status =
            if receipt_state == ReceiptState::Signed && !self.pending.contains(&id) {
                Some(WriteFact::Outcome(WriteOutcome::Settled))
            } else {
                Self::retained_receipt_fact(&receipt)
            };
        let terminal_status = match retained_status {
            Some(status @ WriteFact::Outcome(_)) => Some(status),
            Some(status) => {
                replay.push((ReceiptReplayFactKey::ReceiptStatus, status));
                None
            }
            None => None,
        };
        // A reattaching app is told which of the two unsigned states this
        // obligation is in, exactly as the queue projection reports it
        // (#1261): a signer holding the request is not a signer nobody has.
        if receipt_state == ReceiptState::Accepted
            && self
                .pending
                .get(&projection_id)
                .is_some_and(|pending| !pending.already_signed)
        {
            replay.push((
                ReceiptReplayFactKey::AwaitingCapability,
                WriteFact::Signing(Self::signing_park(
                    receipt.expected_pubkey,
                    self.pending.get(&projection_id),
                )),
            ));
        }
        // The routing park is retained and replayed the same way the signer
        // park is. An app that restarts, reattaches to an id it persisted,
        // and is told nothing has learned nothing -- a park nobody can see
        // again is indistinguishable from data loss. The REASON replays with
        // it, off the same reducer memory a live resolution writes, so a
        // reattached park says who it is waiting for rather than only that it
        // is waiting.
        if let Some(pending) = self
            .pending
            .get(&projection_id)
            .filter(|pending| pending.durable_routes.is_empty() && !pending.route_complete)
        {
            replay.push((
                ReceiptReplayFactKey::Destinations,
                WriteFact::Destinations {
                    relays: BTreeSet::new(),
                    complete: false,
                    awaiting_author_routes: pending.route_needs.clone(),
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
                let event_id = attempt.event_id;
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
                                        event_id,
                                        relay: attempt.relay.clone(),
                                        state: RelayState::Waiting(RelayWaiting::NotConnected),
                                    },
                                ));
                            }
                            HandoffEvidence::Written => replay.push((
                                replay_key(ReceiptAttemptReplayPhase::Handoff),
                                WriteFact::Relay {
                                    event_id,
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
                                    event_id,
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
                                    event_id,
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
                        event_id,
                        relay: attempt.relay,
                        state: RelayState::Published,
                    },
                    PublishQueueAttemptOutcome::Rejected(reason) => WriteFact::Relay {
                        event_id,
                        relay: attempt.relay,
                        state: RelayState::Rejected { reason },
                    },
                    PublishQueueAttemptOutcome::GaveUp => WriteFact::Relay {
                        event_id,
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
                                event_id: lane.key.event_id,
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
                                event_id: lane.key.event_id,
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
                                event_id: lane.key.event_id,
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
                                event_id: lane.key.event_id,
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
        if let Some(status) = terminal_status {
            replay.push((ReceiptReplayFactKey::ReceiptStatus, status));
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
            frozen_id: Some(receipt_event_id),
            isolated_fact_cursors,
        }
    }

    /// #961: advance one runtime registration's durable checkpoint for one mailbox-
    /// accepted live fact. The cursor moves only for a matching retained fact;
    /// transient live-only statuses deliberately leave it unchanged.
    pub(in crate::core) fn receipt_cursor_after_status(
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

    pub(in crate::core) fn receipt_is_live(&self, id: ReceiptId) -> bool {
        self.pending.contains(&id)
            || self
                .store
                .reattach_receipt(id.0)
                .ok()
                .flatten()
                .is_some_and(|receipt| {
                    matches!(
                        receipt.payload,
                        PublishQueueReceiptPayload::ReplaceableOperation {
                            state: nmp_store::ReplaceableOperationReceiptState::Contributing {
                                current: Some(_),
                            },
                            ..
                        }
                    )
                })
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
    /// Superseded safety receipts are automatically age/count bounded. Other
    /// terminal receipt classes remain app-removable and #46 continues to own
    /// their general retention policy.
    pub(in crate::core) fn publish_queue_entries(
        &self,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PersistenceError> {
        let receipts = self
            .store
            .publish_queue_receipts_after(after.map(|id| id.0), limit)?;
        validate_publish_queue_page(
            after.map(|id| id.0),
            limit,
            receipts.iter().map(|receipt| receipt.receipt_id),
        )?;
        self.project_publish_queue_entries(receipts)
    }

    /// Look up the currently open obligations for one canonical event id
    /// (#903). The in-memory reverse index is rebuilt from durable intents at
    /// boot and updated at acceptance, so this does not scan retained history.
    pub(in crate::core) fn publish_queue_entries_for_event(
        &self,
        event_id: EventId,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PersistenceError> {
        let Some(ids) = self.pending.receipts_for_event(&event_id) else {
            return Ok(Vec::new());
        };
        let receipts = ids
            .iter()
            .copied()
            .filter(|id| after.is_none_or(|after| *id > after))
            .take(usize::from(limit))
            .map(|id| {
                self.store.reattach_receipt(id.0)?.ok_or_else(|| {
                    PersistenceError::new(format!(
                        "active event index names missing receipt {}",
                        id.0
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.project_publish_queue_entries(receipts)
    }

    fn project_publish_queue_entries(
        &self,
        receipts: Vec<PublishQueueReceipt>,
    ) -> Result<Vec<PublishQueueEntry>, PersistenceError> {
        let mut entries = Vec::with_capacity(receipts.len());
        for receipt in receipts {
            let id = ReceiptId(receipt.receipt_id);
            if let PublishQueueReceiptPayload::ReplaceableOperation {
                state:
                    nmp_store::ReplaceableOperationReceiptState::Contributing {
                        current: Some(current),
                    },
                ..
            } = &receipt.payload
            {
                let owner_pending = self
                    .pending
                    .receipts_for_event(&current.materialization.event_id)
                    .and_then(|receipts| {
                        receipts
                            .iter()
                            .filter_map(|receipt| self.pending.get(receipt))
                            .min_by_key(|pending| pending.intent_id)
                    });
                entries.push(PublishQueueEntry {
                    receipt_id: id,
                    event_id: current.materialization.event_id,
                    pubkey: receipt.expected_pubkey,
                    accepted_at: receipt.accepted_at.unwrap_or_else(|| Timestamp::from(0u64)),
                    signing: match current.sig_state {
                        IntentSigState::Signed => SigningState::Signed {
                            event_id: current.materialization.event_id,
                        },
                        IntentSigState::AwaitingSigner | IntentSigState::Pending => {
                            SigningState::AwaitingSigner {
                                pubkey: receipt.expected_pubkey,
                            }
                        }
                    },
                    relays: owner_pending
                        .map(|pending| pending.durable_routes.clone())
                        .unwrap_or_default(),
                    route_complete: owner_pending.is_some_and(|pending| pending.route_complete),
                    relay_states: owner_pending
                        .map(|pending| {
                            self.relay_states_for(pending.intent_id, pending.signing_pubkey)
                        })
                        .transpose()?
                        .unwrap_or_default(),
                    outcome: None,
                });
                continue;
            }
            // A cohort that closed reports the generation it delivered and
            // then stops: no route, no lanes, no open work, and no way back
            // in. Every other replaceable-operation shape is terminal
            // acceptance state the ordinary arms below cannot describe
            // either, so they stay a refused projection rather than a
            // silently wrong one.
            if let PublishQueueReceiptPayload::ReplaceableOperation {
                state: nmp_store::ReplaceableOperationReceiptState::Settled { materialization },
                ..
            } = &receipt.payload
            {
                entries.push(PublishQueueEntry {
                    receipt_id: id,
                    event_id: materialization.event_id,
                    pubkey: receipt.expected_pubkey,
                    accepted_at: receipt.accepted_at.unwrap_or_else(|| Timestamp::from(0u64)),
                    signing: SigningState::Signed {
                        event_id: materialization.event_id,
                    },
                    relays: BTreeSet::new(),
                    route_complete: true,
                    relay_states: Vec::new(),
                    outcome: Some(WriteOutcome::Settled),
                });
                continue;
            }
            let (event_id, state) = match &receipt.payload {
                PublishQueueReceiptPayload::Event { event_id, state } => (*event_id, *state),
                PublishQueueReceiptPayload::ReplaceableOperation { .. } => {
                    return Err(PersistenceError::new(
                        "event publish-queue index names replaceable-operation receipt",
                    ));
                }
            };
            let pending = self.pending.get(&id);
            let signing = match state {
                ReceiptState::Signed => SigningState::Signed { event_id },
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
            let outcome = match state {
                ReceiptState::Cancelled => Some(WriteOutcome::NotSent(NotSentReason::Cancelled)),
                ReceiptState::Superseded => Some(WriteOutcome::Superseded),
                ReceiptState::Refused(reason) => Some(WriteOutcome::Refused(reason)),
                ReceiptState::NoDestination => Some(WriteOutcome::NoDestination),
                ReceiptState::Accepted | ReceiptState::Signed => match pending {
                    // A completed answer that truly named nobody is terminal
                    // even when its close transaction failed and left the
                    // open-work row available here. A route revision that did
                    // not persist is NOT that: it leaves `route_complete`
                    // false, so the obligation stays open work instead of
                    // reading as this verdict.
                    Some(pending)
                        if pending.route_complete && pending.durable_routes.is_empty() =>
                    {
                        Some(WriteOutcome::NoDestination)
                    }
                    // Still open work: no outcome yet.
                    Some(_) => None,
                    // The open-work row is gone and every lane finished.
                    None => (state == ReceiptState::Signed).then_some(WriteOutcome::Settled),
                },
                ReceiptState::Compensated => {
                    Some(WriteOutcome::NotSent(NotSentReason::SignerRefused))
                }
            };
            // Destinations and lane states come from the DURABLE rows, keyed
            // by the receipt's own intent id, because settlement deletes the
            // pending row while leaving every route revision and lane behind.
            // Reading them through `pending` made a write report zero relays
            // at the exact moment it finished — an app showing "published to
            // 3 of 5" went blank on becoming 5 of 5.
            //
            // A receipt-only refusal (`intent_id: None`) never gained an
            // intent row, so it never routed and never owned a lane; empty is
            // the true answer there and not a missing one.
            let (relays, relay_states) = match receipt.intent_id {
                Some(intent_id) => (
                    self.durable_relays_for(intent_id)?,
                    self.relay_states_for(intent_id, receipt.expected_pubkey)?,
                ),
                None => (BTreeSet::new(), Vec::new()),
            };
            entries.push(PublishQueueEntry {
                receipt_id: id,
                event_id,
                pubkey: receipt.expected_pubkey,
                accepted_at: receipt.accepted_at.unwrap_or_else(|| Timestamp::from(0u64)),
                signing,
                relays,
                route_complete: pending.is_none_or(|pending| pending.route_complete),
                relay_states,
                outcome,
            });
        }
        Ok(entries)
    }

    /// Project one intent's durable lane rows into the per-relay picture an
    /// app reads.
    ///
    /// Keyed by `intent_id` and never by the in-memory `PendingWrite`, because
    /// settlement DELETES that row while the lane, route-revision and
    /// attempt-detail rows survive it (only `remove_publish_queue_entry`
    /// reclaims those). Reading through the pending row is what made a
    /// finished write report zero relays at the exact moment it succeeded.
    fn relay_states_for(
        &self,
        intent_id: IntentId,
        signing_pubkey: PublicKey,
    ) -> Result<Vec<(RelayUrl, RelayState)>, PersistenceError> {
        let lanes = self.store.recover_publish_queue_lanes(intent_id)?;
        // Attempt evidence is only consulted for a lane that is actually in
        // flight, so a settled or backing-off intent pays no extra read.
        let details = if lanes
            .iter()
            .any(|lane| matches!(lane.state, PublishQueueLaneState::InFlight { .. }))
        {
            self.store.recover_attempt_details(intent_id)?
        } else {
            Vec::new()
        };
        let attempt_detail = |relay: &RelayUrl, ordinal: u64| {
            details
                .iter()
                .find(|detail| detail.relay == *relay && detail.ordinal == ordinal)
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
                    // `Eligible` covers two live situations, because
                    // `schedule_ready` deliberately does NOT commit a durable
                    // transition when it finds an eligible lane whose session
                    // is gone — that would cost one fsync per eligible lane on
                    // every pass of a disconnected engine, which at boot is
                    // the whole queue.
                    //
                    // Connectivity is process-local anyway, so it is what
                    // separates them here, read from the same
                    // `connected_relays` the scheduler itself gates on. A lane
                    // with a live session is genuinely just queued; one
                    // without is genuinely not connected. Reporting BOTH as
                    // not connected invented a fault for the first, and
                    // reporting both as merely queued would hide a real one
                    // for the second.
                    PublishQueueLaneState::Eligible { since } => {
                        let session = RelaySessionKey::new(
                            lane.key.relay.clone(),
                            Some(signing_pubkey),
                        );
                        if self.connected_relays.contains(&session) {
                            RelayState::Waiting(RelayWaiting::Eligible { since })
                        } else {
                            RelayState::Waiting(RelayWaiting::NotConnected)
                        }
                    }
                    // An attempt is running. Which of the two honest answers
                    // it gets turns on transport's handoff evidence, which
                    // `start_lane_attempt` and `record_lane_handoff` each
                    // commit in the SAME transaction as the lane state — so
                    // the evidence a given phase needs is always present, and
                    // its absence is a durable inconsistency rather than a
                    // state to guess at.
                    PublishQueueLaneState::InFlight { ordinal, ref phase } => {
                        let detail = attempt_detail(&lane.key.relay, ordinal).ok_or_else(|| {
                            PersistenceError::new("in-flight lane has no attempt detail row")
                        })?;
                        match phase {
                            PublishQueueInFlightPhase::AwaitingHandoff => {
                                let started_at = detail.started_at.ok_or_else(|| {
                                    PersistenceError::new("started attempt has no start time")
                                })?;
                                RelayState::Attempting {
                                    attempt: ordinal,
                                    started_at,
                                }
                            }
                            // A lane reaches AwaitingAck on `Written` AND on
                            // `Ambiguous` (see `on_write_handoff`), and only
                            // the first is proof. `Sent` claims socket write
                            // + flush, so an ambiguous handoff must not
                            // borrow it.
                            PublishQueueInFlightPhase::AwaitingAck { .. } => {
                                let handoff = detail.handoff.as_ref().ok_or_else(|| {
                                    PersistenceError::new(
                                        "acked-awaiting lane has no handoff evidence",
                                    )
                                })?;
                                match handoff.result {
                                    HandoffEvidence::Written => RelayState::Sent {
                                        attempt: ordinal,
                                        written_at: handoff.at,
                                    },
                                    HandoffEvidence::Ambiguous | HandoffEvidence::NotHandedOff => {
                                        let started_at = detail.started_at.ok_or_else(|| {
                                            PersistenceError::new(
                                                "started attempt has no start time",
                                            )
                                        })?;
                                        RelayState::Attempting {
                                            attempt: ordinal,
                                            started_at,
                                        }
                                    }
                                }
                            }
                        }
                    }
                    PublishQueueLaneState::Terminal { ref outcome, .. } => match outcome {
                        PublishQueueTerminalOutcome::Acked => RelayState::Published,
                        PublishQueueTerminalOutcome::Rejected(reason) => RelayState::Rejected {
                            reason: reason.clone(),
                        },
                        PublishQueueTerminalOutcome::GaveUp => RelayState::GaveUp,
                        PublishQueueTerminalOutcome::AuthDenied(denial) => RelayState::AuthFailed {
                            pubkey: signing_pubkey,
                            source: match denial.source {
                                StoredAuthDenialSource::Policy => AuthDenialSource::Policy,
                                StoredAuthDenialSource::Signer => AuthDenialSource::Signer,
                                StoredAuthDenialSource::Relay => AuthDenialSource::Relay,
                            },
                            reason: denial.reason.clone(),
                        },
                    },
                };
                Ok((lane.key.relay, state))
            })
            .collect()
    }

    /// Every relay this intent durably resolved to, read from the committed
    /// route revisions rather than the in-memory `durable_routes` mirror.
    ///
    /// This is the same union crash recovery rebuilds
    /// `PendingWrite::durable_routes` from, so an open write's answer is
    /// unchanged — and a settled write, whose pending row no longer exists,
    /// finally gets one instead of reporting no destinations at all.
    fn durable_relays_for(
        &self,
        intent_id: IntentId,
    ) -> Result<BTreeSet<RelayUrl>, PersistenceError> {
        Ok(self
            .store
            .recover_route_revisions(intent_id)?
            .iter()
            .flat_map(|revision| revision.relays.iter().cloned())
            .collect())
    }

    /// Forget one queue entry (#1039).
    ///
    /// This is a real TERMINATION path, not housekeeping: a write parked
    /// forever on a signer that never attached, and a permanently-failed
    /// refused entry, end no other way. An entry whose obligation is still
    /// open is refused — `cancel_write` it first, then remove the terminal
    /// receipt cancellation leaves behind. Cancelling is what releases the
    /// obligation and compensates the optimistic row the write promised;
    /// this door only forgets what is already terminal, which is why it can
    /// ask the cheap question ("is this receipt still in `pending`?") and
    /// leave the store's own open-intent check authoritative for the rest.
    pub(in crate::core) fn remove_publish_queue_entry(
        &mut self,
        id: ReceiptId,
    ) -> Result<(), RemoveQueueEntryError> {
        if self.pending.contains(&id) {
            return Err(RemoveQueueEntryError::StillActive { receipt_id: id });
        }
        match self.store.remove_publish_queue_entry(id.0) {
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
        }
    }
}

/// Defend the public bounded-page contract at the reducer boundary after the
/// concrete Redb range read. Redb performs the bounded read directly; this
/// second check prevents an oversized, overlapping, duplicated, or reordered
/// page from crossing the reducer boundary.
fn validate_publish_queue_page(
    after: Option<u64>,
    limit: u8,
    receipt_ids: impl IntoIterator<Item = u64>,
) -> Result<(), PersistenceError> {
    let mut previous = after;
    let mut count = 0usize;
    for receipt_id in receipt_ids {
        count += 1;
        if count > usize::from(limit) {
            return Err(PersistenceError::new(format!(
                "publish queue backend returned more than limit {limit}"
            )));
        }
        if previous.is_some_and(|previous| receipt_id <= previous) {
            return Err(PersistenceError::new(format!(
                "publish queue backend returned receipt {receipt_id} after {previous:?}"
            )));
        }
        previous = Some(receipt_id);
    }
    Ok(())
}

