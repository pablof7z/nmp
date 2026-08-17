//! Live-query planning, relay repair, and row projection.
//!
//! This module owns subscription lifetimes, router recompilation, discovery,
//! NIP-77 handoff/repair, and committed-store mutations projected to observers.

use super::attribution::CompletedCoverageClaim;
use super::observation::StoredEvents;
use super::*;

/// One observation's merged current row set plus its per-BRANCH acquisition
/// evidence, indexed by canonical branch order (#1108). This is the internal
/// snapshot `refresh_observation` diffs against the observation's own last
/// delivered state; it is never handed to a caller or an effect directly.
type ObservationProjection = (BTreeMap<EventId, Row>, Vec<AcquisitionEvidence>);

/// Canonical durable claim identities for one exact logical atom. Lifecycle
/// ownership remains keyed by the original `DemandKey`; only coverage
/// refresh expands multi-author outbox atoms to their per-author claims.
fn coverage_claim_keys(atom: &ContextualAtom) -> BTreeSet<CoverageKey> {
    nmp_store::coverage_claim_atoms(atom)
        .iter()
        .map(nmp_store::coverage_key)
        .collect()
}

fn release_ref<K: Ord + Copy>(refs: &mut BTreeMap<K, usize>, key: K) -> bool {
    let remove = refs.get_mut(&key).is_some_and(|count| {
        *count = count
            .checked_sub(1)
            .expect("per-handle ownership refcount cannot underflow");
        *count == 0
    });
    if remove {
        refs.remove(&key);
    }
    remove
}

#[derive(Clone, Copy)]
pub(super) enum PlanDeltaMode {
    Full,
    Incremental,
}

/// Which NIP-77 frame the runtime attempted to hand to a relay worker, for
/// [`EngineCore::on_nip77_handoff`] (issue #775).
///
/// Closed and exhaustive: each variant names the exact reducer state that
/// advanced before the frame existed, and therefore the exact state a
/// transport refusal has to consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nip77Frame {
    /// `Effect::StartProbe` -- a capability probe's throwaway `NEG-OPEN`.
    /// The prober moved the relay to `Probing` and recorded a pending wire id.
    Probe,
    /// `Effect::NegOpen` -- a real reconciliation's opening `NEG-OPEN`. A
    /// `NegSession` and a pending request-evidence record exist.
    Open,
    /// `Effect::NegMsg` -- the next round of an already-open reconciliation.
    /// The reconciler has already consumed the relay's message and advanced.
    Continue,
}

impl EngineCore {
    /// The sole EngineCore authority crossing the durable coverage-write
    /// boundary. Callers own completion or retry policy, but every path
    /// commits one request-scoped batch atomically through this door.
    fn record_request_coverage_batch(
        &mut self,
        batch: &[(ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        self.store.record_coverage(batch)
    }

    /// Mint a NIP-77 role wire id nobody has ever been handed before (#932).
    ///
    /// Every call advances [`Self::next_nip77_incarnation`], so re-deriving a
    /// role subscription for the same plan id, role, and filter after the
    /// previous one was closed and discarded yields a DIFFERENT 64-hex wire
    /// string. A straggler EOSE addressed to the closed incarnation therefore
    /// resolves to nothing instead of popping the reopened request's fresh
    /// attribution FIFO -- see [`nip77_role_sub_id`] for the full reasoning.
    fn mint_nip77_role_sub_id(
        &mut self,
        plan_sub_id: &SubId,
        role: u8,
        filter: &ConcreteFilter,
    ) -> SubId {
        let incarnation = self.next_nip77_incarnation;
        self.next_nip77_incarnation = self.next_nip77_incarnation.wrapping_add(1);
        nip77_role_sub_id(plan_sub_id, role, filter, incarnation)
    }

    // ---- subscribe / unsubscribe / re-root ------------------------------

    /// Open ONE observation over every canonical branch of `query` (#1108),
    /// transactionally (#1153).
    ///
    /// The open is all-or-nothing across two failure classes. A branch whose
    /// graph cannot be built withdraws every branch already opened; a
    /// canonical row projection that cannot be read withdraws the whole
    /// observation. Either way the refusal owns no handle, no demand atom, no
    /// mailbox and no wire request, and no branch of it was ever installed.
    /// On success the branches share one projection, one evidence vector in
    /// canonical branch order, and one cancellation.
    pub fn open_observation(
        &mut self,
        query: LiveQuery,
        now: Timestamp,
    ) -> ObservationOpen<ObservationId, RowsSeed> {
        let mut effects = Vec::new();
        // Graph construction can read the store (a `Derived` binding resolves
        // its inner query). The resolver transaction discards every partially
        // built graph node on failure, so this refusal owns no handle or
        // demand atom -- and the branches opened BEFORE the failing one are
        // withdrawn here for the same reason.
        let mut opened: Vec<QueryHandle> = Vec::new();
        for branch in query.branches() {
            match self.resolver.subscribe(&self.store, branch.clone()) {
                SubscribeOutcome::Opened { handle, delta } => {
                    self.consume_resolver_delta(delta);
                    opened.push(handle);
                }
                SubscribeOutcome::Refused { error, delta } => {
                    self.consume_resolver_delta(delta);
                    for handle in opened {
                        let delta = self.resolver.unsubscribe(handle.id());
                        self.consume_resolver_delta(delta);
                    }
                    self.flush_consumed_resolver_closes(&mut effects);
                    let reason = format!("canonical query resolution failed: {error}");
                    self.degrade_store(error, &mut effects);
                    return ObservationOpen::Refused { reason, effects };
                }
            }
        }

        // Every branch's freshness decision is taken before the observation
        // exists, so a store read that cannot answer unwinds exactly like a
        // failed resolve above (#763): the alternative is deciding `Live` on
        // a failed coverage read and reporting it as a policy decision.
        let mut acquisitions = Vec::with_capacity(opened.len());
        for index in 0..opened.len() {
            let (id, freshness) = (opened[index].id(), opened[index].freshness());
            match self.decide_handle_acquisition(id, freshness, now) {
                Ok(acquisition) => acquisitions.push(acquisition),
                Err(error) => {
                    for handle in opened {
                        let delta = self.resolver.unsubscribe(handle.id());
                        self.consume_resolver_delta(delta);
                    }
                    self.flush_consumed_resolver_closes(&mut effects);
                    let reason = format!("query freshness decision failed: {error}");
                    self.degrade_store(error, &mut effects);
                    return ObservationOpen::Refused { reason, effects };
                }
            }
        }

        let observation = ObservationId(self.next_observation_id);
        self.next_observation_id = self.next_observation_id.wrapping_add(1);
        let branches: Vec<HandleId> = opened.iter().map(|handle| handle.id()).collect();
        for (index, (handle, acquisition)) in opened.into_iter().zip(acquisitions).enumerate() {
            let id = handle.id();
            self.handles.insert(
                id,
                BranchState {
                    _handle: handle,
                    acquisition,
                    observation,
                    index,
                    execution: ObservationExecutionState::default(),
                },
            );
        }
        self.observations.insert(
            observation,
            ObservationState {
                branches: branches.clone(),
                aggregate_result_limit: query.aggregate_result_limit(),
                last_rows: BTreeMap::new(),
                last_evidence: None,
                projection_complete: false,
                next_sequence: 0,
            },
        );

        // Prove the candidate's canonical row union before recompiling the
        // router or refreshing any sibling. If any branch's read fails,
        // removing the just-created owners and draining their resolver drops
        // restores the exact pre-call demand. No wire, relay-admission,
        // attribution, diagnostics, or sibling-frame effect has yet been
        // created.
        let current = match self.observation_rows_for(observation) {
            Ok(current) => current,
            Err(error) => {
                let reason = format!("canonical row projection failed: {error}");
                self.observations.remove(&observation);
                for branch in &branches {
                    self.remove_request_targets_for_handle(*branch);
                    drop(self.handles.remove(branch));
                    let delta = self.resolver.unsubscribe(*branch);
                    self.consume_resolver_delta(delta);
                }
                self.flush_consumed_resolver_closes(&mut effects);
                self.degrade_store(error, &mut effects);
                return ObservationOpen::Refused { reason, effects };
            }
        };

        for branch in &branches {
            self.reconcile_observation_resolution(*branch, ResolutionCause::Initial, &mut effects);
        }
        // The opening evidence frame reads coverage, so it can fail the same
        // way the canonical row projection above can (#763). It is the last
        // fallible step of the open, and it refuses the open rather than
        // delivering a frame whose sources read as "nothing proven" when the
        // store could not be read at all. The withdrawal is `on_unsubscribe`'s
        // unwind minus its `Withdrawn` fact: nothing was ever delivered for
        // this observation, so nothing is owed a terminal fact.
        let evidence = match self.observation_evidence_for(observation) {
            Ok(evidence) => evidence,
            Err(error) => {
                let reason = format!("acquisition evidence projection failed: {error}");
                self.observations.remove(&observation);
                for branch in &branches {
                    let delta = self.resolver.unsubscribe(*branch);
                    self.consume_resolver_delta(delta);
                    self.remove_request_targets_for_handle(*branch);
                    self.handles.remove(branch);
                }
                self.flush_consumed_resolver_closes(&mut effects);
                self.degrade_store(error, &mut effects);
                return ObservationOpen::Refused { reason, effects };
            }
        };
        let mut diagnostics_changed = false;
        for branch in &branches {
            let acquisition = self.handles[branch].acquisition.clone();
            diagnostics_changed |= self.attach_wire_handle(*branch, &acquisition, &mut effects);
        }
        self.flush_consumed_resolver_closes(&mut effects);
        if diagnostics_changed {
            effects.push(Effect::DiagnosticsChanged);
        }
        if self.wire_admission_needed() {
            effects.push(Effect::ArmWireAdmission);
        }
        let seed = self
            .apply_observation_projection(observation, current, evidence)
            .expect("a new observation has no prior projection and always yields one seed");
        ObservationOpen::Opened {
            id: observation,
            seed,
            effects,
        }
    }

    pub(super) fn on_subscribe(&mut self, query: LiveQuery) -> Vec<Effect> {
        let now = self.clock();
        match self.open_observation(query, now) {
            ObservationOpen::Opened {
                id,
                seed,
                mut effects,
            } => {
                effects.push(Effect::EmitRows(id, seed.deltas, seed.evidence));
                effects
            }
            ObservationOpen::Refused { effects, .. } => effects,
        }
    }

    /// Withdraw one observation and every branch it still owns.
    ///
    /// Each branch is released exactly once. Graph nodes and atoms are
    /// refcounted by the resolver, so a branch whose acquisition identity is
    /// still claimed by an unrelated observation stays live: closing this
    /// observation withdraws only what nothing else still owns.
    pub(super) fn on_unsubscribe(&mut self, id: ObservationId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(state) = self.observations.remove(&id) else {
            return effects;
        };
        // The terminal fact is issued from the observation's own sequence
        // before its branches disappear, so a consumer sees an ordered trace
        // that ends in exactly one `Withdrawn`.
        effects.push(Effect::EmitObservationEvidence(
            id,
            vec![ObservationEvidence {
                sequence: state.next_sequence.saturating_add(1),
                branch: None,
                fact: ObservationFact::Withdrawn,
            }],
        ));
        let mut closing = Vec::new();
        for branch in state.branches {
            closing.extend(self.detach_wire_handle(branch));
            let resolver_delta = self.resolver.unsubscribe(branch);
            self.consume_resolver_delta(resolver_delta);
            self.remove_request_targets_for_handle(branch);
            self.handles.remove(&branch);
        }
        self.flush_consumed_resolver_closes(&mut effects);
        self.withdraw_wire_demand(closing, &mut effects);
        effects
    }

    // ---- shared recompile + row-refresh plumbing -------------------------

    /// Recompile the router from the resolver's CURRENT demand, record any
    /// newly-sent REQs' attribution snapshots, and push `Effect::Wire` for
    /// whatever op actually changed on the wire. A broad request for a
    /// behaviorally-proven NIP-77 relay becomes a gap-free handoff: first a
    /// distinct live candidate REQ with `limit:0`, then (only after that
    /// candidate's exact EOSE) Negentropy while the live REQ stays open.
    /// Ledger #8 remains structural: only a `ProbedRelay` token can enter
    /// [`Self::begin_neg_handoff`].
    pub(super) fn recompile(&mut self, effects: &mut Vec<Effect>) {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.router_compiles
            .set(self.router_compiles.get().saturating_add(1));
        self.rebuild_wire_ownership();
        let demand = self.wire_demand();
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.attribution_atoms_rebuilt.set(
            self.attribution_atoms_rebuilt
                .get()
                .saturating_add(demand.len() as u64),
        );
        self.attribution.observe_demand(demand.iter());
        self.flush_author_outbox_route_need_changes(effects);
        // Finding E3 (epic #507): prune `shape_by_key` against the SAME
        // `demand` just observed above, plus every key still `coverage_claims` by
        // an outstanding attribution snapshot (see `prune_shapes`'s own
        // doc for why the latter is required) -- mirrors the
        // `nip11_information.retain(..)` a few lines below, in the same
        // function, against the same kind of "current authoritative set"
        // (`planned`/`demand`) recompile just established.
        self.attribution.prune_shapes(demand.iter());
        let outcome = self
            .router
            .compile(&demand, &self.routing_facts, self.compile_budget());
        let transferred_claims =
            self.apply_request_metadata_updates(&outcome.request_metadata_updates, effects);
        let mut plan_effects = Vec::new();
        self.apply_router_plan_delta(
            &outcome.replacements,
            outcome.wire,
            PlanDeltaMode::Full,
            &mut plan_effects,
        );
        let mut wire_effects = Vec::new();
        for effect in plan_effects {
            if matches!(effect, Effect::Wire(_)) {
                wire_effects.push(effect);
            } else {
                effects.push(effect);
            }
        }
        self.refresh_evidence_for_coverage_keys(&transferred_claims, effects);
        self.refresh_pending_wire_atoms();
        self.refresh_all_observations(effects);
        self.refresh_all_histories(effects);
        // Runtime may feed a local handoff outcome back synchronously while
        // dispatching `Wire`. Publish this full recompile's pre-handoff
        // AwaitingRequest truth first, matching incremental admission,
        // reconnect, and protected-auth replay ordering.
        effects.extend(wire_effects);
    }

    /// Compile exactly the currently-uncovered logical demand as one pending
    /// cohort. Existing plan requests are coverage inputs, never merge or
    /// identity candidates, so this transition cannot rewrite them.
    pub(super) fn flush_wire_admission(&mut self, now: Timestamp) -> Vec<Effect> {
        // Admission is the exact transition that may mint a NIP-77 handoff
        // and its liveness deadline. Carry runtime wall truth into that stamp
        // without turning admission into a maintenance sweep.
        self.advance_clock(now);
        let pending: BTreeSet<_> = self.pending_wire_atoms.values().cloned().collect();
        if pending.is_empty() {
            return Vec::new();
        }

        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.router_compiles
            .set(self.router_compiles.get().saturating_add(1));
        let budget = self.compile_budget();
        let outcome = self.router.admit(&pending, &self.routing_facts, budget);
        let mut effects = Vec::new();
        let transferred_claims =
            self.apply_request_metadata_updates(&outcome.request_metadata_updates, &mut effects);
        let mut plan_effects = Vec::new();
        self.apply_router_plan_delta(
            &BTreeSet::new(),
            outcome.wire,
            PlanDeltaMode::Incremental,
            &mut plan_effects,
        );
        let mut wire_effects = Vec::new();
        for effect in plan_effects {
            if matches!(effect, Effect::Wire(_)) {
                wire_effects.push(effect);
            } else {
                effects.push(effect);
            }
        }
        if outcome.diagnostics_changed {
            effects.push(Effect::DiagnosticsChanged);
        }
        self.refresh_evidence_for_coverage_keys(&outcome.changed_coverage, &mut effects);
        self.refresh_evidence_for_coverage_keys(&transferred_claims, &mut effects);
        self.reconcile_pending_wire_cohort(&pending);
        // Runtime can synchronously feed a local handoff outcome while
        // dispatching `Wire`. Publish the pre-handoff AwaitingRequest snapshot
        // first so that callback's Requesting truth cannot be followed by a
        // stale outer-turn AwaitingRequest regression.
        effects.extend(wire_effects);
        effects
    }

    pub(super) fn apply_request_metadata_updates(
        &mut self,
        updates: &[nmp_router::RequestMetadataUpdate],
        effects: &mut Vec<Effect>,
    ) -> BTreeSet<CoverageKey> {
        let mut transferred_claims = BTreeSet::new();
        for update in updates {
            self.extend_request_attempt_metadata(update);
            self.extend_plan_execution_metadata(update);
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.request_owner_entries_examined.set(
                self.request_owner_entries_examined
                    .get()
                    .saturating_add(update.added_owner_demands.len() as u64),
            );
            self.extend_current_request_owner_demands(
                &update.session,
                &update.sub_id,
                update.filter_hash,
                &update.added_owner_demands,
            );
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.request_claim_entries_examined.set(
                self.request_claim_entries_examined
                    .get()
                    .saturating_add(update.added_coverage_claims.len() as u64),
            );
            let (had_previous_claims, added, extended_current) =
                self.attribution.extend_current_request_claims(
                    &update.sub_id,
                    update.filter_hash,
                    update.added_coverage_claims.clone(),
                );
            if !extended_current {
                transferred_claims.extend(self.transfer_finished_request_claims(
                    &update.session,
                    &update.sub_id,
                    update.filter_hash,
                    had_previous_claims,
                    &added,
                    effects,
                ));
            }
        }
        transferred_claims
    }

    pub(super) fn apply_request_metadata_removals(
        &mut self,
        removals: &[nmp_router::RequestMetadataRemoval],
    ) {
        for removal in removals {
            let detachable_claims: BTreeSet<_> = removal
                .removed_coverage_claims
                .iter()
                .filter(|claim| {
                    !self
                        .router
                        .physical_request_claims(&removal.session, &removal.sub_id)
                        .is_some_and(|physical| physical.contains(claim))
                })
                .copied()
                .collect();
            self.remove_request_attempt_metadata(removal);
            if let Some(metadata) = self.plan_execution_metadata.get_mut(&removal.sub_id) {
                if metadata.filter.hash() == removal.filter_hash {
                    metadata
                        .coverage_claims
                        .retain(|claim| !removal.removed_coverage_claims.contains(claim));
                    metadata
                        .owner_demands
                        .retain(|demand| !removal.removed_owner_demands.contains(demand));
                }
            }
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.request_owner_entries_examined.set(
                self.request_owner_entries_examined
                    .get()
                    .saturating_add(removal.removed_owner_demands.len() as u64),
            );
            self.remove_current_request_owner_demands(
                &removal.session,
                &removal.sub_id,
                removal.filter_hash,
                &removal.removed_owner_demands,
            );
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.request_claim_entries_examined.set(
                self.request_claim_entries_examined
                    .get()
                    .saturating_add(removal.removed_coverage_claims.len() as u64),
            );
            self.attribution
                .release_live_request_claims_delta(&removal.sub_id, &detachable_claims);
            self.attribution.remove_current_send_claims(
                &removal.sub_id,
                removal.filter_hash,
                &detachable_claims,
            );
        }
    }

    pub(super) fn install_plan_execution_metadata(
        &mut self,
        sub_id: SubId,
        filter: ConcreteFilter,
        coverage_claims: BTreeSet<CoverageKey>,
        owner_demands: BTreeSet<nmp_router::DemandKey>,
    ) {
        self.plan_execution_metadata.insert(
            sub_id,
            PlanExecutionMetadata {
                filter,
                coverage_claims,
                owner_demands,
            },
        );
    }

    fn extend_plan_execution_metadata(&mut self, update: &nmp_router::RequestMetadataUpdate) {
        let filter = {
            let Some(metadata) = self.plan_execution_metadata.get_mut(&update.sub_id) else {
                return;
            };
            if metadata.filter.hash() != update.filter_hash {
                return;
            }
            metadata
                .coverage_claims
                .extend(update.added_coverage_claims.iter().copied());
            metadata
                .owner_demands
                .extend(update.added_owner_demands.iter().copied());
            metadata.filter.clone()
        };

        let pending_handoffs = self
            .pending_neg_handoffs_by_plan
            .get(&update.sub_id)
            .cloned()
            .unwrap_or_default();
        let has_pending_handoffs = !pending_handoffs.is_empty();
        for child in pending_handoffs {
            let Some(handoff) = self.pending_neg_handoffs.get(&child) else {
                continue;
            };
            let filter_hash = ConcreteFilter {
                limit: Some(0),
                ..handoff.filter.clone()
            }
            .hash();
            self.extend_nip77_role_generation(&update.session, &child, filter_hash, update);
        }

        let neg_sessions = self
            .neg_sessions_by_plan
            .get(&update.sub_id)
            .cloned()
            .unwrap_or_default();
        let has_neg_sessions = !neg_sessions.is_empty();
        for child in neg_sessions {
            let Some(filter_hash) = self.neg_sessions.get(&child).map(|role| role.filter.hash())
            else {
                continue;
            };
            self.extend_nip77_role_generation(&update.session, &child, filter_hash, update);
        }

        let backfills = self
            .pending_backfills_by_plan
            .get(&update.sub_id)
            .cloned()
            .unwrap_or_default();
        let has_backfills = !backfills.is_empty();
        for child in backfills {
            let Some(role) = self.pending_backfills.get(&child) else {
                continue;
            };
            let erased_hash = ConcreteFilter {
                since: None,
                until: None,
                limit: None,
                ..filter.clone()
            }
            .hash();
            let targets = match role {
                TemporaryReq::MissingIds { neg_sub_id, .. } => {
                    vec![(neg_sub_id.clone(), erased_hash)]
                }
                TemporaryReq::Backlog { .. } => vec![(child.clone(), erased_hash)],
                TemporaryReq::BacklogActivatesLive { live_sub_id, .. } => {
                    let live_filter_hash = ConcreteFilter {
                        limit: Some(0),
                        ..filter.clone()
                    }
                    .hash();
                    vec![
                        (live_sub_id.clone(), live_filter_hash),
                        (child.clone(), erased_hash),
                    ]
                }
            };
            for (target, filter_hash) in targets {
                self.extend_nip77_role_generation(&update.session, &target, filter_hash, update);
            }
        }

        let has_active_live = self.active_nip77_live.contains_key(&update.sub_id);
        if has_pending_handoffs || has_neg_sessions || has_backfills || has_active_live {
            self.attribution
                .retain_added_live_request_claims(&update.sub_id, &update.added_coverage_claims);
        }
    }

    fn extend_nip77_role_generation(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter_hash: DescriptorHash,
        update: &nmp_router::RequestMetadataUpdate,
    ) {
        self.extend_current_request_owner_demands(
            session,
            sub_id,
            filter_hash,
            &update.added_owner_demands,
        );
        self.attribution.extend_current_send_claims(
            sub_id,
            filter_hash,
            &update.added_coverage_claims,
        );
    }

    fn transfer_finished_request_claims(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter_hash: DescriptorHash,
        had_previous_claims: bool,
        added: &BTreeSet<CoverageKey>,
        effects: &mut Vec<Effect>,
    ) -> BTreeSet<CoverageKey> {
        if !had_previous_claims || added.is_empty() {
            return BTreeSet::new();
        }
        let Some(live) = self
            .live_wire_requests
            .get(&(session.clone(), sub_id.clone()))
            .cloned()
        else {
            return BTreeSet::new();
        };
        if live.filter.hash() != filter_hash || live.filter.limit.is_some() {
            return BTreeSet::new();
        }
        let StoredEvents::Finished {
            request_revision,
            committed_interval: Some(interval),
        } = live.stored_events
        else {
            return BTreeSet::new();
        };
        let Some(claims): Option<BTreeMap<_, _>> = added
            .iter()
            .map(|key| self.attribution.claim_shape(*key).map(|atom| (*key, atom)))
            .collect()
        else {
            return BTreeSet::new();
        };
        let key = (session.clone(), sub_id.clone());
        let should_attempt =
            if let Some(pending) = self.pending_request_claim_transfers.get_mut(&key) {
                if pending.filter_hash != filter_hash {
                    return BTreeSet::new();
                }
                pending.request_revision = request_revision;
                pending.interval = interval;
                pending.claims.extend(claims);
                pending.due <= self.clock
            } else {
                self.pending_request_claim_transfers.insert(
                    key.clone(),
                    PendingRequestClaimTransfer {
                        session: session.clone(),
                        sub_id: sub_id.clone(),
                        request_revision,
                        filter_hash,
                        interval,
                        claims,
                        due: self.clock,
                        failures: 0,
                    },
                );
                true
            };
        if should_attempt {
            self.attempt_request_claim_transfer(&key, effects)
        } else {
            BTreeSet::new()
        }
    }

    fn attempt_request_claim_transfer(
        &mut self,
        key: &(RelaySessionKey, SubId),
        effects: &mut Vec<Effect>,
    ) -> BTreeSet<CoverageKey> {
        let Some(mut pending) = self.pending_request_claim_transfers.remove(key) else {
            return BTreeSet::new();
        };
        #[cfg(any(test, feature = "bench-instrumentation"))]
        {
            self.request_claim_transfer_attempts
                .set(self.request_claim_transfer_attempts.get().saturating_add(1));
            self.request_claim_transfer_claims_attempted.set(
                self.request_claim_transfer_claims_attempted
                    .get()
                    .saturating_add(pending.claims.len() as u64),
            );
        }
        let batch: Vec<_> = pending
            .claims
            .values()
            .cloned()
            .map(|atom| (atom, pending.session.relay.clone(), pending.interval))
            .collect();
        if let Err(error) = self.record_request_coverage_batch(&batch) {
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.request_claim_transfer_failures
                .set(self.request_claim_transfer_failures.get().saturating_add(1));
            pending.failures = pending.failures.saturating_add(1);
            pending.due = self.clock + bootstrap_retry_delay_secs(pending.failures);
            self.pending_request_claim_transfers
                .insert(key.clone(), pending);
            self.degrade_store(error, effects);
            return BTreeSet::new();
        }

        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.request_claim_transfer_commits
            .set(self.request_claim_transfer_commits.get().saturating_add(1));

        let committed: BTreeSet<_> = pending.claims.keys().copied().collect();
        let still_current = self
            .live_wire_requests
            .get(&(pending.session.clone(), pending.sub_id.clone()))
            .is_some_and(|live| {
                live.filter.hash() == pending.filter_hash
                    && matches!(
                        live.stored_events,
                        StoredEvents::Finished {
                            request_revision,
                            ..
                        } if request_revision == pending.request_revision
                    )
            });
        if still_current {
            self.attribution
                .retain_added_live_request_claims(&pending.sub_id, &committed);
        }
        committed
    }

    pub(super) fn retry_due_request_claim_transfers(
        &mut self,
        now: Timestamp,
        effects: &mut Vec<Effect>,
    ) {
        let due: Vec<_> = self
            .pending_request_claim_transfers
            .iter()
            .filter_map(|(key, pending)| (pending.due <= now).then_some(key.clone()))
            .collect();
        for key in due {
            let committed = self.attempt_request_claim_transfer(&key, effects);
            self.refresh_evidence_for_coverage_keys(&committed, effects);
        }
    }

    fn cancel_request_claim_transfers(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        replacement_filter: Option<DescriptorHash>,
    ) {
        self.pending_request_claim_transfers.retain(|_, pending| {
            pending.session != *session
                || pending.sub_id != *sub_id
                || replacement_filter == Some(pending.filter_hash)
        });
    }

    #[cfg(test)]
    pub(super) fn reconcile_request_claim_transfers_for_wire_delta(
        &mut self,
        wire_delta: &WireDelta,
    ) {
        self.reconcile_request_claim_transfers_except(wire_delta, &BTreeSet::new());
    }

    fn reconcile_request_claim_transfers_except(
        &mut self,
        wire_delta: &WireDelta,
        deferred_closes: &BTreeSet<(RelaySessionKey, SubId)>,
    ) {
        for (session, ops) in &wire_delta.ops {
            for op in ops {
                match op {
                    WireOp::Req(sub_id, filter) => {
                        self.cancel_request_claim_transfers(session, sub_id, Some(filter.hash()))
                    }
                    WireOp::Close(sub_id)
                        if !deferred_closes.contains(&(session.clone(), sub_id.clone())) =>
                    {
                        self.cancel_request_claim_transfers(session, sub_id, None)
                    }
                    WireOp::Close(_) => {}
                }
            }
        }
    }

    pub(super) fn withdraw_wire_demand(
        &mut self,
        closing_atoms: Vec<ContextualAtom>,
        effects: &mut Vec<Effect>,
    ) {
        if closing_atoms.is_empty() {
            return;
        }
        let budget = self.compile_budget();
        let withdrawal = self.router.withdraw(closing_atoms, budget);
        self.apply_request_metadata_removals(&withdrawal.request_metadata_removals);
        if !withdrawal.wire.ops.is_empty()
            || !withdrawal.changed_coverage.is_empty()
            || withdrawal.diagnostics_changed
        {
            self.apply_router_plan_delta(
                &BTreeSet::new(),
                withdrawal.wire,
                PlanDeltaMode::Incremental,
                effects,
            );
            if withdrawal.diagnostics_changed {
                effects.push(Effect::DiagnosticsChanged);
            }
            self.refresh_evidence_for_coverage_keys(&withdrawal.changed_coverage, effects);
        }
        if self.wire_admission_needed() {
            effects.push(Effect::ArmWireAdmission);
        }
    }

    pub(super) fn apply_router_plan_delta(
        &mut self,
        replacements: &BTreeSet<nmp_router::RequestReplacement>,
        wire_delta: WireDelta,
        mode: PlanDeltaMode,
        effects: &mut Vec<Effect>,
    ) {
        for replacement in replacements {
            let mut replacement = replacement.clone();
            if let Some(inherited) = self.take_request_replacement(&replacement.prior_sub_id) {
                self.cancel_replacement_successor_work(&replacement.prior_sub_id, effects);
                replacement.prior_sub_id = inherited.prior_sub_id;
            }
            self.insert_request_replacement(replacement);
        }
        let transition_priors: BTreeSet<_> = replacements
            .iter()
            .map(|replacement| {
                (
                    replacement.session.clone(),
                    replacement.prior_sub_id.clone(),
                )
            })
            .collect();
        self.reconcile_request_claim_transfers_except(&wire_delta, &transition_priors);
        let planned = &self.router.plan().reqs;
        match mode {
            PlanDeltaMode::Full => {
                self.planned_read_sessions = planned.keys().cloned().collect();
                self.planned_read_session_counts_by_relay.clear();
                for session in planned.keys() {
                    *self
                        .planned_read_session_counts_by_relay
                        .entry(session.relay.clone())
                        .or_insert(0) += 1;
                }
                self.nip11_information.retain(|relay, _| {
                    self.planned_read_session_counts_by_relay
                        .contains_key(relay)
                });
                self.events_by_session_kind
                    .retain(|session, _| planned.contains_key(session));
                effects.extend(
                    planned
                        .keys()
                        .filter(|session| {
                            session.access != AccessContext::Public
                                && !self
                                    .session_registry
                                    .auth_ready_sessions
                                    .contains_key(*session)
                        })
                        .cloned()
                        .map(Effect::EnsureReadRelay),
                );
            }
            PlanDeltaMode::Incremental => {
                let touched: BTreeSet<_> = wire_delta
                    .ops
                    .iter()
                    .map(|(session, _)| session.clone())
                    .collect();
                for session in touched {
                    let planned_now = planned.contains_key(&session);
                    let tracked = self.planned_read_sessions.contains(&session);
                    if planned_now && !tracked {
                        self.planned_read_sessions.insert(session.clone());
                        *self
                            .planned_read_session_counts_by_relay
                            .entry(session.relay.clone())
                            .or_insert(0) += 1;
                        if session.access != AccessContext::Public
                            && !self
                                .session_registry
                                .auth_ready_sessions
                                .contains_key(&session)
                        {
                            effects.push(Effect::EnsureReadRelay(session));
                        }
                    } else if !planned_now && tracked {
                        self.planned_read_sessions.remove(&session);
                        self.events_by_session_kind.remove(&session);
                        if let Some(count) = self
                            .planned_read_session_counts_by_relay
                            .get_mut(&session.relay)
                        {
                            *count = count.saturating_sub(1);
                            if *count == 0 {
                                self.planned_read_session_counts_by_relay
                                    .remove(&session.relay);
                                self.nip11_information.remove(&session.relay);
                            }
                        }
                    }
                }
            }
        }
        // `router.compile()` above ALWAYS finalizes `prev_plan`/`last_diag`
        // for the full current demand, regardless of whether anything
        // actually changed on the wire (see `Router::compile`'s own body) —
        // so diagnostics is pushed unconditionally here (M5 plan §1.2 step
        // 3: "push it at the end of recompile()"), even on the early return
        // below for a no-op wire delta.
        if matches!(mode, PlanDeltaMode::Full) {
            effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        }
        for (session, ops) in &wire_delta.ops {
            for op in ops {
                match op {
                    WireOp::Req(sub_id, filter) => {
                        let claims = self
                            .router
                            .request_claims(session, sub_id)
                            .unwrap_or_default();
                        let owner_demands = self
                            .router
                            .request_demands(session, sub_id)
                            .cloned()
                            .unwrap_or_default();
                        self.install_plan_execution_metadata(
                            sub_id.clone(),
                            filter.clone(),
                            claims.clone(),
                            owner_demands,
                        );
                        self.attribution.retain_live_request_claims(sub_id, claims);
                    }
                    WireOp::Close(sub_id) => {
                        if !transition_priors.contains(&(session.clone(), sub_id.clone())) {
                            self.plan_execution_metadata.remove(sub_id);
                            self.attribution.release_live_request_claims(sub_id);
                        }
                    }
                }
            }
        }
        if wire_delta.ops.is_empty() {
            return;
        }

        let mut kept: Vec<(RelaySessionKey, Vec<WireOp>)> = Vec::new();
        for (session, ops) in &wire_delta.ops {
            // A PROTECTED session's ops are dropped from the wire delta
            // entirely until its exact current generation has completed AUTH
            // (#8): its REQs park (the AUTH reducer's ready transition,
            // `finish_auth_ok`, replays the full planned set on readiness,
            // so nothing is lost), and no CLOSE is needed pre-auth — nothing
            // was ever sent on that socket for this plan to withdraw.
            if session.access != AccessContext::Public
                && !self
                    .session_registry
                    .auth_ready_sessions
                    .contains_key(session)
            {
                continue;
            }
            let mut kept_ops: Vec<WireOp> = Vec::new();
            for op in ops {
                match op {
                    WireOp::Req(sub_id, filter) => {
                        let coverage_claims = self
                            .router
                            .request_claims(session, sub_id)
                            .unwrap_or_default();
                        let owner_demands = self
                            .router
                            .request_demands(session, sub_id)
                            .cloned()
                            .unwrap_or_default();

                        // "Small exact result" (a `limit`) always stays REQ
                        // -- a bounded, terminating fetch is not what
                        // negentropy set-reconciliation is for, and `limit`
                        // poisons coverage attribution regardless (ruling
                        // §3), so there is nothing reconciliation would buy
                        // it. The live-first NIP-77 handoff is additionally PUBLIC-
                        // session-only (#8): the probe verdict was earned on
                        // the unauthenticated socket and proves nothing
                        // about an authenticated session's view.
                        let broad = filter.limit.is_none();
                        match (
                            broad && session.access == AccessContext::Public,
                            self.prober.probed(&session.relay),
                        ) {
                            (true, Some(probed)) => {
                                let prior_live_sub_id = self
                                    .pending_request_replacements
                                    .get(sub_id)
                                    .and_then(|transition| {
                                        self.active_nip77_live
                                            .get(&transition.prior_sub_id)
                                            .cloned()
                                    });
                                self.begin_neg_handoff(
                                    probed,
                                    sub_id.clone(),
                                    prior_live_sub_id,
                                    filter.clone(),
                                    effects,
                                );
                            }
                            _ => {
                                self.record_observed_request_with_purpose(
                                    RequestSend {
                                        session,
                                        sub_id,
                                        filter,
                                        coverage_claims,
                                        owner_demands,
                                        replay: false,
                                        event_failure_target: EventFailureTarget::ThisSend,
                                    },
                                    RequestAttemptPurpose::Ordinary,
                                );
                                kept_ops.push(op.clone());
                            }
                        }
                    }
                    WireOp::Close(sub_id) => {
                        if transition_priors.contains(&(session.clone(), sub_id.clone())) {
                            continue;
                        }
                        if self.pending_request_replacements.contains_key(sub_id) {
                            self.cancel_request_replacement(sub_id, effects);
                        } else {
                            kept_ops.extend(self.close_nip77_plan(sub_id, effects));
                        }
                    }
                }
            }
            if !kept_ops.is_empty() {
                kept.push((session.clone(), kept_ops));
            }
        }

        if !kept.is_empty() {
            effects.push(Effect::Wire(
                self.attempted_wire_delta(WireDelta { ops: kept }),
            ));
        }
    }

    pub(super) fn wire_admission_needed(&self) -> bool {
        !self.pending_wire_atoms.is_empty()
    }

    pub(super) fn refresh_evidence_for_coverage_keys(
        &mut self,
        keys: &BTreeSet<CoverageKey>,
        effects: &mut Vec<Effect>,
    ) {
        self.refresh_evidence_for_coverage_and_demand_keys(keys, &BTreeSet::new(), effects);
    }

    pub(super) fn refresh_evidence_for_coverage_and_demand_keys(
        &mut self,
        coverage_keys: &BTreeSet<CoverageKey>,
        demand_keys: &BTreeSet<nmp_router::DemandKey>,
        effects: &mut Vec<Effect>,
    ) {
        if coverage_keys.is_empty() && demand_keys.is_empty() {
            return;
        }
        let mut candidates: BTreeSet<_> = coverage_keys
            .iter()
            .filter_map(|key| self.wire_handles_by_coverage.get(key))
            .flatten()
            .copied()
            .collect();
        candidates.extend(
            demand_keys
                .iter()
                .filter_map(|key| self.wire_handles_by_demand.get(key))
                .flatten()
                .copied(),
        );
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.evidence_candidates_examined.set(
            self.evidence_candidates_examined
                .get()
                .saturating_add(candidates.len() as u64),
        );
        let mut observations = BTreeSet::new();
        let mut histories = BTreeSet::new();
        for handle in candidates {
            if let Some(state) = self.handles.get(&handle) {
                observations.insert(state.observation);
            } else if let Some(history) = self.history.session_for_handle(handle).as_ref() {
                histories.insert(*history);
            }
        }
        for id in observations {
            self.refresh_observation_evidence(id, effects);
        }
        for id in histories {
            self.refresh_history_evidence(id, effects);
        }
    }

    /// The exact atom union currently owned by handles whose immutable
    /// per-Demand opening-time freshness decision is `Live`. Suppressed
    /// Demand scopes still own their graph and cache projection, but their
    /// atoms are absent from this wire truth.
    pub(super) fn wire_demand(&self) -> BTreeSet<ContextualAtom> {
        self.wire_owner_counts
            .values()
            .map(|(atom, _)| atom.clone())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn retain_wire_atom_owner(&mut self, atom: &ContextualAtom) -> bool {
        self.retain_wire_atom_owner_with_effects(atom, &mut Vec::new())
    }

    fn retain_wire_atom_owner_with_effects(
        &mut self,
        atom: &ContextualAtom,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let key = nmp_router::DemandKey::for_atom(atom);
        let evidence = self
            .wire_routing_evidence_owner_counts
            .entry(key)
            .or_default();
        let mut evidence_grew = false;
        for fact in &atom.routing_evidence {
            let count = evidence.entry(fact.clone()).or_insert(0);
            evidence_grew |= *count == 0;
            *count += 1;
        }
        let effective_evidence = evidence.keys().cloned().collect();
        let (first_owner, effective_atom) = {
            let entry = self
                .wire_owner_counts
                .entry(key)
                .or_insert_with(|| (atom.clone(), 0));
            entry.1 = entry.1.saturating_add(1);
            entry.0.routing_evidence = effective_evidence;
            (entry.1 == 1, entry.0.clone())
        };
        self.retain_author_outbox_wire_owner(atom);

        self.router.activate(effective_atom.clone());
        let mut metadata_diagnostics_changed = false;
        if first_owner {
            self.attribution.observe_atom(&effective_atom);
            self.pending_resolver_wire_closes.remove(&key);
            if let Some(outcome) = self.router.reactivate_covered_atom(&effective_atom) {
                let transferred =
                    self.apply_request_metadata_updates(&outcome.request_metadata_updates, effects);
                self.refresh_evidence_for_coverage_keys(&transferred, effects);
                metadata_diagnostics_changed |= outcome.diagnostics_changed;
                self.pending_wire_atoms.remove(&key);
            } else if self.router.admission_incomplete(key) {
                self.pending_wire_atoms.insert(key, effective_atom.clone());
            }
        } else if (evidence_grew && self.router.admission_incomplete(key))
            || self.pending_wire_atoms.contains_key(&key)
        {
            self.pending_wire_atoms.insert(key, effective_atom.clone());
        }
        metadata_diagnostics_changed
    }

    /// Release one exact owner's contribution. Returns the final effective
    /// atom only when the logical `DemandKey` became ownerless.
    pub(super) fn release_wire_atom_owner(
        &mut self,
        atom: &ContextualAtom,
    ) -> Option<ContextualAtom> {
        let key = nmp_router::DemandKey::for_atom(atom);
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.routing_evidence_owner_keys_touched.set(
            self.routing_evidence_owner_keys_touched
                .get()
                .saturating_add(1),
        );

        let effective_evidence = self
            .wire_routing_evidence_owner_counts
            .get_mut(&key)
            .map(|evidence| {
                for fact in &atom.routing_evidence {
                    if let Some(count) = evidence.get_mut(fact) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            evidence.remove(fact);
                        }
                    }
                }
                evidence.keys().cloned().collect()
            })
            .unwrap_or_default();

        let (final_atom, became_ownerless) = {
            let (effective_atom, count) = self.wire_owner_counts.get_mut(&key)?;
            *count = count.saturating_sub(1);
            (effective_atom.clone(), *count == 0)
        };
        self.release_author_outbox_wire_owner(atom);
        if became_ownerless {
            self.attribution.release_atom(&final_atom);
            self.wire_owner_counts.remove(&key);
            self.wire_routing_evidence_owner_counts.remove(&key);
            self.pending_wire_atoms.remove(&key);
            self.pending_resolver_wire_closes.remove(&key);
            return Some(final_atom);
        }

        let effective_atom = {
            let (effective_atom, _) = self
                .wire_owner_counts
                .get_mut(&key)
                .expect("a demand with surviving owners remains indexed");
            effective_atom.routing_evidence = effective_evidence;
            effective_atom.clone()
        };
        self.router.activate(effective_atom.clone());
        if self.pending_wire_atoms.contains_key(&key) {
            self.pending_wire_atoms.insert(key, effective_atom.clone());
        }
        None
    }

    pub(super) fn attach_wire_handle(
        &mut self,
        id: HandleId,
        acquisition: &HandleAcquisition,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let atoms = self.wire_atoms_for_handle(id, acquisition);
        let mut diagnostics_changed = false;
        let mut demand_refs = BTreeMap::new();
        let mut coverage_refs = BTreeMap::new();
        for atom in &atoms {
            let demand = nmp_router::DemandKey::for_atom(atom);
            *demand_refs.entry(demand).or_insert(0) += 1;
            self.wire_handles_by_demand
                .entry(demand)
                .or_default()
                .insert(id);
            for claim_key in coverage_claim_keys(atom) {
                *coverage_refs.entry(claim_key).or_insert(0) += 1;
                self.wire_handles_by_coverage
                    .entry(claim_key)
                    .or_default()
                    .insert(id);
            }
            self.wire_handles_by_atom
                .entry(atom.clone())
                .or_default()
                .insert(id);
            diagnostics_changed |= self.retain_wire_atom_owner_with_effects(atom, effects);
        }
        self.wire_atoms_by_handle.insert(id, atoms);
        self.wire_demand_refs_by_handle.insert(id, demand_refs);
        self.wire_coverage_refs_by_handle.insert(id, coverage_refs);
        self.activate_request_targets_for_handle(id);
        diagnostics_changed
    }

    pub(super) fn detach_wire_handle(&mut self, id: HandleId) -> Vec<ContextualAtom> {
        let mut closing = Vec::new();
        self.deactivate_request_targets_for_handle(id);
        let atoms = self.wire_atoms_by_handle.remove(&id).unwrap_or_default();
        let demand_refs = self
            .wire_demand_refs_by_handle
            .remove(&id)
            .unwrap_or_default();
        let coverage_refs = self
            .wire_coverage_refs_by_handle
            .remove(&id)
            .unwrap_or_default();
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.withdrawal_handle_detaches
            .set(self.withdrawal_handle_detaches.get().saturating_add(1));
        for demand in demand_refs.keys() {
            if let Some(handles) = self.wire_handles_by_demand.get_mut(demand) {
                handles.remove(&id);
                if handles.is_empty() {
                    self.wire_handles_by_demand.remove(demand);
                }
            }
        }
        for claim_key in coverage_refs.keys() {
            if let Some(handles) = self.wire_handles_by_coverage.get_mut(claim_key) {
                handles.remove(&id);
                if handles.is_empty() {
                    self.wire_handles_by_coverage.remove(claim_key);
                }
            }
        }
        for atom in atoms {
            if let Some(handles) = self.wire_handles_by_atom.get_mut(&atom) {
                handles.remove(&id);
                if handles.is_empty() {
                    self.wire_handles_by_atom.remove(&atom);
                }
            }
            if let Some(final_atom) = self.release_wire_atom_owner(&atom) {
                closing.push(final_atom);
            }
        }
        closing
    }

    /// Consume one resolver outcome exactly once.
    ///
    /// Resolver ownership and live-wire ownership are deliberately different:
    /// cache-only handles own resolver atoms but never own relay work. The
    /// ordinary path therefore detaches by handle above. This recovery path
    /// handles the one fact only a drained pending-drop delta can reveal: a
    /// resolver handle disappeared before core ran that detach. Reverse edges
    /// remove only owners of the reported atom; no sibling census is needed.
    pub(super) fn consume_resolver_delta(&mut self, delta: DemandDelta) {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.resolver_delta_ops_consumed.set(
            self.resolver_delta_ops_consumed
                .get()
                .saturating_add(delta.ops.len() as u64),
        );
        for op in delta.ops {
            let DemandOp::Close(atom) = op else {
                continue;
            };
            let key = nmp_router::DemandKey::for_atom(&atom);
            let Some(stale_handles) = self.wire_handles_by_atom.remove(&atom) else {
                continue;
            };
            let mut released_owners = 0usize;
            for handle in stale_handles {
                let departing_claims = coverage_claim_keys(&atom);
                let (removed, remove_handle) = self
                    .wire_atoms_by_handle
                    .get_mut(&handle)
                    .map(|atoms| {
                        let removed = atoms.remove(&atom);
                        (removed, atoms.is_empty())
                    })
                    .unwrap_or_default();
                if !removed {
                    continue;
                }
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.resolver_owner_keys_touched.set(
                    self.resolver_owner_keys_touched
                        .get()
                        .saturating_add(1 + departing_claims.len() as u64),
                );
                released_owners += usize::from(removed);
                let demand_released = self
                    .wire_demand_refs_by_handle
                    .get_mut(&handle)
                    .is_some_and(|refs| release_ref(refs, key));
                if demand_released {
                    self.deactivate_request_targets_for_handle_demand(handle, key);
                    if let Some(handles) = self.wire_handles_by_demand.get_mut(&key) {
                        handles.remove(&handle);
                        if handles.is_empty() {
                            self.wire_handles_by_demand.remove(&key);
                        }
                    }
                }
                for claim_key in departing_claims {
                    let coverage_released = self
                        .wire_coverage_refs_by_handle
                        .get_mut(&handle)
                        .is_some_and(|refs| release_ref(refs, claim_key));
                    if coverage_released {
                        if let Some(handles) = self.wire_handles_by_coverage.get_mut(&claim_key) {
                            handles.remove(&handle);
                            if handles.is_empty() {
                                self.wire_handles_by_coverage.remove(&claim_key);
                            }
                        }
                    }
                }
                if remove_handle {
                    self.wire_atoms_by_handle.remove(&handle);
                    self.wire_demand_refs_by_handle.remove(&handle);
                    self.wire_coverage_refs_by_handle.remove(&handle);
                }
            }
            for _ in 0..released_owners {
                if let Some(final_atom) = self.release_wire_atom_owner(&atom) {
                    self.pending_resolver_wire_closes.insert(key, final_atom);
                }
            }
        }
    }

    /// Finish one resolver transaction after replacement handles, if any,
    /// have attached. An exact live replacement canceled its pending close in
    /// `attach_wire_handle`; only genuinely ownerless atoms reach the router.
    pub(super) fn flush_consumed_resolver_closes(&mut self, effects: &mut Vec<Effect>) {
        let closing = std::mem::take(&mut self.pending_resolver_wire_closes)
            .into_values()
            .collect();
        self.withdraw_wire_demand(closing, effects);
        self.flush_author_outbox_route_need_changes(effects);
    }

    fn rebuild_wire_ownership(&mut self) {
        let mut contributions = Vec::new();
        for (id, state) in &self.handles {
            contributions.push((*id, self.wire_atoms_for_handle(*id, &state.acquisition)));
        }
        for (handle_id, acquisition) in self.history.wire_attachments() {
            contributions.push((
                handle_id,
                self.wire_atoms_for_handle(handle_id, acquisition),
            ));
        }
        self.wire_atoms_by_handle.clear();
        self.wire_demand_refs_by_handle.clear();
        self.wire_coverage_refs_by_handle.clear();
        self.wire_owner_counts.clear();
        self.author_outbox_wire_owner_counts.clear();
        let previous_author_outbox_route_needs =
            std::mem::take(&mut self.author_outbox_route_needs);
        self.wire_routing_evidence_owner_counts.clear();
        self.wire_handles_by_atom.clear();
        self.wire_handles_by_coverage.clear();
        self.wire_handles_by_demand.clear();
        self.request_targets_by_demand.clear();
        self.active_request_targets_by_handle_demand.clear();
        self.pending_resolver_wire_closes.clear();
        for (id, atoms) in contributions {
            let mut demand_refs = BTreeMap::new();
            let mut coverage_refs = BTreeMap::new();
            for atom in &atoms {
                let key = nmp_router::DemandKey::for_atom(atom);
                *demand_refs.entry(key).or_insert(0) += 1;
                self.wire_handles_by_atom
                    .entry(atom.clone())
                    .or_default()
                    .insert(id);
                self.wire_handles_by_demand
                    .entry(key)
                    .or_default()
                    .insert(id);
                for claim_key in coverage_claim_keys(atom) {
                    *coverage_refs.entry(claim_key).or_insert(0) += 1;
                    self.wire_handles_by_coverage
                        .entry(claim_key)
                        .or_default()
                        .insert(id);
                }
                let entry = self
                    .wire_owner_counts
                    .entry(key)
                    .or_insert_with(|| (atom.clone(), 0));
                entry.1 = entry.1.saturating_add(1);
                let evidence = self
                    .wire_routing_evidence_owner_counts
                    .entry(key)
                    .or_default();
                for fact in &atom.routing_evidence {
                    *evidence.entry(fact.clone()).or_insert(0) += 1;
                }
            }
            self.wire_atoms_by_handle.insert(id, atoms);
            self.wire_demand_refs_by_handle.insert(id, demand_refs);
            self.wire_coverage_refs_by_handle.insert(id, coverage_refs);
        }
        let ordinary_handles: Vec<_> = self.request_targets_by_handle.keys().copied().collect();
        for id in ordinary_handles {
            self.activate_request_targets_for_handle(id);
        }
        for (key, (atom, _)) in &mut self.wire_owner_counts {
            atom.routing_evidence = self
                .wire_routing_evidence_owner_counts
                .get(key)
                .into_iter()
                .flat_map(BTreeMap::keys)
                .cloned()
                .collect();
        }
        self.rebuild_author_outbox_route_needs();
        self.author_outbox_route_needs_changed |=
            previous_author_outbox_route_needs != self.author_outbox_route_needs;
        self.refresh_pending_wire_atoms();
    }

    fn refresh_pending_wire_atoms(&mut self) {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.pending_atoms_rebuilt.set(
            self.pending_atoms_rebuilt
                .get()
                .saturating_add(self.wire_owner_counts.len() as u64),
        );
        self.pending_wire_atoms = self
            .wire_owner_counts
            .iter()
            .filter(|(key, _)| self.router.admission_incomplete(**key))
            .map(|(key, (atom, _))| (*key, atom.clone()))
            .collect();
    }

    fn reconcile_pending_wire_cohort(&mut self, cohort: &BTreeSet<ContextualAtom>) {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.pending_cohort_atoms_reconciled.set(
            self.pending_cohort_atoms_reconciled
                .get()
                .saturating_add(cohort.len() as u64),
        );
        for atom in cohort {
            let key = nmp_router::DemandKey::for_atom(atom);
            if !self.router.plan().limited_demands.contains(&key) {
                self.pending_wire_atoms.remove(&key);
            }
        }
    }

    pub(super) fn wire_atoms_for_handle(
        &self,
        id: HandleId,
        acquisition: &HandleAcquisition,
    ) -> BTreeSet<ContextualAtom> {
        self.resolver
            .demand_scopes(id)
            .into_iter()
            .zip(&acquisition.scopes)
            .filter(|(_, decision)| decision.contributes_wire())
            .flat_map(|((atoms, _), _)| atoms)
            .collect()
    }

    /// Evaluate only one scoped cohort through the same candidate compiler
    /// and residual-capacity reducer as live admission. The preview reads
    /// exact incumbent indexes but never mutates live wire or ownership.
    pub(super) fn shadow_plan_for(&self, demand: BTreeSet<ContextualAtom>) -> RelayPlan {
        let preview =
            self.router
                .preview_admission(&demand, &self.routing_facts, self.compile_budget());
        #[cfg(any(test, feature = "bench-instrumentation"))]
        {
            self.freshness_candidate_atoms.set(
                self.freshness_candidate_atoms
                    .get()
                    .saturating_add(preview.work.candidate_atoms),
            );
            self.freshness_incumbent_demand_edges_visited.set(
                self.freshness_incumbent_demand_edges_visited
                    .get()
                    .saturating_add(preview.work.incumbent_demand_edges_visited),
            );
            self.freshness_plan_request_entries_visited.set(
                self.freshness_plan_request_entries_visited
                    .get()
                    .saturating_add(preview.work.incumbent_request_entries_visited),
            );
            self.freshness_coalesce_pair_attempts.set(
                self.freshness_coalesce_pair_attempts
                    .get()
                    .saturating_add(preview.work.coalesce_pair_attempts),
            );
        }
        preview.plan
    }

    /// Freeze every Demand boundary's opening-time wire participation. Each
    /// scope checks only its own atoms, while the candidate plan includes all
    /// non-CacheOnly scopes that could participate in this handle. An
    /// unsatisfied `MaxAge` becomes `Live` once and stays there; a satisfied
    /// scope retains the exact evaluation plan for evidence and is never
    /// re-evaluated.
    pub(super) fn decide_handle_acquisition(
        &self,
        id: HandleId,
        root_freshness: Freshness,
        now: Timestamp,
    ) -> Result<HandleAcquisition, PersistenceError> {
        let mut scopes = self.resolver.demand_scopes(id);
        if let Some((_, freshness)) = scopes.first_mut() {
            *freshness = root_freshness;
        }
        let candidate_plan = scopes
            .iter()
            .any(|(_, freshness)| matches!(freshness, Freshness::MaxAge { .. }))
            .then(|| {
                let candidate_demand = scopes
                    .iter()
                    .filter(|(_, freshness)| *freshness != Freshness::CacheOnly)
                    .flat_map(|(atoms, _)| atoms.iter().cloned())
                    .collect();
                self.shadow_plan_for(candidate_demand)
            });
        let mut decided = Vec::with_capacity(scopes.len());
        for (atoms, freshness) in scopes {
            decided.push(match freshness {
                Freshness::Live => ScopeAcquisition::Live,
                Freshness::CacheOnly => ScopeAcquisition::CacheOnly,
                Freshness::MaxAge { seconds } => {
                    let plan = candidate_plan
                        .as_ref()
                        .expect("a MaxAge scope built the candidate plan");
                    let evidence = self.opening_coverage_evidence_for(&atoms, plan)?;
                    if self.plan_is_fresh_for(&evidence, seconds, now) {
                        ScopeAcquisition::CoverageSatisfied { evidence }
                    } else {
                        ScopeAcquisition::Live
                    }
                }
            });
        }
        Ok(HandleAcquisition { scopes: decided })
    }

    pub(super) fn acquisition_evidence_for_scopes(
        &self,
        scopes: Vec<(BTreeSet<ContextualAtom>, Freshness)>,
        acquisition: &HandleAcquisition,
    ) -> Result<AcquisitionEvidence, PersistenceError> {
        self.acquisition_evidence_for_scopes_with_plan(scopes, acquisition, self.router.plan())
    }

    pub(super) fn acquisition_evidence_for_scopes_with_plan(
        &self,
        scopes: Vec<(BTreeSet<ContextualAtom>, Freshness)>,
        acquisition: &HandleAcquisition,
        live_plan: &RelayPlan,
    ) -> Result<AcquisitionEvidence, PersistenceError> {
        let auth_status = self.auth_status_map();
        let finished_stored_events = self.finished_stored_events();
        let placed_requests = self.placed_request_keys();
        let awaiting_requests = self.awaiting_request_keys();
        let empty_plan = RelayPlan::default();
        let mut parts = Vec::with_capacity(scopes.len());
        for ((atoms, _), decision) in scopes.into_iter().zip(&acquisition.scopes) {
            if let Some(evidence) = decision.opening_evidence() {
                parts.push(evidence.clone());
                continue;
            }
            let plan = match decision {
                ScopeAcquisition::Live => live_plan,
                ScopeAcquisition::CacheOnly => &empty_plan,
                ScopeAcquisition::CoverageSatisfied { .. } => {
                    unreachable!("opening evidence returned above")
                }
            };
            parts.push(evidence::acquisition_evidence(
                &atoms,
                plan,
                evidence::AcquisitionEvidenceContext {
                    store: &self.store,
                    connected: &self.session_registry.connected_relays,
                    auth_status: &auth_status,
                    ever_connected: &self.session_registry.ever_connected_relays,
                    relay_open_failures: &self.session_registry.relay_open_failures,
                    finished_stored_events: &finished_stored_events,
                    placed_requests: &placed_requests,
                    awaiting_requests: &awaiting_requests,
                    acquisition: evidence::EvidenceAcquisition::Live,
                },
            )?);
        }
        if parts.is_empty() {
            return Ok(AcquisitionEvidence {
                sources: Vec::new(),
                shortfall: vec![ShortfallFact::NoResolvedDemand],
            });
        }
        Ok(evidence::merge_acquisition_evidence(parts))
    }

    /// Read the exact evidence used for the one-time MaxAge decision. Retaining
    /// this snapshot prevents the opening frame from rereading the same rows
    /// and preserves the historical watermark that justified suppression.
    fn opening_coverage_evidence_for(
        &self,
        atoms: &BTreeSet<ContextualAtom>,
        plan: &RelayPlan,
    ) -> Result<AcquisitionEvidence, PersistenceError> {
        let auth_status = self.auth_status_map();
        let finished_stored_events = self.finished_stored_events();
        let placed_requests = self.placed_request_keys();
        let awaiting_requests = self.awaiting_request_keys();
        evidence::acquisition_evidence(
            atoms,
            plan,
            evidence::AcquisitionEvidenceContext {
                store: &self.store,
                connected: &self.session_registry.connected_relays,
                auth_status: &auth_status,
                ever_connected: &self.session_registry.ever_connected_relays,
                relay_open_failures: &self.session_registry.relay_open_failures,
                finished_stored_events: &finished_stored_events,
                placed_requests: &placed_requests,
                awaiting_requests: &awaiting_requests,
                acquisition: evidence::EvidenceAcquisition::CoverageSatisfied,
            },
        )
    }

    /// Unanimous current-assignment freshness over the already-read opening
    /// evidence. Presence of a matching event is deliberately irrelevant: a
    /// coverage row proves the question was checked, so an empty cached result
    /// can satisfy `MaxAge` too.
    pub(super) fn plan_is_fresh_for(
        &self,
        evidence: &AcquisitionEvidence,
        max_age_seconds: u64,
        now: Timestamp,
    ) -> bool {
        let cutoff = Timestamp::from(now.as_secs().saturating_sub(max_age_seconds));
        !evidence.sources.is_empty()
            && evidence.shortfall.is_empty()
            && evidence.sources.iter().all(|source| {
                source
                    .reconciled_through
                    .is_some_and(|through| through >= cutoff)
            })
    }

    /// One exact request is abandoned. It may no longer earn coverage or
    /// produce a settlement fact.
    pub(super) fn abandon_sub(&mut self, sub_id: &SubId) {
        self.attempts.retire_for_sub(sub_id);
        self.attribution.discard_sub(sub_id);
        let session = RelaySessionKey::new(sub_id.0.clone(), sub_id.2);
        self.cancel_request_claim_transfers(&session, sub_id, None);
        let key = (session, sub_id.clone());
        self.pending_request_evidence.remove(&key);
        if let Some(revisions) = self.active_request_revisions_by_sub.remove(&key) {
            for revision in revisions {
                self.active_request_evidence.remove(&revision);
            }
        }
        self.live_wire_requests.remove(&key);
    }

    /// A session dropped. Every attributed request on it is dead; replay
    /// creates fresh request revisions after reconnect.
    pub(super) fn abandon_session_subs(&mut self, session: &RelaySessionKey) {
        self.attempts.retire_for_session(session);
        self.attribution.clear_session(session);
        self.pending_request_evidence
            .retain(|(candidate, _), _| candidate != session);
        let keys: Vec<_> = self
            .active_request_revisions_by_sub
            .keys()
            .filter(|(candidate, _)| candidate == session)
            .cloned()
            .collect();
        for key in keys {
            if let Some(revisions) = self.active_request_revisions_by_sub.remove(&key) {
                for revision in revisions {
                    self.active_request_evidence.remove(&revision);
                }
            }
        }
        self.live_wire_requests
            .retain(|(candidate, _), _| candidate != session);
    }

    /// Whether the exact accepted wire subscription is already live.
    ///
    /// Full filter equality is deliberate: a changed filter on the same
    /// NIP-01 subscription id remains a real replacement. Exact handle
    /// equality prevents an earlier socket from authorizing a fresh one.
    pub(super) fn wire_request_is_live(
        &self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter: &ConcreteFilter,
        handle: TransportRelayHandle,
    ) -> bool {
        self.live_wire_requests
            .get(&(session.clone(), sub_id.clone()))
            .is_some_and(|live| live.filter == *filter && live.handle == handle)
    }

    pub(super) fn session_has_live_generation(
        &self,
        session: &RelaySessionKey,
        handle: TransportRelayHandle,
    ) -> bool {
        self.live_wire_requests
            .iter()
            .any(|((candidate, _), live)| candidate == session && live.handle == handle)
    }

    fn insert_pending_neg_handoff(&mut self, sub_id: SubId, handoff: PendingNegHandoff) {
        self.pending_neg_handoffs_by_plan
            .entry(handoff.plan_sub_id.clone())
            .or_default()
            .insert(sub_id.clone());
        self.pending_neg_handoffs.insert(sub_id, handoff);
    }

    pub(super) fn take_pending_neg_handoff(&mut self, sub_id: &SubId) -> Option<PendingNegHandoff> {
        let handoff = self.pending_neg_handoffs.remove(sub_id)?;
        let plan = handoff.plan_sub_id.clone();
        if let Some(children) = self.pending_neg_handoffs_by_plan.get_mut(&plan) {
            children.remove(sub_id);
            if children.is_empty() {
                self.pending_neg_handoffs_by_plan.remove(&plan);
            }
        }
        Some(handoff)
    }

    fn insert_neg_session(&mut self, sub_id: SubId, session: NegSession) {
        self.neg_sessions_by_plan
            .entry(session.plan_sub_id.clone())
            .or_default()
            .insert(sub_id.clone());
        self.neg_sessions.insert(sub_id, session);
    }

    pub(super) fn take_neg_session(&mut self, sub_id: &SubId) -> Option<NegSession> {
        let session = self.neg_sessions.remove(sub_id)?;
        let plan = session.plan_sub_id.clone();
        if let Some(children) = self.neg_sessions_by_plan.get_mut(&plan) {
            children.remove(sub_id);
            if children.is_empty() {
                self.neg_sessions_by_plan.remove(&plan);
            }
        }
        Some(session)
    }

    fn insert_pending_backfill(&mut self, sub_id: SubId, request: TemporaryReq) {
        self.pending_backfills_by_plan
            .entry(request.plan_sub_id().clone())
            .or_default()
            .insert(sub_id.clone());
        self.pending_backfills.insert(sub_id, request);
    }

    pub(super) fn take_pending_backfill(&mut self, sub_id: &SubId) -> Option<TemporaryReq> {
        let request = self.pending_backfills.remove(sub_id)?;
        let plan = request.plan_sub_id().clone();
        if let Some(children) = self.pending_backfills_by_plan.get_mut(&plan) {
            children.remove(sub_id);
            if children.is_empty() {
                self.pending_backfills_by_plan.remove(&plan);
            }
        }
        Some(request)
    }

    /// Start the gap-free NIP-77 handoff (#563). This function can only be
    /// called with a behaviorally-minted [`ProbedRelay`]. It sends a distinct
    /// candidate live REQ with `limit:0`, keeps the prior live REQ open, and
    /// records a typed pending state. `open_neg_session` is reachable only
    /// when the candidate's exact EOSE arrives.
    pub(super) fn begin_neg_handoff(
        &mut self,
        probed: ProbedRelay,
        plan_sub_id: SubId,
        prior_live_sub_id: Option<SubId>,
        filter: ConcreteFilter,
        effects: &mut Vec<Effect>,
    ) {
        let stale_closes = self.cancel_nip77_repair_for_plan(&plan_sub_id, effects);
        if !stale_closes.is_empty() {
            effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                ops: vec![(RelaySessionKey::public(probed.url().clone()), stale_closes)],
            })));
        }

        let live_filter = ConcreteFilter {
            limit: Some(0),
            ..filter.clone()
        };
        let live_sub_id = self.mint_nip77_role_sub_id(&plan_sub_id, NIP77_LIVE_ROLE, &live_filter);
        let public_session = RelaySessionKey::public(probed.url().clone());
        let metadata = self
            .plan_execution_metadata
            .get(&plan_sub_id)
            .cloned()
            .expect("a NIP-77 role is derived from an installed plan request");
        self.record_observed_request_with_purpose(
            RequestSend {
                session: &public_session,
                sub_id: &live_sub_id,
                filter: &live_filter,
                coverage_claims: metadata.coverage_claims,
                owner_demands: metadata.owner_demands,
                replay: false,
                event_failure_target: EventFailureTarget::ThisSend,
            },
            RequestAttemptPurpose::Nip77LiveCandidate {
                plan_sub_id: plan_sub_id.clone(),
            },
        );
        self.insert_pending_neg_handoff(
            live_sub_id.clone(),
            PendingNegHandoff {
                probed,
                plan_sub_id,
                live_sub_id: live_sub_id.clone(),
                prior_live_sub_id,
                filter,
                started_at: self.clock,
            },
        );
        effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
            ops: vec![(public_session, vec![WireOp::Req(live_sub_id, live_filter)])],
        })));
        effects.push(Effect::DiagnosticsChanged);
    }

    /// Withdraw every pending/repair phase belonging to one semantic router
    /// subscription while deliberately leaving its currently-active live REQ
    /// alone. Used before a replacement handoff; [`Self::close_nip77_plan`]
    /// additionally withdraws the active live owner on demand removal.
    pub(super) fn cancel_nip77_repair_for_plan(
        &mut self,
        plan_sub_id: &SubId,
        effects: &mut Vec<Effect>,
    ) -> Vec<WireOp> {
        let mut closes = BTreeSet::new();

        let pending = self
            .pending_neg_handoffs_by_plan
            .remove(plan_sub_id)
            .unwrap_or_default();
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.nip77_plan_children_touched.set(
            self.nip77_plan_children_touched
                .get()
                .saturating_add(pending.len() as u64),
        );
        for live_id in pending {
            self.pending_neg_handoffs.remove(&live_id);
            self.abandon_sub(&live_id);
            closes.insert(live_id);
        }

        let neg_ids = self
            .neg_sessions_by_plan
            .remove(plan_sub_id)
            .unwrap_or_default();
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.nip77_plan_children_touched.set(
            self.nip77_plan_children_touched
                .get()
                .saturating_add(neg_ids.len() as u64),
        );
        for neg_id in neg_ids {
            if let Some(session) = self.neg_sessions.remove(&neg_id) {
                self.abandon_sub(&neg_id);
                effects.push(Effect::NegClose(session.relay, neg_id));
            }
        }

        self.retire_temporary_reqs_for_plan(plan_sub_id, &mut closes);

        closes.into_iter().map(WireOp::Close).collect()
    }

    /// Withdraw every temporary repair REQ owned by `plan_sub_id`, adding
    /// each subscription that must leave the wire to `closes`.
    ///
    /// Shared by [`Self::cancel_nip77_repair_for_plan`] and
    /// [`Self::start_backlog_req`] rather than written twice, because a
    /// `BacklogActivatesLive` entry owns a NESTED live candidate (and
    /// sometimes its predecessor) that lives in no other map at all -- a
    /// second, hand-rolled teardown that forgot that nesting would leak the
    /// candidate on the wire forever and leave a wire id a late EOSE could
    /// still resolve through.
    fn retire_temporary_reqs_for_plan(
        &mut self,
        plan_sub_id: &SubId,
        closes: &mut BTreeSet<SubId>,
    ) {
        let temporary = self
            .pending_backfills_by_plan
            .remove(plan_sub_id)
            .unwrap_or_default();
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.nip77_plan_children_touched.set(
            self.nip77_plan_children_touched
                .get()
                .saturating_add(temporary.len() as u64),
        );
        for sub_id in temporary {
            match self.pending_backfills.remove(&sub_id) {
                Some(TemporaryReq::MissingIds { neg_sub_id, .. }) => {
                    // NEG has already closed on the wire, but its coverage
                    // snapshot intentionally remained alive while the missing
                    // ids were in flight. Withdrawing/superseding that fetch
                    // must release the deferred snapshot too.
                    self.abandon_sub(&neg_sub_id);
                }
                Some(TemporaryReq::BacklogActivatesLive {
                    live_sub_id,
                    prior_live_sub_id,
                    ..
                }) => {
                    // The live candidate REQ is tracked ONLY inside this
                    // fallback entry while its own EOSE is still
                    // outstanding -- it lives in neither
                    // `pending_neg_handoffs` nor `active_nip77_live`.
                    // Withdrawing/superseding demand mid-fallback must
                    // close and discard it here, or it leaks forever: a
                    // late EOSE on its orphaned wire id would otherwise
                    // still resolve through `attribution` and mint
                    // phantom coverage for demand that no longer exists.
                    self.abandon_sub(&live_sub_id);
                    closes.insert(live_sub_id);
                    // `prior_live_sub_id` is ordinarily still the entry
                    // tracked in `active_nip77_live[plan_sub_id]`, closed
                    // either by `close_nip77_plan` (full withdrawal) or
                    // carried forward into the next handoff's own
                    // `prior_live_sub_id` (supersession, see
                    // `begin_neg_handoff`). Only close it here if it has
                    // already drifted away from that slot, so this never
                    // double-closes a subscription another path owns.
                    if let Some(prior) = prior_live_sub_id {
                        if self.active_nip77_live.get(plan_sub_id) != Some(&prior) {
                            self.abandon_sub(&prior);
                            closes.insert(prior);
                        }
                    }
                }
                Some(TemporaryReq::Backlog { .. }) | None => {}
            }
            self.abandon_sub(&sub_id);
            closes.insert(sub_id);
        }
    }

    pub(super) fn close_nip77_plan(
        &mut self,
        plan_sub_id: &SubId,
        effects: &mut Vec<Effect>,
    ) -> Vec<WireOp> {
        let mut closes: BTreeSet<SubId> = self
            .cancel_nip77_repair_for_plan(plan_sub_id, effects)
            .into_iter()
            .filter_map(|op| match op {
                WireOp::Close(sub_id) => Some(sub_id),
                WireOp::Req(..) => None,
            })
            .collect();
        let active = self
            .active_nip77_live
            .remove(plan_sub_id)
            .unwrap_or_else(|| plan_sub_id.clone());
        self.abandon_sub(&active);
        closes.insert(active);
        closes.into_iter().map(WireOp::Close).collect()
    }

    fn retire_nip77_roles_for_replacement(
        &mut self,
        plan_sub_id: &SubId,
        effects: &mut Vec<Effect>,
    ) -> Vec<WireOp> {
        let mut closes: BTreeSet<_> = self
            .cancel_nip77_repair_for_plan(plan_sub_id, effects)
            .into_iter()
            .filter_map(|op| match op {
                WireOp::Close(sub_id) => Some(sub_id),
                WireOp::Req(..) => None,
            })
            .collect();
        if let Some(active) = self.active_nip77_live.remove(plan_sub_id) {
            self.abandon_sub(&active);
            closes.insert(active);
        }
        closes.into_iter().map(WireOp::Close).collect()
    }

    fn insert_request_replacement(&mut self, replacement: nmp_router::RequestReplacement) {
        self.request_replacements_by_session
            .entry(replacement.session.clone())
            .or_default()
            .insert(replacement.next_sub_id.clone());
        let prior = self
            .pending_request_replacements
            .insert(replacement.next_sub_id.clone(), replacement);
        assert!(
            prior.is_none(),
            "one successor owns one replacement transition"
        );
    }

    fn take_request_replacement(
        &mut self,
        successor: &SubId,
    ) -> Option<nmp_router::RequestReplacement> {
        let replacement = self.pending_request_replacements.remove(successor)?;
        if let Some(successors) = self
            .request_replacements_by_session
            .get_mut(&replacement.session)
        {
            successors.remove(successor);
            if successors.is_empty() {
                self.request_replacements_by_session
                    .remove(&replacement.session);
            }
        }
        Some(replacement)
    }

    fn cancel_replacement_successor_work(&mut self, successor: &SubId, effects: &mut Vec<Effect>) {
        let session = RelaySessionKey::new(successor.0.clone(), successor.2);
        let closes = self.cancel_nip77_repair_for_plan(successor, effects);
        self.abandon_sub(successor);
        self.attribution.release_live_request_claims(successor);
        self.plan_execution_metadata.remove(successor);
        self.cancel_request_claim_transfers(&session, successor, None);
        if !closes.is_empty() {
            effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                ops: vec![(session, closes)],
            })));
        }
    }

    fn retire_replacement_predecessor(
        &mut self,
        replacement: nmp_router::RequestReplacement,
        effects: &mut Vec<Effect>,
    ) {
        let had_nip77_roles = self
            .pending_neg_handoffs_by_plan
            .contains_key(&replacement.prior_sub_id)
            || self
                .neg_sessions_by_plan
                .contains_key(&replacement.prior_sub_id)
            || self
                .pending_backfills_by_plan
                .contains_key(&replacement.prior_sub_id)
            || self
                .active_nip77_live
                .contains_key(&replacement.prior_sub_id);
        let mut closes: BTreeSet<_> = self
            .retire_nip77_roles_for_replacement(&replacement.prior_sub_id, effects)
            .into_iter()
            .filter_map(|op| match op {
                WireOp::Close(sub_id) => Some(sub_id),
                WireOp::Req(..) => None,
            })
            .collect();
        if !had_nip77_roles {
            closes.insert(replacement.prior_sub_id.clone());
        }
        self.abandon_sub(&replacement.prior_sub_id);
        self.attribution
            .release_live_request_claims(&replacement.prior_sub_id);
        self.plan_execution_metadata
            .remove(&replacement.prior_sub_id);
        self.cancel_request_claim_transfers(&replacement.session, &replacement.prior_sub_id, None);
        if !closes.is_empty() {
            effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                ops: vec![(
                    replacement.session,
                    closes.into_iter().map(WireOp::Close).collect(),
                )],
            })));
        }
    }

    pub(super) fn complete_request_replacement(
        &mut self,
        successor: &SubId,
        effects: &mut Vec<Effect>,
    ) {
        let Some(replacement) = self.take_request_replacement(successor) else {
            return;
        };
        self.retire_replacement_predecessor(replacement, effects);
    }

    fn cancel_request_replacement(&mut self, successor: &SubId, effects: &mut Vec<Effect>) {
        let Some(replacement) = self.take_request_replacement(successor) else {
            return;
        };
        self.cancel_replacement_successor_work(successor, effects);
        self.retire_replacement_predecessor(replacement, effects);
    }

    pub(super) fn abandon_request_replacements_for_session(&mut self, session: &RelaySessionKey) {
        let successors = self
            .request_replacements_by_session
            .remove(session)
            .unwrap_or_default();
        for successor in successors {
            let Some(replacement) = self.pending_request_replacements.remove(&successor) else {
                continue;
            };
            self.attribution
                .release_live_request_claims(&replacement.prior_sub_id);
            self.plan_execution_metadata
                .remove(&replacement.prior_sub_id);
            self.cancel_request_claim_transfers(session, &replacement.prior_sub_id, None);
        }
    }

    /// The candidate live REQ's EOSE is the handoff barrier. Promote it to
    /// the only active live owner, retire the overlapped predecessor, then
    /// and only then snapshot local holdings and open Negentropy.
    pub(super) fn activate_live_and_open_neg(
        &mut self,
        handoff: PendingNegHandoff,
        effects: &mut Vec<Effect>,
    ) {
        self.active_nip77_live
            .insert(handoff.plan_sub_id.clone(), handoff.live_sub_id.clone());
        if self
            .pending_request_replacements
            .contains_key(&handoff.plan_sub_id)
        {
            self.complete_request_replacement(&handoff.plan_sub_id, effects);
        } else if let Some(prior) = handoff.prior_live_sub_id.as_ref() {
            if prior != &handoff.live_sub_id {
                self.abandon_sub(prior);
                effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                    ops: vec![(
                        RelaySessionKey::public(handoff.probed.url().clone()),
                        vec![WireOp::Close(prior.clone())],
                    )],
                })));
            }
        }
        self.open_neg_session(handoff, effects);
    }

    /// Open a real reconciliation only after the candidate live REQ is
    /// active. NIP-01 and NIP-77 use separate subscription namespaces; the
    /// role-derived `neg_sub_id` makes that separation explicit in reducer
    /// state and permits both protocols to remain open concurrently.
    pub(super) fn open_neg_session(
        &mut self,
        handoff: PendingNegHandoff,
        effects: &mut Vec<Effect>,
    ) {
        let PendingNegHandoff {
            probed,
            plan_sub_id,
            filter,
            ..
        } = handoff;

        let neg_filter = ConcreteFilter {
            since: None,
            until: None,
            limit: None,
            ..filter
        };
        // Seeding the reconciler reads only holdings already observed from
        // THIS relay. A row learned from relay A is locally available, but
        // advertising it to relay B would make a shared id compare equal and
        // suppress B's backfill before NMP has ever verified B's copy. That
        // permanently loses B provenance. On an I/O failure (issue #122)
        // degrade to read-only and do not open the session rather than panic
        // — the `Close` pushed above still stands, so the sub-id is simply
        // released.
        let local_rows = match self.store.query(&neg_filter.to_nostr()) {
            Ok(rows) => rows,
            Err(e) => {
                self.degrade_store(e, effects);
                let owner = plan_sub_id.clone();
                self.start_backlog_req(
                    plan_sub_id,
                    neg_filter,
                    TemporaryReq::Backlog { plan_sub_id: owner },
                    effects,
                );
                return;
            }
        };
        let local_ids: Vec<(u64, EventId)> = local_rows
            .into_iter()
            .filter(|stored| stored.provenance.seen.contains_key(probed.url()))
            .map(|se| (se.event.created_at.as_secs(), se.event.id))
            .collect();
        let (reconciler, initial_hex) = Reconciler::open(&local_ids);

        let neg_sub_id = self.mint_nip77_role_sub_id(&plan_sub_id, NIP77_NEG_ROLE, &neg_filter);

        let public_session = RelaySessionKey::public(probed.url().clone());
        let metadata = self
            .plan_execution_metadata
            .get(&plan_sub_id)
            .cloned()
            .expect("a NEG role is derived from an installed plan request");
        let (attribution_send, attempt_id) = self.record_observed_request_with_purpose(
            RequestSend {
                session: &public_session,
                sub_id: &neg_sub_id,
                filter: &neg_filter,
                coverage_claims: metadata.coverage_claims,
                owner_demands: metadata.owner_demands,
                replay: false,
                event_failure_target: EventFailureTarget::ThisSend,
            },
            RequestAttemptPurpose::Nip77Open {
                plan_sub_id: plan_sub_id.clone(),
            },
        );
        // The request-evidence record stays PENDING here (issue #775). This
        // door proves the exact current connected generation -- it is opened
        // synchronously from that generation's live-candidate EOSE -- but a
        // connected generation is not an accepted frame: the worker's finite
        // outbound envelope (#506/#1331) refuses at admission, and that
        // refusal is materially reachable. Activating the evidence here would
        // claim NMP had placed a question it may never place. The runtime
        // reports the real outcome through `on_nip77_handoff`, which either
        // activates this same record or consumes it as refused.
        self.insert_neg_session(
            neg_sub_id.clone(),
            NegSession {
                plan_sub_id,
                relay: probed.url().clone(),
                filter: neg_filter.clone(),
                attribution_send,
                started_at: self.clock,
                reconciler,
            },
        );
        effects.push(Effect::NegOpen(
            attempt_id,
            probed,
            neg_sub_id,
            neg_filter,
            initial_hex,
        ));
    }

    /// The one door every NIP-77 outbound frame's transport outcome returns
    /// through (issue #775).
    ///
    /// `Pool::send`'s `false` is local backpressure and nothing else: the
    /// worker's finite outbound envelope refused the frame at admission, the
    /// handle is stale, or no session could be opened at all. It is never a
    /// statement about the relay. Before this door the runtime discarded it
    /// (`let _ = pool.send(..)`) at all three NIP-77 effects, so reducer state
    /// that had already advanced past the send stayed advanced: a probe sat in
    /// `Probing` for the engine's lifetime, and a reconciliation waited out the
    /// 30-second silent-relay deadline for a frame that never left the process.
    ///
    /// `handle` is the exact Public generation the frame was attempted on, or
    /// `None` when no session could be opened. It is read only on acceptance.
    ///
    /// Public only through the doc-hidden mechanism surface, exactly like
    /// [`Self::on_wire_request_handoff`], so headless reducer falsifiers drive
    /// the same edge the runtime does.
    #[doc(hidden)]
    pub fn on_nip77_handoff(
        &mut self,
        frame: Nip77Frame,
        outcome: RequestHandoffOutcome,
    ) -> Vec<Effect> {
        let Some(attempt) = self.attempts.get(outcome.attempt_id()).cloned() else {
            return Vec::new();
        };
        let relay = attempt.session.relay.clone();
        let sub_id = attempt.sub_id.clone();
        let accepted = matches!(outcome, RequestHandoffOutcome::Accepted { .. });
        let mut effects = Vec::new();
        match (frame, accepted) {
            // Acceptance means the worker owns the bytes. A probe and a
            // continuing round have no further reducer state to advance --
            // they already advanced, and now honestly.
            (Nip77Frame::Probe, true) | (Nip77Frame::Continue, true) => {
                self.attempts.take(&outcome);
            }
            (Nip77Frame::Open, true) => {
                let Some(_session) = self.neg_sessions.get(&sub_id) else {
                    return effects;
                };
                effects.extend(self.on_wire_request_handoff(outcome));
            }
            (Nip77Frame::Probe, false) => {
                self.attempts.take(&outcome);
                if self
                    .prober
                    .refuse_probe(&relay, &crate::core::wire_sub_id_string(&sub_id))
                {
                    // The projected capability verdict just moved back to
                    // `unknown`; it is observable state, so publish it rather
                    // than waiting for an unrelated recompile.
                    effects.push(Effect::DiagnosticsChanged);
                }
            }
            (Nip77Frame::Open, false) => {
                let Some(session) = self.take_neg_session(&sub_id) else {
                    return effects;
                };
                // Consume the still-pending request evidence as refused. This
                // is the one place an app can learn that a NIP-77 question was
                // never placed, and it is emitted instead of -- never beside --
                // the `RelayRequest` an accepted handoff would have produced.
                let (handoff_effects, _) = self.consume_wire_request_handoff(outcome);
                effects.extend(handoff_effects);
                // No `NEG-CLOSE`: the relay never saw a `NEG-OPEN`, so closing
                // would be a frame about a session that does not exist -- and
                // would compete for the very envelope room that was just
                // refused.
                let mut fallback_effects = Vec::new();
                self.neg_session_backlog_fallback(&sub_id, session, &mut fallback_effects);
                effects.extend(fallback_effects);
            }
            (Nip77Frame::Continue, false) => {
                self.attempts.take(&outcome);
                let Some(session) = self.take_neg_session(&sub_id) else {
                    return effects;
                };
                // The open WAS accepted, so this reconciliation exists on the
                // relay and its best-effort `NEG-CLOSE` is warranted -- the
                // same terminal the `NEG-ERR`, malformed-payload and
                // liveness-deadline paths take, reached immediately rather
                // than 30 seconds after a frame that never left the process.
                //
                // `reason` has no consumer here on purpose: this request's
                // evidence is already ACTIVE (its open was accepted), so it is
                // retired by `abandon_sub` rather than refused, exactly as the
                // other three abandonment paths retire it. The app-visible
                // consequence is the fallback REQ's own fresh evidence.
                self.neg_session_fallback_to_req(sub_id, session, &mut effects);
            }
        }
        effects
    }

    /// Drive one inbound `NEG-MSG` round for `sub_id`'s live session, if any
    /// (a frame for a sub this reducer isn't tracking is an untrusted-
    /// network fact, silently ignored -- same discipline as
    /// `handle_write_ack`'s unknown-`OK` case).
    pub(super) fn step_neg_session(
        &mut self,
        sub_id: SubId,
        relay: RelayUrl,
        message_hex: &str,
        effects: &mut Vec<Effect>,
    ) {
        let Some(session) = self.neg_sessions.get_mut(&sub_id) else {
            return;
        };
        let filter = session.filter.clone();
        let filter_hash = filter.hash();
        let step = session.reconciler.step(message_hex);
        match step {
            Ok(NegStep::Continue(next_hex)) => {
                let attempt_id = self.attempts.mint(RequestAttemptState {
                    session: RelaySessionKey::public(relay.clone()),
                    sub_id: sub_id.clone(),
                    filter_hash,
                    filter,
                    coverage_claims: BTreeSet::new(),
                    owner_demands: BTreeSet::new(),
                    replay: false,
                    event_failure_target: EventFailureTarget::ThisSend,
                    request_revision: None,
                    retry_failures: 0,
                    purpose: RequestAttemptPurpose::Nip77Continue,
                });
                effects.push(Effect::NegMsg(attempt_id, relay, sub_id, next_hex));
            }
            Ok(NegStep::Done(need_ids)) => {
                let session = self
                    .take_neg_session(&sub_id)
                    .expect("just matched via get_mut above -- still present");
                self.finish_neg_session(sub_id, relay, session, need_ids, effects);
            }
            Err(_) => {
                // A malformed/unexpected reconcile payload from an
                // untrusted relay: abandon this reconciliation and fall
                // back to a plain REQ for the same filter -- the same
                // recovery path as the liveness-deadline/NEG-ERR cases,
                // never a silent read-gap.
                if let Some(session) = self.take_neg_session(&sub_id) {
                    self.neg_session_fallback_to_req(sub_id, session, effects);
                }
            }
        }
    }

    /// Reconciliation completed. Close only the NIP-77 namespace and
    /// backfill whatever ids Negentropy proved we are missing through the
    /// ordinary REQ/EOSE/ingest pipeline. The live NIP-01 subscription was
    /// opened before reconciliation and deliberately remains untouched.
    ///
    /// Evidence crediting (ledger #7) is NOT immediate when a backfill is
    /// needed: recording a reconciled watermark before the backfilled events
    /// are actually ingested would attach evidence to a store
    /// that is still, transiently, missing precisely the events negentropy
    /// just proved are missing.
    /// `TemporaryReq::MissingIds` defers credit to the backfill sub's OWN
    /// EOSE, by which point the events are already ingested (EVENT precedes
    /// EOSE, NIP-01). An empty `need_ids` credits immediately.
    pub(super) fn finish_neg_session(
        &mut self,
        sub_id: SubId,
        relay: RelayUrl,
        session: NegSession,
        need_ids: BTreeSet<EventId>,
        effects: &mut Vec<Effect>,
    ) {
        let NegSession {
            plan_sub_id,
            attribution_send,
            ..
        } = session;
        let completed_at = self.clock;
        effects.push(Effect::NegClose(relay.clone(), sub_id.clone()));

        if need_ids.is_empty() {
            let committed_coverage =
                self.credit_neg_coverage(&sub_id, attribution_send, completed_at, &relay, effects);
            let terminal_demands = if committed_coverage.is_some() {
                self.emit_request_settled(
                    attribution_send,
                    completed_at,
                    RequestTerminal::Nip77,
                    effects,
                )
            } else {
                self.retire_request_evidence(attribution_send)
            };
            let committed_coverage = committed_coverage.unwrap_or_default();
            self.refresh_evidence_for_coverage_and_demand_keys(
                &committed_coverage,
                &terminal_demands,
                effects,
            );
            self.abandon_sub(&sub_id);
        } else {
            let backfill = ConcreteFilter {
                ids: Some(need_ids.iter().map(|id| id.to_hex()).collect()),
                ..ConcreteFilter::default()
            };
            // An id-targeted one-shot backfill fetch, not itself tied to
            // any live Demand (#106): no `authors` binding at all, so
            // `Public`/`Public` is the exact context `Demand::from_filter`'s
            // static default would assign an authorless filter -- and this
            // sub carries no coverage credit of its own anyway (`coverage_claims`
            // is empty below; its typed `TemporaryReq::MissingIds` owner
            // unlocks `sub_id`'s credit at EOSE).
            let backfill_sub =
                self.mint_nip77_role_sub_id(&plan_sub_id, NIP77_MISSING_ROLE, &backfill);
            let missing_plan = plan_sub_id.clone();
            let evidence_demands = self
                .plan_execution_metadata
                .get(&missing_plan)
                .map(|metadata| metadata.owner_demands.clone())
                .expect("a missing-id role is derived from an installed plan request");
            self.insert_pending_backfill(
                backfill_sub.clone(),
                TemporaryReq::MissingIds {
                    plan_sub_id,
                    neg_sub_id: sub_id.clone(),
                    attribution_send,
                    completed_at,
                },
            );
            // No coverage credit of its OWN for this one-shot id-set fetch
            // -- `coverage_claims` is deliberately empty; it targets exactly the
            // ids negentropy already proved, it is not itself a proof over
            // any atom's shape (the credit it unlocks is `sub_id`'s).
            let public_session = RelaySessionKey::public(relay.clone());
            self.record_observed_request_with_purpose(
                RequestSend {
                    session: &public_session,
                    sub_id: &backfill_sub,
                    filter: &backfill,
                    coverage_claims: BTreeSet::new(),
                    owner_demands: BTreeSet::new(),
                    replay: false,
                    event_failure_target: EventFailureTarget::Correlated(attribution_send),
                },
                RequestAttemptPurpose::Nip77MissingIds {
                    plan_sub_id: missing_plan,
                },
            );
            self.refresh_evidence_for_coverage_and_demand_keys(
                &BTreeSet::new(),
                &evidence_demands,
                effects,
            );
            effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                ops: vec![(
                    RelaySessionKey::public(relay.clone()),
                    vec![WireOp::Req(backfill_sub, backfill)],
                )],
            })));
        }
        effects.push(Effect::DiagnosticsChanged);
    }

    /// Attribute the exact NEG send-time snapshot that completed. Unlike an
    /// ordinary REQ's ambiguous EOSE, NEG-DONE is structurally correlated to
    /// its live `NegSession`. Credit may wait for a backfill EOSE, but
    /// `completed_at` remains the NEG completion time.
    pub(super) fn credit_neg_coverage(
        &mut self,
        sub_id: &SubId,
        attribution_send: AttributionSendId,
        completed_at: Timestamp,
        relay: &RelayUrl,
        effects: &mut Vec<Effect>,
    ) -> Option<BTreeSet<CoverageKey>> {
        // Negentropy sessions are opened exclusively on the Public session
        // (#8), so their credit resolves through the same Public-session
        // attribution key `open_neg_session` recorded under.
        let attributed = self.attribution.attribute_correlated_completion(
            &RelaySessionKey::public(relay.clone()),
            &wire_sub_id_string(sub_id),
            attribution_send,
            completed_at,
        );
        attributed
            .and_then(|completed| self.persist_attributed_completion(completed, relay, effects))
    }

    /// The one facts-before-claims persistence door shared by ordinary EOSE
    /// and NEG completion. A poisoned completion performs no store I/O.
    /// Every retained shape is resolved before one atomic request-level
    /// coverage transaction starts. Only after that whole transaction commits
    /// does this return the committed keys for evidence refresh.
    pub(super) fn persist_attributed_completion(
        &mut self,
        completed: CompletedAttribution,
        relay: &RelayUrl,
        effects: &mut Vec<Effect>,
    ) -> Option<BTreeSet<CoverageKey>> {
        let completed_sub_id = completed.sub_id().clone();
        let completed_send = completed.send_id();
        let completed_filter_hash = completed.filter_hash();
        let committed_interval = completed.eligible_generation_interval();
        let claims = completed.into_eligible_claims()?;
        if claims.is_empty() {
            return Some(BTreeSet::new());
        }

        let mut batch = Vec::with_capacity(claims.len());
        for claim in &claims {
            batch.push((claim.atom.clone(), relay.clone(), claim.interval));
        }

        if let Err(error) = self.record_request_coverage_batch(&batch) {
            self.degrade_store(error, effects);
            return None;
        }

        let session = RelaySessionKey::new(completed_sub_id.0.clone(), completed_sub_id.2);
        self.retire_request_claim_transfer_covered_by_completion(
            &session,
            &completed_sub_id,
            completed_send.revision(),
            completed_filter_hash,
            &claims,
        );
        if let Some(live) = self
            .live_wire_requests
            .get_mut(&(session, completed_sub_id))
        {
            if let StoredEvents::Streaming {
                request_revision,
                committed_interval: live_interval,
            } = &mut live.stored_events
            {
                if *request_revision == completed_send.revision() {
                    *live_interval = committed_interval;
                }
            }
        }

        let committed = claims.into_iter().map(|claim| claim.key).collect();
        Some(committed)
    }

    fn retire_request_claim_transfer_covered_by_completion(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        request_revision: u64,
        filter_hash: DescriptorHash,
        completed_claims: &[CompletedCoverageClaim],
    ) {
        let key = (session.clone(), sub_id.clone());
        let Some(pending) = self.pending_request_claim_transfers.get(&key) else {
            return;
        };
        let superseded = pending.filter_hash == filter_hash
            && request_revision > pending.request_revision
            && pending.claims.keys().all(|key| {
                completed_claims.iter().any(|claim| {
                    claim.key == *key
                        && claim.interval.from <= pending.interval.from
                        && claim.interval.through >= pending.interval.through
                })
            });
        if !superseded {
            return;
        }
        let pending = self
            .pending_request_claim_transfers
            .remove(&key)
            .expect("the checked transfer remains pending");
        let still_current = self.live_wire_requests.get(&key).is_some_and(|live| {
            live.filter.hash() == filter_hash
                && matches!(
                    live.stored_events,
                    StoredEvents::Streaming {
                        request_revision: live_revision,
                        ..
                    } if live_revision == request_revision
                )
        });
        if still_current {
            self.attribution.retain_added_live_request_claims(
                sub_id,
                &pending.claims.keys().copied().collect(),
            );
        }
    }

    /// Start one unlimited one-shot backlog REQ under a role-separated id.
    /// It never aliases the live NIP-01 id or the NIP-77 session id.
    pub(super) fn start_backlog_req(
        &mut self,
        plan_sub_id: SubId,
        filter: ConcreteFilter,
        request: TemporaryReq,
        effects: &mut Vec<Effect>,
    ) {
        let filter = ConcreteFilter {
            since: None,
            until: None,
            limit: None,
            ..filter
        };
        let relay = plan_sub_id.0.clone();
        // Displace any repair REQ this plan subscription still owns before
        // opening a new one. It used to be enough to notice that the newly
        // derived id COLLIDED with a pending entry's, but role ids are now
        // reincarnated per mint (#932), so an identical shape no longer
        // yields an identical key and a key collision can never be observed
        // again. A plan carries at most one repair phase at a time -- every
        // route into here first removes its own phase, and every route into a
        // NEW phase goes through `cancel_nip77_repair_for_plan` -- so this
        // sweep is expected to find nothing; it exists so that if that
        // invariant is ever broken the stale repair is retired (nested live
        // candidate included) instead of leaking on the wire.
        let mut displaced = BTreeSet::new();
        self.retire_temporary_reqs_for_plan(&plan_sub_id, &mut displaced);
        let mut ops: Vec<WireOp> = displaced.into_iter().map(WireOp::Close).collect();
        let backlog_sub_id =
            self.mint_nip77_role_sub_id(&plan_sub_id, NIP77_FALLBACK_ROLE, &filter);
        self.insert_pending_backfill(backlog_sub_id.clone(), request);
        let metadata = self
            .plan_execution_metadata
            .get(&plan_sub_id)
            .cloned()
            .expect("a fallback role is derived from an installed plan request");
        let evidence_demands = metadata.owner_demands.clone();
        let public_session = RelaySessionKey::public(relay.clone());
        self.record_observed_request_with_purpose(
            RequestSend {
                session: &public_session,
                sub_id: &backlog_sub_id,
                filter: &filter,
                coverage_claims: metadata.coverage_claims,
                owner_demands: metadata.owner_demands,
                replay: false,
                event_failure_target: EventFailureTarget::ThisSend,
            },
            RequestAttemptPurpose::Nip77Backlog {
                plan_sub_id: plan_sub_id.clone(),
            },
        );
        self.refresh_evidence_for_coverage_and_demand_keys(
            &BTreeSet::new(),
            &evidence_demands,
            effects,
        );
        ops.push(WireOp::Req(backlog_sub_id, filter));
        effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
            ops: vec![(RelaySessionKey::public(relay), ops)],
        })));
        effects.push(Effect::DiagnosticsChanged);
    }

    /// A relay that accepted `limit:0` but never sent its barrier EOSE must
    /// not strand acquisition. Keep that candidate (and any prior live
    /// owner) open while a distinct unlimited backlog REQ supplies a safe
    /// fallback. Its EOSE promotes the already-sent candidate and retires
    /// the predecessor; no Negentropy is attempted on this path.
    pub(super) fn handoff_fallback_to_req(
        &mut self,
        handoff: PendingNegHandoff,
        effects: &mut Vec<Effect>,
    ) {
        let PendingNegHandoff {
            plan_sub_id,
            live_sub_id,
            prior_live_sub_id,
            filter,
            ..
        } = handoff;
        let owner = plan_sub_id.clone();
        self.start_backlog_req(
            plan_sub_id,
            filter,
            TemporaryReq::BacklogActivatesLive {
                plan_sub_id: owner,
                live_sub_id,
                prior_live_sub_id,
            },
            effects,
        );
    }

    /// Abandon a live reconciliation and fall back to a distinct plain REQ
    /// for the same unfloored/unlimited filter. The already-active live REQ
    /// remains open throughout timeout, NEG-ERR, malformed-message, and
    /// store-failure recovery.
    pub(super) fn neg_session_fallback_to_req(
        &mut self,
        sub_id: SubId,
        session: NegSession,
        effects: &mut Vec<Effect>,
    ) {
        effects.push(Effect::NegClose(session.relay.clone(), sub_id.clone()));
        self.neg_session_backlog_fallback(&sub_id, session, effects);
    }

    /// The fallback itself, without the best-effort `NEG-CLOSE`: retire the
    /// reconciliation's own subscription and take the ordinary unlimited
    /// backlog REQ.
    ///
    /// Split out because a reconciliation whose `NEG-OPEN` transport REFUSED
    /// has no session on the relay to close (issue #775) -- every other caller
    /// is abandoning a reconciliation the relay really did open, and keeps the
    /// `NEG-CLOSE`.
    fn neg_session_backlog_fallback(
        &mut self,
        sub_id: &SubId,
        session: NegSession,
        effects: &mut Vec<Effect>,
    ) {
        self.abandon_sub(sub_id);
        let owner = session.plan_sub_id.clone();
        self.start_backlog_req(
            session.plan_sub_id,
            session.filter,
            TemporaryReq::Backlog { plan_sub_id: owner },
            effects,
        );
    }

    pub(super) fn refresh_all_observations(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<ObservationId> = self.observations.keys().copied().collect();
        for id in ids {
            self.refresh_observation(id, effects);
        }
    }

    /// Refresh only acquisition evidence after a coverage-only mutation.
    /// Coverage cannot change canonical rows, so a complete projection can
    /// retain its remembered row set and avoid reopening the store's event
    /// indexes. An incomplete projection still falls back to the full oracle.
    ///
    /// #1646: the production door for every AUTH transition (challenge,
    /// policy/signer/send completion, relay connect/disconnect, epoch
    /// invalidation). Those transitions change session/coverage evidence —
    /// never canonical rows — so they never need `refresh_all_observations`'s
    /// full store reopen.
    pub(super) fn refresh_all_observation_evidence(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<ObservationId> = self.observations.keys().copied().collect();
        for id in ids {
            self.refresh_observation_evidence(id, effects);
        }
    }

    /// Refresh every observation that owns at least one of these BRANCH
    /// handles through the full oracle, exactly once each. One reactive
    /// change touching several branches of the same observation therefore
    /// produces ONE frame, never one frame per affected branch.
    ///
    /// The production committed-mutation path reaches the same observations
    /// through [`Self::apply_committed_row_changes`], which prefers the exact
    /// incremental algebra; this is the forced-full-refresh comparison lane.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn refresh_observations_of_branches(
        &mut self,
        branches: impl IntoIterator<Item = HandleId>,
        effects: &mut Vec<Effect>,
    ) {
        // The resolver also owns internal handles (notably the
        // self-bootstrap discovery query). They participate in graph
        // invalidation but belong to no observation here, so they are
        // filtered out before any store read is opened.
        let mut ids: Vec<ObservationId> = branches
            .into_iter()
            .filter_map(|branch| self.handles.get(&branch).map(|state| state.observation))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            self.refresh_observation(id, effects);
        }
    }

    /// Project one governed store mutation after its crash-atomic commit.
    /// Reactive demand changes may alter router/evidence shape and therefore
    /// keep the broad full-refresh oracle. A stable shape can deliver the
    /// exact durable row facts through #195's fail-safe incremental algebra.
    ///
    /// This is the plain form used by every committed-mutation door that has
    /// no extra non-resolver evidence of its own (`retract`,
    /// `react_to_compensation`, `accept_local`): the resolver's own `delta`
    /// is the ONLY signal for the broad-vs-exact choice.
    pub(super) fn apply_committed_mutation(
        &mut self,
        committed: CommittedMutationResult,
        effects: &mut Vec<Effect>,
    ) {
        self.apply_committed_mutation_with(committed, false, false, effects);
    }

    /// The one shared refresh-vs-apply decision behind every committed-
    /// mutation door, generalized with two force flags for callers that hold
    /// extra evidence the resolver's `delta` cannot see. A locally-pending
    /// write getting
    /// satisfied by a verified relay copy needs every handle re-read even
    /// when neither demand nor directory changed (`force_broad_refresh`,
    /// folded together with `force_recompile` since a directory change also
    /// implies a broad refresh). Both flags default to `false` through
    /// [`Self::apply_committed_mutation`], which reproduces this function's
    /// original (pre-#230) behavior exactly.
    pub(super) fn apply_committed_mutation_with(
        &mut self,
        committed: CommittedMutationResult,
        force_recompile: bool,
        force_broad_refresh: bool,
        effects: &mut Vec<Effect>,
    ) {
        #[cfg(feature = "bench-instrumentation")]
        let total_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        let CommittedMutationResult {
            delta,
            affected_handles,
            row_changes,
        } = committed;
        let invalidated = row_changes
            .removed
            .iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();
        if !invalidated.is_empty() {
            effects.push(Effect::UpdateCommittedObservations {
                invalidated,
                published: Vec::new(),
            });
        }
        let demand_changed = !delta.is_empty();
        let affected: Vec<_> = affected_handles.into_iter().collect();
        let affected_histories: BTreeSet<_> = affected
            .iter()
            .filter_map(|handle| self.history.session_for_handle(*handle))
            .collect();
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::committed_projection_prelude(phase_started.elapsed());

        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        if demand_changed {
            for id in &affected {
                self.reconcile_observation_resolution(
                    *id,
                    ResolutionCause::DependencyChanged,
                    effects,
                );
            }
        }
        let recompiled = demand_changed || force_recompile;
        if recompiled {
            self.recompile(effects);
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::committed_projection_recompile(phase_started.elapsed());

        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        if !recompiled {
            if force_broad_refresh {
                self.refresh_all_observations(effects);
            } else {
                self.apply_committed_row_changes(affected.iter().copied(), &row_changes, effects);
            }
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::committed_live_projection(phase_started.elapsed());

        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        if !recompiled {
            if force_broad_refresh {
                self.refresh_all_histories(effects);
            } else {
                for id in affected_histories {
                    if !self.try_apply_committed_history_row_changes(id, &row_changes, effects) {
                        self.refresh_history(id, WindowLoad::Idle, effects);
                    }
                }
            }
        }
        #[cfg(feature = "bench-instrumentation")]
        {
            crate::ingest_attribution::committed_history_projection(phase_started.elapsed());
            crate::ingest_attribution::committed_projection_total(total_started.elapsed());
        }
    }

    /// Apply a committed writer batch directly to ordinary one-root handle
    /// projections. This is the other half of #177's targeted invalidation:
    /// once the resolver has already proven which handles are affected, a
    /// simple handle should not re-query 60k or 1M prior rows to emit one
    /// exact delta. Complex/multi-root and strict-cache projections keep the
    /// existing full-refresh oracle until their incremental algebra is proven.
    pub(super) fn apply_committed_row_changes(
        &mut self,
        branches: impl IntoIterator<Item = HandleId>,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) {
        let mut ids: Vec<ObservationId> = branches
            .into_iter()
            .filter_map(|branch| self.handles.get(&branch).map(|state| state.observation))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            if !self.try_apply_committed_row_changes(id, changes, effects) {
                self.refresh_observation(id, effects);
            }
        }
    }

    /// Returns `true` when the handle was fully and exactly handled without a
    /// store read (including the no-visible-change case), `false` when the
    /// caller must fall back to `refresh_observation`.
    pub(super) fn try_apply_committed_row_changes(
        &mut self,
        id: ObservationId,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let Some(observation) = self.observations.get(&id) else {
            return true;
        };
        // The incremental algebra below is proven for a single-branch
        // observation with no aggregate bound. A composed observation has to
        // re-derive the union and its global bound across every branch, so it
        // keeps the full-refresh oracle until its own algebra is proven
        // independently -- exactly as multi-root and Strict projections do.
        if observation.branches.len() != 1 || observation.aggregate_result_limit.is_some() {
            return false;
        }
        let branch = observation.branches[0];
        let root_atoms = self.resolver.root_atoms(branch);
        // One currently-resolved root atom is not enough to prove this is
        // an ordinary projection: a Derived/SetOp query can momentarily
        // resolve to one root while still owning interior dependency atoms.
        // Keep those shapes on the full-refresh oracle until their
        // incremental algebra is proven independently.
        if root_atoms.len() != 1 || self.resolver.subtree_atoms(branch).len() != 1 {
            return false;
        }
        let atom = root_atoms
            .first()
            .expect("one-root projection has one concrete atom");
        let Some(branch_state) = self.handles.get(&branch) else {
            return true;
        };
        let state = self
            .observations
            .get(&id)
            .expect("observation was read at the top of this function");
        if branch_state._handle.cache() == CacheMode::Strict
            || state.last_evidence.is_none()
            || !state.projection_complete
        {
            return false;
        }

        let filter = atom.to_nostr();
        let matches = |event: &nostr::Event| filter.match_event(event, MatchEventOptions::new());
        let row_limit = effective_row_limit(&root_atoms);
        let visible_removal = changes
            .removed
            .iter()
            .any(|event| matches(event) && state.last_rows.contains_key(&event.id));
        // A full top-N window may have older candidates outside remembered
        // state. Removing a visible member therefore needs exactly one
        // bounded oracle read to backfill correctly. Insert-only top-N
        // changes are exact from `old top-N ∪ inserted` and stay read-free.
        if row_limit.is_some_and(|limit| state.last_rows.len() == limit && visible_removal) {
            return false;
        }

        // Unlimited handles are the scale-critical case: mutate remembered
        // selection/provenance state in place and allocate only for the
        // committed delta. Cloning the full BTreeMap here would merely trade
        // a full store replay for O(history) memory/time inside the engine.
        if row_limit.is_none() {
            let state = self
                .observations
                .get_mut(&id)
                .expect("observation remained live during synchronous projection");
            let evidence = state
                .last_evidence
                .clone()
                .expect("direct projection requires prior evidence");
            let mut added = BTreeMap::<EventId, Row>::new();
            let mut updated = BTreeMap::<EventId, Row>::new();
            let mut sources_grew = BTreeSet::<EventId>::new();
            let mut removed = BTreeSet::<EventId>::new();

            for event in &changes.removed {
                if matches(event) && state.last_rows.remove(&event.id).is_some() {
                    removed.insert(event.id);
                }
            }
            for row in &changes.inserted {
                if !matches(&row.event) {
                    continue;
                }
                let sources = row.observed_relays.clone();
                state.last_rows.insert(
                    row.event.id,
                    RememberedRow {
                        created_at: row.event.created_at.as_secs(),
                        signature_state: row_signature_from_store_state(
                            &row.event,
                            row.signature_state,
                        ),
                        sources: sources.clone(),
                    },
                );
                added.insert(
                    row.event.id,
                    row_from_stored_event(
                        {
                            #[cfg(feature = "bench-instrumentation")]
                            crate::ingest_attribution::projection_event_clone();
                            row.event.clone()
                        },
                        row.signature_state,
                        sources,
                    ),
                );
            }
            for row in &changes.provenance_grew {
                if !matches(&row.event) {
                    continue;
                }
                if let Some(remembered) = state.last_rows.get_mut(&row.event.id) {
                    let signature_state =
                        row_signature_from_store_state(&row.event, row.signature_state);
                    let signature_changed = remembered.signature_state != signature_state;
                    let prior_len = remembered.sources.len();
                    remembered
                        .sources
                        .extend(row.observed_relays.iter().cloned());
                    remembered.signature_state = signature_state;
                    if signature_changed {
                        updated.insert(
                            row.event.id,
                            row_from_stored_event(
                                row.event.clone(),
                                row.signature_state,
                                remembered.sources.clone(),
                            ),
                        );
                        sources_grew.remove(&row.event.id);
                    } else if remembered.sources.len() != prior_len {
                        sources_grew.insert(row.event.id);
                    }
                }
            }
            for row in &changes.updated {
                if !matches(&row.event) {
                    continue;
                }
                if let Some(remembered) = state.last_rows.get_mut(&row.event.id) {
                    let signature_state =
                        row_signature_from_store_state(&row.event, row.signature_state);
                    remembered.signature_state = signature_state;
                    remembered.sources = row.observed_relays.clone();
                    if let Some(added_row) = added.get_mut(&row.event.id) {
                        *added_row = row_from_stored_event(
                            row.event.clone(),
                            row.signature_state,
                            remembered.sources.clone(),
                        );
                    } else {
                        updated.insert(
                            row.event.id,
                            row_from_stored_event(
                                row.event.clone(),
                                row.signature_state,
                                remembered.sources.clone(),
                            ),
                        );
                    }
                    sources_grew.remove(&row.event.id);
                }
            }

            let changed_current: BTreeSet<_> = added
                .keys()
                .chain(updated.keys())
                .chain(sources_grew.iter())
                .copied()
                .collect();
            let mut delta = Vec::with_capacity(changed_current.len() + removed.len());
            for event_id in changed_current {
                if let Some(row) = added.remove(&event_id) {
                    delta.push(RowDelta::Added(row));
                } else if let Some(row) = updated.remove(&event_id) {
                    delta.push(RowDelta::Updated(row));
                } else {
                    delta.push(RowDelta::SourcesGrew {
                        id: event_id,
                        sources: state.last_rows[&event_id].sources.clone(),
                    });
                }
            }
            delta.extend(removed.into_iter().map(RowDelta::Removed));
            if delta.is_empty() {
                return true;
            }
            effects.push(Effect::EmitRows(id, delta, evidence));
            return true;
        }

        // Bounded handles remember at most N rows, so cloning their small
        // window is bounded by the caller's explicit limit. This makes
        // insertion/eviction and exact delta ordering straightforward.
        let previous = state.last_rows.clone();
        let mut current = previous.clone();
        let mut complete_rows = BTreeMap::<EventId, Row>::new();

        for event in &changes.removed {
            if matches(event) {
                current.remove(&event.id);
            }
        }
        for row in &changes.inserted {
            if !matches(&row.event) {
                continue;
            }
            let sources = row.observed_relays.clone();
            current.insert(
                row.event.id,
                RememberedRow {
                    created_at: row.event.created_at.as_secs(),
                    signature_state: row_signature_from_store_state(
                        &row.event,
                        row.signature_state,
                    ),
                    sources: sources.clone(),
                },
            );
            complete_rows.insert(
                row.event.id,
                row_from_stored_event(
                    {
                        #[cfg(feature = "bench-instrumentation")]
                        crate::ingest_attribution::projection_event_clone();
                        row.event.clone()
                    },
                    row.signature_state,
                    sources,
                ),
            );
        }
        for row in &changes.provenance_grew {
            if !matches(&row.event) {
                continue;
            }
            if let Some(remembered) = current.get_mut(&row.event.id) {
                remembered
                    .sources
                    .extend(row.observed_relays.iter().cloned());
                remembered.signature_state =
                    row_signature_from_store_state(&row.event, row.signature_state);
                complete_rows.insert(
                    row.event.id,
                    row_from_stored_event(
                        row.event.clone(),
                        row.signature_state,
                        remembered.sources.clone(),
                    ),
                );
            }
        }
        for row in &changes.updated {
            if !matches(&row.event) {
                continue;
            }
            if let Some(remembered) = current.get_mut(&row.event.id) {
                remembered.signature_state =
                    row_signature_from_store_state(&row.event, row.signature_state);
                remembered.sources = row.observed_relays.clone();
                complete_rows.insert(
                    row.event.id,
                    row_from_stored_event(
                        row.event.clone(),
                        row.signature_state,
                        remembered.sources.clone(),
                    ),
                );
            }
        }

        let limit = row_limit.expect("unlimited projection returned above");
        if current.len() > limit {
            let mut ordered: Vec<_> = current
                .iter()
                .map(|(event_id, row)| (row.created_at, *event_id))
                .collect();
            ordered.sort_by(|a, b| nip01_newest_first((a.0, &a.1), (b.0, &b.1)));
            let keep: BTreeSet<_> = ordered
                .into_iter()
                .take(limit)
                .map(|(_, event_id)| event_id)
                .collect();
            current.retain(|event_id, _| keep.contains(event_id));
        }

        if current == previous {
            return true;
        }
        let evidence = state
            .last_evidence
            .clone()
            .expect("direct projection requires prior evidence");
        let mut delta = Vec::new();
        for (event_id, remembered) in &current {
            match previous.get(event_id) {
                None => delta.push(RowDelta::Added(
                    complete_rows
                        .remove(event_id)
                        .expect("new direct row came from committed insertion"),
                )),
                Some(last) if last.signature_state != remembered.signature_state => {
                    delta.push(RowDelta::Updated(
                        complete_rows
                            .remove(event_id)
                            .expect("signature change carries the complete current row"),
                    ));
                }
                Some(last) if last.sources != remembered.sources => {
                    delta.push(RowDelta::SourcesGrew {
                        id: *event_id,
                        sources: remembered.sources.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        for event_id in previous.keys() {
            if !current.contains_key(event_id) {
                delta.push(RowDelta::Removed(*event_id));
            }
        }

        let state = self
            .observations
            .get_mut(&id)
            .expect("observation remained live during synchronous projection");
        state.last_rows = current;
        effects.push(Effect::EmitRows(id, delta, evidence));
        true
    }

    /// Recompute ONE observation's merged row set + per-branch acquisition
    /// evidence and emit `Effect::EmitRows` only if either actually changed.
    ///
    /// Every branch is recomputed inside this ONE pass, so a reactive change
    /// touching several branches lands as one atomic transition: the app
    /// never sees a frame in which one branch has already re-rooted and
    /// another has not. Rows are unioned by event id with provenance merged,
    /// then the declared aggregate result limit is applied ONCE to that union
    /// (never per branch), and the delta is diffed against the observation's
    /// own last delivered state -- never the full current set (see
    /// `RowDelta`'s doc: this is what keeps a long-running subscription's
    /// total delivered row volume ~O(distinct rows) instead of O(rows squared)).
    /// Evidence can change with no row change at all (a watermark advancing,
    /// or a source's link status flipping) -- that case still emits, carrying
    /// an EMPTY row delta alongside the new evidence.
    pub(super) fn refresh_observation(&mut self, id: ObservationId, effects: &mut Vec<Effect>) {
        // A read failure while snapshotting any branch (issue #122) degrades
        // to read-only: leave the observation's LAST delivered rows untouched
        // (never fabricate a phantom retraction from a failed read) and
        // surface the degrade on diagnostics instead of panicking.
        let (current, evidence) = match self.observation_rows_and_evidence(id) {
            Ok(Some(value)) => value,
            Ok(None) => return,
            Err(error) => {
                if let Some(state) = self.observations.get_mut(&id) {
                    state.projection_complete = false;
                }
                self.degrade_store(error, effects);
                return;
            }
        };
        if let Some(seed) = self.apply_observation_projection(id, current, evidence) {
            effects.push(Effect::EmitRows(id, seed.deltas, seed.evidence));
        }
    }

    /// Fold ONE recomputed union into the observation's delivered state and
    /// return the exact transition, or `None` when nothing changed. Splitting
    /// this out of [`Self::refresh_observation`] is what lets an open prove
    /// its canonical projection BEFORE any effect is created (#1153).
    fn apply_observation_projection(
        &mut self,
        id: ObservationId,
        current: BTreeMap<EventId, Row>,
        evidence: Vec<AcquisitionEvidence>,
    ) -> Option<RowsSeed> {
        let state = self.observations.get_mut(&id)?;
        let current_rows: BTreeMap<EventId, RememberedRow> = current
            .iter()
            .map(|(id, row)| {
                (
                    *id,
                    RememberedRow {
                        created_at: row.created_at().as_secs(),
                        signature_state: row.signature(),
                        sources: row.sources.clone(),
                    },
                )
            })
            .collect();
        state.projection_complete = true;
        if current_rows == state.last_rows && state.last_evidence.as_ref() == Some(&evidence) {
            return None;
        }
        let mut delta: Vec<RowDelta> = Vec::new();
        for (event_id, row) in current {
            match state.last_rows.get(&event_id) {
                None => delta.push(RowDelta::Added(row)),
                Some(last) if last.signature_state != row.signature() => {
                    delta.push(RowDelta::Updated(row));
                }
                Some(last) if last.sources != row.sources => {
                    delta.push(RowDelta::SourcesGrew {
                        id: event_id,
                        sources: row.sources,
                    });
                }
                Some(_) => {}
            }
        }
        for old_id in state.last_rows.keys() {
            if !current_rows.contains_key(old_id) {
                delta.push(RowDelta::Removed(*old_id));
            }
        }
        state.last_rows = current_rows;
        state.last_evidence = Some(evidence.clone());
        Some(RowsSeed {
            deltas: delta,
            evidence,
        })
    }

    fn refresh_observation_evidence(&mut self, id: ObservationId, effects: &mut Vec<Effect>) {
        let Some(state) = self.observations.get(&id) else {
            return;
        };
        if !state.projection_complete {
            self.refresh_observation(id, effects);
            return;
        }

        // A coverage read that could not answer leaves the LAST delivered
        // evidence in place and degrades, exactly as `refresh_observation`
        // does for a failed row snapshot (#122/#763): re-emitting evidence
        // computed from a failed read would republish "nothing proven" over
        // a watermark this reducer simply could not see.
        let evidence = match self.observation_evidence_for(id) {
            Ok(evidence) => evidence,
            Err(error) => {
                self.degrade_store(error, effects);
                return;
            }
        };
        let Some(state) = self.observations.get_mut(&id) else {
            return;
        };
        if state.last_evidence.as_ref() == Some(&evidence) {
            return;
        }
        state.last_evidence = Some(evidence.clone());
        effects.push(Effect::EmitRows(id, Vec::new(), evidence));
    }

    /// One acquisition-evidence entry per canonical branch, in branch order.
    ///
    /// Branch identity is never erased: two branches that resolved the same
    /// scalar value keep separate entries, and a branch whose sources are all
    /// unreachable reports its own shortfall without any sibling's proof
    /// masking it. Nothing here is rolled up into a query-global verdict.
    fn observation_evidence_for(
        &self,
        id: ObservationId,
    ) -> Result<Vec<AcquisitionEvidence>, PersistenceError> {
        let Some(state) = self.observations.get(&id) else {
            return Ok(vec![AcquisitionEvidence {
                sources: Vec::new(),
                shortfall: vec![ShortfallFact::NoResolvedDemand],
            }]);
        };
        let mut evidence = Vec::with_capacity(state.branches.len());
        for branch in &state.branches {
            evidence.push(self.branch_evidence_for(*branch)?);
        }
        Ok(evidence)
    }

    fn branch_evidence_for(&self, id: HandleId) -> Result<AcquisitionEvidence, PersistenceError> {
        let Some(state) = self.handles.get(&id) else {
            return Ok(AcquisitionEvidence {
                sources: Vec::new(),
                shortfall: vec![ShortfallFact::NoResolvedDemand],
            });
        };
        self.acquisition_evidence_for_scopes(self.resolver.demand_scopes(id), &state.acquisition)
    }

    /// The observation's merged current row set plus its per-branch evidence.
    /// `Ok(None)` means the observation was withdrawn concurrently.
    fn observation_rows_and_evidence(
        &self,
        id: ObservationId,
    ) -> Result<Option<ObservationProjection>, PersistenceError> {
        if !self.observations.contains_key(&id) {
            return Ok(None);
        }
        let rows = self.observation_rows_for(id)?;
        let evidence = self.observation_evidence_for(id)?;
        Ok(Some((rows, evidence)))
    }

    /// The observation's merged current row set.
    ///
    /// Branch rows are unioned by EVENT ID -- the one row-identity the store
    /// and the app already share -- with provenance merged, so a row admitted
    /// by two branches appears once carrying both branches' sources. No
    /// protocol coordinate or resolved scalar ever replaces event-id union
    /// semantics. The declared aggregate bound applies to the MERGED union,
    /// once, in NIP-01 canonical newest-first order -- never N rows per branch
    /// presented as one N-row result.
    fn observation_rows_for(
        &self,
        id: ObservationId,
    ) -> Result<BTreeMap<EventId, Row>, PersistenceError> {
        let Some(state) = self.observations.get(&id) else {
            return Ok(BTreeMap::new());
        };
        let branches = state.branches.clone();
        let aggregate_result_limit = state.aggregate_result_limit;
        let mut union: BTreeMap<EventId, Row> = BTreeMap::new();
        for branch in &branches {
            for (event_id, row) in self.rows_for(*branch)? {
                match union.entry(event_id) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(row);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().sources.extend(row.sources);
                    }
                }
            }
        }
        if let Some(limit) = aggregate_result_limit {
            if union.len() > limit {
                let mut ordered: Vec<(u64, EventId)> = union
                    .iter()
                    .map(|(event_id, row)| (row.created_at().as_secs(), *event_id))
                    .collect();
                ordered.sort_by(|a, b| nip01_newest_first((a.0, &a.1), (b.0, &b.1)));
                let keep: BTreeSet<EventId> =
                    ordered.into_iter().take(limit).map(|(_, id)| id).collect();
                union.retain(|event_id, _| keep.contains(event_id));
            }
        }
        Ok(union)
    }

    /// The query's current matching row set (by id) + its
    /// [`AcquisitionEvidence`] -- an internal snapshot `refresh_observation`
    /// diffs against the handle's own remembered `last_rows` to compute the
    /// outgoing delta. This snapshot itself is never handed to a caller/
    /// effect directly.
    ///
    /// #124: when the demand carries a Nostr `limit:N` this projection is the
    /// N MOST RECENT matching rows -- `created_at` DESC, ties broken by event
    /// `id` ASC (bytewise), the NIP-01 canonical newest-first order -- NOT
    /// every cached match. The authoritative cap lives HERE, at the handle
    /// projection, deliberately NOT in `RedbStore::query` (which must keep
    /// returning every current match: unlimited Derived-node recompute,
    /// negentropy, and ingest callers rely on its FULL match set. Explicitly
    /// limited Derived nodes use `query_newest` at their own projection seam;
    /// that is a separate NIP-01 event-selection operation, not a mutation of
    /// `query()`'s complete-set contract.
    /// For this projection alone, each root atom may be pre-bounded through
    /// `RedbStore::query_newest`; taking N newest from each atom is exact
    /// because a row outside one atom's top N already has N newer witnesses
    /// in that same atom. The final merged/deduped set is still capped ONCE,
    /// per NIP-01 per-subscription `limit` (see [`effective_row_limit`]).
    /// Because `refresh_observation` diffs THIS truncated snapshot against
    /// `last_rows`, the top-N is maintained reactively for free: a newer
    /// match entering the top-N evicts the oldest (Added(new)+Removed(oldest),
    /// never exceeding N), and retracting a top-N member pulls the next-newest
    /// in. `limit: None` is unchanged -- every match, no ordering imposed.
    /// Row truncation NEVER touches `evidence` below (coverage is about what
    /// was acquired, not how many rows are shown -- ledger #17): a limited
    /// query still records no coverage watermark.
    ///
    /// Rows are computed over `root_atoms` alone (delivery
    /// shape unchanged); evidence is computed over `subtree_atoms` (#12: the
    /// query's FULL subtree, interior `Derived` atoms included). Each row
    /// carries its provenance (#105: `StoredEvent::provenance`, already
    /// merged/persisted by `RedbStore::insert`'s dedup path) rather than
    /// discarding it -- the mechanism already exists in `nmp-store`; this is
    /// only its honest projection.
    ///
    /// #107: `CacheMode::Strict` applies the root Demand's pinned cache
    /// projection here -- a cached row is returned only when
    /// `nmp_store::Provenance::visible_under_pin` admits it against the
    /// handle's own pinned relay set (`Row.sources`, #105's existing field;
    /// no new store mechanism, and no second way to say where a row came
    /// from). This is read off THIS
    /// handle's own `QueryHandle::cache()`, never the shared graph node's --
    /// two handles sharing the identical (cache-free-deduped) acquisition
    /// key may still disagree on `cache` (Fable's ruling: cache is excluded
    /// from `AcquisitionKey`), so an Agnostic and a Strict handle over the
    /// same pinned root selection MUST project different row sets despite
    /// sharing one graph/wire/coverage underneath. The pinned relay set
    /// itself comes only from `root_atoms`' `source`: every nested
    /// `Derived.inner` Demand owns an independent source and cache policy,
    /// so consulting a descendant here would let its pins contaminate the
    /// caller-owned root projection.
    /// `CacheMode::Strict` is only meaningful over a `SourceAuthority::
    /// Pinned` selection (the Contract: "pinned cache policy is part of
    /// source identity") -- over any other source there is no pinned relay
    /// set to intersect against, so Strict is a no-op there, identical to
    /// Agnostic.
    pub(super) fn rows_for(
        &self,
        id: HandleId,
    ) -> Result<BTreeMap<EventId, Row>, PersistenceError> {
        let root_atoms = self.resolver.root_atoms(id);
        let demand_scopes = self.resolver.demand_scopes(id);
        let pinned_relays: Option<BTreeSet<RelayUrl>> = self
            .handles
            .get(&id)
            .filter(|state| state._handle.cache() == CacheMode::Strict)
            .and_then(|_| {
                demand_scopes
                    .first()
                    .into_iter()
                    .flat_map(|(atoms, _)| atoms)
                    .find_map(|atom| match &atom.source {
                        SourceAuthority::Pinned(relays) => Some(relays.clone()),
                        _ => None,
                    })
            });

        let row_limit = effective_row_limit(&root_atoms);
        let mut by_id: BTreeMap<EventId, Row> = BTreeMap::new();
        for atom in &root_atoms {
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.projection_store_queries
                .set(self.projection_store_queries.get().saturating_add(1));
            let filter = atom.to_nostr();
            let rows = match row_limit {
                Some(limit) => self.store.query_newest(&filter, limit)?,
                None => self.store.query(&filter)?,
            };
            for se in rows {
                if let Some(pinned) = &pinned_relays {
                    if !se.provenance.visible_under_pin(pinned) {
                        continue;
                    }
                }
                by_id.entry(se.event.id).or_insert_with(|| {
                    let signature_state = se
                        .provenance
                        .local
                        .as_ref()
                        .map_or(SigState::Signed, |local| local.sig_state);
                    row_from_stored_event(
                        se.event,
                        signature_state,
                        se.provenance.seen.into_keys().collect(),
                    )
                });
            }
        }
        // #124: a demand carrying `limit:N` projects only its N newest rows.
        // Applied authoritatively to the merged/deduped set in NIP-01
        // canonical newest-first order. Each root atom was only pre-bounded
        // above; this final pass preserves the per-subscription (not
        // per-atom) contract. `refresh_observation`'s diff then maintains the
        // top-N reactively. No-op when there is no limit or the set fits.
        if let Some(limit) = row_limit {
            if by_id.len() > limit {
                let mut ordered: Vec<(u64, EventId)> = by_id
                    .iter()
                    .map(|(event_id, row)| (row.created_at().as_secs(), *event_id))
                    .collect();
                ordered.sort_by(|a, b| nip01_newest_first((a.0, &a.1), (b.0, &b.1)));
                let keep: BTreeSet<EventId> =
                    ordered.into_iter().take(limit).map(|(_, id)| id).collect();
                by_id.retain(|event_id, _| keep.contains(event_id));
            }
        }
        Ok(by_id)
    }
}
