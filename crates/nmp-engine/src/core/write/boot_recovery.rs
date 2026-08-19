//! Boot recovery: rebuilding the scheduler's lane and publish-queue-deadline input from durable state.

use super::*;

impl CoreState {
    pub(in crate::core) fn consume_due_publish_queue_deadlines(
        &mut self,
        now: Timestamp,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        loop {
            let due = match self
                .store
                .due_publish_queue_deadlines(now, DEADLINE_READ_BATCH)
            {
                Ok(due) => due,
                Err(_) => {
                                        break;
                }
            };
            if due.is_empty() {
                break;
            }
            for deadline in due {
                let id = self.pending.receipt_for_intent(deadline.key.intent_id);
                let lane = self
                    .store
                    .recover_publish_queue_lanes(deadline.key.intent_id)
                    .ok()
                    .and_then(|lanes| {
                        lanes.into_iter().find(|lane| {
                            lane.key == deadline.key && lane.revision == deadline.lane_revision
                        })
                    });
                let Some(lane) = lane else {
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
                        ) {}
                    }
                    _ => {}
                }
            }
        }
        effects.extend(self.schedule_ready(now));
        effects
    }

    pub(in crate::core) fn recover_on_boot(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        // #790: the journal is now allowed to say "unreadable" instead of
        // panicking the host mid-boot. An `Err` here is NOT "nothing is
        // open": the durable obligation set could not be proven, so this
        // fabricates nothing from it -- no receipt, no lane, no signer
        // request, no route resolution, no wire effect -- and leaves the
        // pending writes owner, lane index included, in the untrustworthy
        // state it must be in for a set that was never rebuilt. The one-shot
        // #122 degradation is the whole visible outcome.
        let mut recovered = match self.store.recover_publish_queue() {
            Ok(recovered) => recovered,
            Err(_error) => {
                // Nothing was rebuilt, so there is no intent to retry a
                // bootstrap for. A later engine-supervised store
                // reconstruction re-enters this whole recovery door.
                        return effects;
            }
        };
        match self.reconcile_recovered_semantic_sources(&recovered) {
            Ok(true) => match self.store.recover_publish_queue() {
                Ok(refreshed) => recovered = refreshed,
                Err(_error) => {
                    return effects;
                }
            },
            Ok(false) => {}
            Err(_error) => {
                return effects;
            }
        }
        let mut recovered_ids = Vec::new();
        let mut recovered_semantic_owners = Vec::new();
        let mut recovered_semantic_coordinates = Vec::new();
        // This is the one deterministic, from-scratch rebuild of `pending`
        // (and, with it, every index derived from `pending`) -- the exact
        // moment the lane index can be trusted again regardless of what
        // happened in a prior process (epic #507 finding E5).
        // Every gap recorded against the previous `pending` set refers to
        // receipt ids this rebuild is about to re-derive from the store.
        // Carrying them across would retry on behalf of a projection that no
        // longer exists; the rebuild below re-registers whatever still fails.

        for intent in recovered {
            if let nmp_store::PublishQueueWork::ReplaceableOperation {
                coordinate,
                materialization: Some(materialization),
            } = &intent.work
            {
                let snapshot = match self.store.replaceable_operation_snapshot(coordinate) {
                    Ok(Some(snapshot)) => snapshot,
                    Ok(None) => continue,
                    Err(_error) => {
                        continue;
                    }
                };
                let Some(generation) = snapshot.current.generation.as_ref() else {
                    continue;
                };
                if !recovered_semantic_coordinates.contains(coordinate) {
                    recovered_semantic_coordinates.push(coordinate.clone());
                }
                if generation.materialization != materialization.receipt.materialization {
                    continue;
                }
                let row = match self.store.query(
                    &nostr::Filter::new().id(materialization.receipt.materialization.event_id),
                ) {
                    Ok(rows) => rows.into_iter().next(),
                    Err(_error) => {
                        continue;
                    }
                };
                let Some(row) = row else { continue };
                let parsed_routing = Self::parse_routing_snapshot(&materialization.routing);
                let id = ReceiptId(intent.receipt_id);
                let already_signed = materialization.receipt.sig_state == IntentSigState::Signed;
                let is_owner = generation.members.first() == Some(&intent.intent_id);
                self.pending.insert(
                    id,
                    PendingWrite {
                        target: PendingWriteTarget::ReplaceableOperation(Box::new(
                            ReplaceableMaterializationTarget {
                                coordinate: coordinate.clone(),
                                expected_source_revision: snapshot.current.source_revision.clone(),
                                expected_program_digest: snapshot.current.program_digest,
                                expected_materialization: generation
                                    .materialization
                                    .materialization_id,
                                expected_event_id: generation.materialization.event_id,
                            },
                        )),
                        routing: parsed_routing
                            .clone()
                            .unwrap_or(WriteRouting::Explicit(Vec::new())),
                        routing_valid: parsed_routing.is_some(),
                        intent_id: intent.intent_id,
                        accepted_at: intent.accepted_at,
                        signing_pubkey: intent.expected_pubkey,
                        frozen: row.event.clone(),
                        already_signed,
                        sign_request_in_flight: false,
                        sign_generation: 0,
                        event_id: already_signed.then_some(row.event.id),
                        pending_relays: BTreeSet::new(),
                        attempt_ordinals: BTreeMap::new(),
                        lane_projection: LaneWorkerProjection::default(),
                        durable_routes: BTreeSet::new(),
                        route_complete: false,
                        destinations_reported: false,
                        route_needs: BTreeSet::new(),
                    },
                );
                self.pending.remember_indexes(
                    id,
                    Some(intent.intent_id),
                    generation.materialization.event_id,
                );
                recovered_ids.push(id);
                if is_owner {
                    recovered_semantic_owners.push((id, intent.intent_id, already_signed));
                }
                continue;
            }
            let Some((frozen, _, routing_snapshot, sig_state)) = intent.event_work() else {
                // #841 owns runtime orchestration for durable replaceable
                // operations. Their reference-only journal arm must never
                // enter the ordinary signer/routing lifecycle without a body.
                continue;
            };
            let frozen = frozen.clone();
            let routing_snapshot = routing_snapshot.to_owned();
            if frozen.kind == nostr::Kind::Authentication {
                let id = ReceiptId(intent.receipt_id);
                let reason = "recovered kind:22242 ordinary write quarantined from AUTH ownership"
                    .to_string();
                self.quarantined_auth_receipts.insert(
                    id,
                    QuarantinedWrite {
                        intent_id: intent.intent_id,
                        frozen: frozen.clone(),
                    },
                );
                self.pending.index_receipt_under_event(frozen.id, id);
                effects.push(Effect::EmitReceipt(
                    id,
                    WriteFact::Signing(SigningState::Refused { reason }),
                ));
                continue;
            }
            let parsed_routing = Self::parse_routing_snapshot(&routing_snapshot);
            let routing_valid = parsed_routing.is_some();
            // An unreadable row is retained exactly as written and never
            // resolved (`routing_valid == false` gates every send path). The
            // in-memory stand-in is the one value that cannot contact a
            // relay even if that gate were ever bypassed — guessing `Auto`
            // here would republish an old obligation to relays nobody chose
            // for it.
            let routing = parsed_routing.unwrap_or(WriteRouting::Explicit(Vec::new()));
            let id = ReceiptId(intent.receipt_id);
            let already_signed = sig_state == IntentSigState::Signed;
            self.pending.insert(
                id,
                PendingWrite {
                    target: PendingWriteTarget::Event,
                    routing,
                    routing_valid,
                    intent_id: intent.intent_id,
                    // The DURABLE acceptance instant, replayed verbatim. It is
                    // what makes a stalled-write projection identical either
                    // side of a restart: nothing here is a process-local
                    // stopwatch that a reopen would reset to zero.
                    destinations_reported: false,
                    accepted_at: intent.accepted_at,
                    signing_pubkey: intent.expected_pubkey,
                    frozen: frozen.clone(),
                    already_signed,
                    sign_request_in_flight: false,
                    sign_generation: 0,
                    event_id: already_signed.then_some(frozen.id),
                    pending_relays: BTreeSet::new(),
                    attempt_ordinals: BTreeMap::new(),
                    lane_projection: LaneWorkerProjection::default(),
                    durable_routes: BTreeSet::new(),
                    route_complete: false,
                    route_needs: BTreeSet::new(),
                },
            );
            self.pending
                .remember_indexes(id, Some(intent.intent_id), frozen.id);
            recovered_ids.push(id);

            if !already_signed {
                continue;
            }

            let revisions = match self.store.recover_route_revisions(intent.intent_id) {
                Ok(revisions) => revisions,
                Err(_error) => {
                    // This intent's durable route set is exactly what could
                    // not be read, so `bootstrap_publish_queue_lanes` cannot
                    // run for it this boot and the reverse index never learns
                    // whatever lanes it may already own. Skip it: the intent
                    // makes no progress until the next boot re-runs recovery
                    // against rows this one could not read. Nothing durable
                    // is lost -- the rows are untouched -- and no retry is
                    // registered, because a store that cannot answer now is
                    // not made answerable by asking again on a tick.
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
                let answer = self.resolve_routes(&self.pending[&id].routing, &frozen);
                let new_routes = answer
                    .relays
                    .difference(&durable_relays)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let mut route_persisted = true;
                if !new_routes.is_empty() {
                    match self.commit_route_revision(intent.intent_id, answer.relays.clone()) {
                        // These exact URLs are not claimed to survive a crash,
                        // so routing is NOT complete: the answer exists but
                        // nothing durable holds it, and the next pass resolves
                        // and commits again. Reporting completeness here is
                        // what would let an empty durable set be read as the
                        // terminal `NoDestination` verdict.
                        Err(_) => route_persisted = false,
                        Ok(_) => {
                            durable_relays.extend(answer.relays.iter().cloned());
                        }
                    }
                }
                if let Some(pending) = self.pending.get_mut(&id) {
                    pending.durable_routes = durable_relays.clone();
                    pending.route_complete = answer.complete && route_persisted;
                    // Needs are STATELESS: nothing about them was recovered
                    // from the journal, they were simply re-derived by the
                    // resolution above. That is what makes a crash cost a
                    // declared need nothing.
                    pending.route_needs = answer.author_route_needs;
                }
            }

            let lanes =
                match self.bootstrap_projected_lanes(intent.intent_id) {
                    Ok(lanes) => lanes,
                    Err(_error) => {
                        // Same reasoning as the `recover_route_revisions`
                        // error above: this is the sole call that teaches the
                        // reverse index this intent's lanes, so a failure
                        // here is an audit hole, not a "no lanes" fact --
                        // degrade rather than guess (epic #507 finding E5).
                        // The projection door has already recorded the
                        // retryable gap that gets this intent out of its
                        // conservative retention (#1000).
                            continue;
                    }
                };
            self.open_bootstrapped_lanes(id, intent.expected_pubkey, lanes, &mut effects);
        }

        for (id, intent_id, already_signed) in recovered_semantic_owners {
            // A semantic successor installs its current-generation lanes and
            // route union atomically before its signature exists. Restore
            // that union for both recovery states: otherwise an unsigned
            // successor reaches `on_signed` with an empty durable route set,
            // mistakes every persisted route for a new addition, and tries
            // to bootstrap current lanes against predecessor attempt history.
            let revisions = match self.store.recover_route_revisions(intent_id) {
                Ok(revisions) => revisions,
                Err(_error) => {
                    continue;
                }
            };
            let durable_relays = revisions
                .iter()
                .flat_map(|revision| revision.relays.iter().cloned())
                .collect::<BTreeSet<_>>();
            if let Some(pending) = self.pending.get_mut(&id) {
                pending.durable_routes = durable_relays;
            }
            if already_signed {
                let signing_pubkey = self.pending[&id].signing_pubkey;
                let event_id = self.pending[&id].frozen.id;
                match self.recover_semantic_generation_lanes(intent_id, event_id) {
                    Ok(lanes) => {
                        self.open_bootstrapped_lanes(id, signing_pubkey, lanes, &mut effects)
                    }
                    Err(_) => {}
                }
            } else if let Some(pending) = self.pending.get_mut(&id) {
                pending.sign_request_in_flight = true;
                pending.sign_generation = pending.sign_generation.saturating_add(1);
                effects.push(Effect::RequestSign(
                    id,
                    pending.sign_generation,
                    unsigned_from_frozen(&pending.frozen),
                ));
            }
        }

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
        self.resync_route_needs(&mut effects);
        self.rebuild_stalled_write_cache();
        // A process can die after its last lane went terminal but before the
        // cohort close committed. Recovery re-asks for every semantic
        // coordinate it rebuilt; the store's exact-generation CAS makes a
        // premature ask a no-op.
        for coordinate in recovered_semantic_coordinates {
            self.try_close_semantic_cohort(&coordinate, &mut effects);
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
        id: ReceiptId,
        signing_pubkey: PublicKey,
        lanes: Vec<PublishQueueLane>,
        effects: &mut Vec<Effect>,
    ) {
        for lane in lanes {
            // The recovered write lane's worker demand is the intent's
            // identity-scoped authenticated session (#8 U2); recovery
            // redials exactly the session the lane will publish on. The
            // signing identity was frozen at acceptance, never re-read from
            // the mutable current account.
            let session =
                RelaySessionKey::new(lane.key.relay.clone(), Some(signing_pubkey));
            match lane.state {
                PublishQueueLaneState::InFlight {
                    ordinal,
                    phase: PublishQueueInFlightPhase::AwaitingHandoff,
                } => {
                    // A process that did not submit this attempt holds no
                    // correlation for it, which is precisely the fact that
                    // says its handoff can never arrive — the same rule, and
                    // the same owner, as the mid-process case (#1316).
                    // Running it HERE rather than leaving it to the
                    // `schedule_ready` that closes this boot is what lets
                    // this boot's own deadline sweep promote the replacement
                    // attempt: the reclaimed lane is eligible as of `now`,
                    // and the sweep has not run yet. No session is asked for
                    // — the ordinary eligible path does that, and only once
                    // the reclaim actually committed, so an obligation whose
                    // attempt evidence is unreadable still claims nothing.
                    let now = self.clock;
                    self.reclaim_orphaned_handoff(id, &lane, ordinal, now);
                }
                PublishQueueLaneState::WaitingConnection
                | PublishQueueLaneState::Eligible { .. }
                | PublishQueueLaneState::Transient { .. }
                | PublishQueueLaneState::InFlight {
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
                    //
                    // If that reset does not commit, this lane makes no
                    // progress until the next boot re-runs this same
                    // recovery: no relay is warmed, since a connection
                    // cannot wake a row still durably `WaitingAuth`, and no
                    // retry is registered. That lost progress is the ruled
                    // price of a failed store write (#1934, #1945) — the
                    // durable row is exactly as it was, and recovery is
                    // idempotent, so the next boot resumes from it.
                    if self
                        .commit_lane_waiting(&lane.key, lane.revision, false)
                        .is_ok()
                    {
                        effects.push(Effect::EnsureWriteRelay(session));
                    }
                }
                PublishQueueLaneState::Terminal { .. } => {}
            }
        }
    }
}
