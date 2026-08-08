//! Live-query planning, relay repair, and row projection.
//!
//! This module owns subscription lifetimes, router recompilation, discovery,
//! NIP-77 handoff/repair, and committed-store mutations projected to observers.

use super::*;

/// One observation's merged current row set plus its per-BRANCH acquisition
/// evidence, indexed by canonical branch order (#1108). This is the internal
/// snapshot `refresh_observation` diffs against the observation's own last
/// delivered state; it is never handed to a caller or an effect directly.
type ObservationProjection = (BTreeMap<EventId, Row>, Vec<AcquisitionEvidence>);

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

impl<S: EventStore> EngineCore<S> {
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
    pub(crate) fn open_observation(
        &mut self,
        query: LiveQuery,
    ) -> ObservationOpen<ObservationId, RowsSeed> {
        let mut effects = Vec::new();
        // Graph construction can read the store (a `Derived` binding resolves
        // its inner query). The resolver transaction discards every partially
        // built graph node on failure, so this refusal owns no handle or
        // demand atom -- and the branches opened BEFORE the failing one are
        // withdrawn here for the same reason.
        let mut opened: Vec<QueryHandle> = Vec::new();
        for branch in query.branches() {
            match self.resolver.subscribe(branch.clone()) {
                Ok((handle, _delta)) => opened.push(handle),
                Err(error) => {
                    for handle in opened {
                        let _ = self.resolver.unsubscribe(handle.id());
                    }
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
            match self.decide_handle_acquisition(id, freshness) {
                Ok(acquisition) => acquisitions.push(acquisition),
                Err(error) => {
                    for handle in opened {
                        let _ = self.resolver.unsubscribe(handle.id());
                    }
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
                    drop(self.handles.remove(branch));
                    let _ = self.resolver.unsubscribe(*branch);
                }
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
                    let _delta = self.resolver.unsubscribe(*branch);
                    self.handles.remove(branch);
                }
                self.degrade_store(error, &mut effects);
                return ObservationOpen::Refused { reason, effects };
            }
        };
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
        // A pinned source is the app naming the exact relays this read must
        // ask -- `RelayScope::on`, a NIP-29 host, an operator indexer query
        // (#1251). The app named them, so the socket heeds them; nothing here
        // widens what an unpinned read may reach.
        for branch in query.branches() {
            if let nmp_grammar::SourceAuthority::Pinned(relays) = &branch.source {
                self.heed_relays(relays.iter().cloned());
            }
        }
        match self.open_observation(query) {
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
        for branch in state.branches {
            let _delta = self.resolver.unsubscribe(branch);
            self.handles.remove(&branch);
        }
        self.withdraw_wire_demand(&mut effects);
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
        let demand = self.wire_demand();
        self.attribution.observe_demand(demand.iter());
        // Finding E3 (epic #507): prune `shape_by_key` against the SAME
        // `demand` just observed above, plus every key still `absorbed` by
        // an outstanding attribution snapshot (see `prune_shapes`'s own
        // doc for why the latter is required) -- mirrors the
        // `nip11_information.retain(..)` a few lines below, in the same
        // function, against the same kind of "current authoritative set"
        // (`planned`/`demand`) recompile just established.
        self.attribution.prune_shapes(demand.iter());
        let admitted_demand = self.admit_projected_routing_evidence(&demand);
        let previous_plan = self.router.plan().clone();
        let wire_delta: WireDelta =
            self.router
                .compile(&admitted_demand, &self.routing_facts, self.compile_budget());
        self.apply_router_plan_delta(previous_plan, wire_delta, effects);
    }

    /// Compile exactly the currently-uncovered logical demand as one pending
    /// cohort. Existing plan requests are coverage inputs, never merge or
    /// identity candidates, so this transition cannot rewrite them.
    pub(super) fn flush_wire_admission(&mut self) -> Vec<Effect> {
        let demand = self.wire_demand();
        let covered = Self::planned_demand_keys(self.router.plan());
        let pending: BTreeSet<_> = demand
            .iter()
            .filter(|atom| {
                let key = nmp_router::DemandKey::for_atom(atom);
                !covered.contains(&key) || self.router.plan().limited_demands.contains(&key)
            })
            .cloned()
            .collect();
        if pending.is_empty() {
            return Vec::new();
        }

        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.router_compiles
            .set(self.router_compiles.get().saturating_add(1));
        self.attribution.observe_demand(demand.iter());
        self.attribution.prune_shapes(demand.iter());
        let admitted = self.admit_projected_routing_evidence(&pending);
        let previous_plan = self.router.plan().clone();
        let budget = self.compile_budget();
        let wire_delta = self.router.admit(&admitted, &self.routing_facts, budget);
        let changed = Self::changed_plan_coverage(&previous_plan, self.router.plan());
        let mut effects = Vec::new();
        self.apply_router_plan_delta(previous_plan, wire_delta, &mut effects);
        self.refresh_evidence_for_coverage_keys(&changed, &mut effects);
        effects
    }

    pub(super) fn withdraw_wire_demand(&mut self, effects: &mut Vec<Effect>) {
        let demand = self.wire_demand();
        self.attribution.observe_demand(demand.iter());
        self.attribution.prune_shapes(demand.iter());
        let previous_plan = self.router.plan().clone();
        let previous_diagnostics = self.router.diagnostics().clone();
        let budget = self.compile_budget();
        let wire_delta = self.router.withdraw(&demand, budget);
        let changed = Self::changed_plan_coverage(&previous_plan, self.router.plan());
        let plan_or_diagnostics_changed = !wire_delta.ops.is_empty()
            || !changed.is_empty()
            || self.router.diagnostics() != &previous_diagnostics;
        if plan_or_diagnostics_changed {
            self.apply_router_plan_delta(previous_plan, wire_delta, effects);
            self.refresh_evidence_for_coverage_keys(&changed, effects);
        }
        if self.wire_admission_needed() {
            effects.push(Effect::ArmWireAdmission);
        }
    }

    fn apply_router_plan_delta(
        &mut self,
        previous_plan: RelayPlan,
        wire_delta: WireDelta,
        effects: &mut Vec<Effect>,
    ) {
        let planned = &self.router.plan().reqs;
        // NIP-11 evidence is retained for any URL that appears as SOME
        // planned session's relay (#8): the document is per-URL evidence,
        // and a URL planned only under a protected session still keeps its
        // document current for the moment its Public session is planned too.
        self.nip11_information
            .retain(|relay, _| planned.keys().any(|session| &session.relay == relay));
        // Finding E4 (epic #507): `events_by_session_kind` is bumped once
        // per inbound EVENT (`on_relay_frame`/`on_relay_frames`) but was
        // never pruned when a session permanently left the plan/directory,
        // growing unbounded across relay churn. `diagnostics::build` only
        // ever reads it via `.get(session)` for `session in
        // &diag.per_session`, and `diag.per_session` is itself built
        // straight off `plan.reqs` (`nmp-router`'s `diag::build`) -- i.e.
        // exactly `planned` here -- so no live reader ever consults an
        // entry outside this set. Safe to prune against the SAME
        // "still-planned" key set as `nip11_information` just above.
        self.events_by_session_kind
            .retain(|session, _| planned.contains_key(session));
        // Protected REQs stay parked until the exact current AUTH epoch is
        // ready, but the relay worker must already exist so the server can
        // deliver the challenge that makes readiness possible. Plan keys are
        // unique, so this emits at most one idempotent acquisition edge per
        // current protected session on each recompile. Exact runtime worker
        // reconciliation still owns withdrawal and closes the worker as soon
        // as the final read/write owner disappears.
        effects.extend(
            planned
                .keys()
                .filter(|session| {
                    session.access != AccessContext::Public
                        && !self.auth_ready_sessions.contains_key(*session)
                })
                .cloned()
                .map(Effect::EnsureReadRelay),
        );
        // `router.compile()` above ALWAYS finalizes `prev_plan`/`last_diag`
        // for the full current demand, regardless of whether anything
        // actually changed on the wire (see `Router::compile`'s own body) —
        // so diagnostics is pushed unconditionally here (M5 plan §1.2 step
        // 3: "push it at the end of recompile()"), even on the early return
        // below for a no-op wire delta.
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
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
                && !self.auth_ready_sessions.contains_key(session)
            {
                continue;
            }
            let mut kept_ops: Vec<WireOp> = Vec::new();
            for op in ops {
                match op {
                    WireOp::Req(sub_id, filter) => {
                        // Union across EVERY planned req carrying this
                        // sub-id, never the first match. The router now
                        // guarantees at most one (`Router::compile`'s
                        // release-mode injectivity assert, #899), so this is
                        // a fold over exactly one entry today. Taking the
                        // FIRST match is what turned that router-side
                        // invariant into a SILENT under-credit here: any
                        // second entry's coverage keys were simply never
                        // attributed, so its atoms refetched forever. Folding
                        // instead of picking means this door reports the truth
                        // about whatever the plan actually holds, rather than
                        // depending on an invariant proved somewhere else.
                        let absorbed: BTreeSet<CoverageKey> = self
                            .router
                            .plan()
                            .reqs
                            .get(session)
                            .into_iter()
                            .flatten()
                            .filter(|r| &r.sub_id == sub_id)
                            .flat_map(|r| r.absorbed.iter().copied())
                            .collect();

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
                                let prior_live_sub_id =
                                    self.active_nip77_live.get(sub_id).cloned().or_else(|| {
                                        previous_plan
                                            .reqs
                                            .get(session)
                                            .is_some_and(|reqs| {
                                                reqs.iter().any(|req| &req.sub_id == sub_id)
                                            })
                                            .then(|| sub_id.clone())
                                    });
                                self.begin_neg_handoff(
                                    probed,
                                    sub_id.clone(),
                                    prior_live_sub_id,
                                    filter.clone(),
                                    absorbed,
                                    effects,
                                );
                            }
                            _ => {
                                self.record_observed_request(
                                    session,
                                    sub_id,
                                    filter,
                                    absorbed,
                                    false,
                                    EventFailureTarget::ThisSend,
                                );
                                kept_ops.push(op.clone());
                            }
                        }
                    }
                    WireOp::Close(sub_id) => {
                        kept_ops.extend(self.close_nip77_plan(sub_id, effects));
                    }
                }
            }
            if !kept_ops.is_empty() {
                kept.push((session.clone(), kept_ops));
            }
        }

        if !kept.is_empty() {
            effects.push(Effect::Wire(WireDelta { ops: kept }));
        }
    }

    fn planned_demand_keys(plan: &RelayPlan) -> BTreeSet<nmp_router::DemandKey> {
        plan.reqs
            .values()
            .flatten()
            .flat_map(|request| request.owners.iter().copied())
            .collect()
    }

    pub(super) fn wire_admission_needed(&self) -> bool {
        let covered = Self::planned_demand_keys(self.router.plan());
        self.wire_demand().iter().any(|atom| {
            let key = nmp_router::DemandKey::for_atom(atom);
            !covered.contains(&key) || self.router.plan().limited_demands.contains(&key)
        })
    }

    fn plan_coverage_assignments(
        plan: &RelayPlan,
    ) -> BTreeMap<CoverageKey, (bool, BTreeSet<(RelaySessionKey, SubId)>)> {
        let mut assignments = BTreeMap::new();
        for (session, requests) in &plan.reqs {
            for request in requests {
                for key in &request.absorbed {
                    assignments
                        .entry(*key)
                        .or_insert_with(|| (false, BTreeSet::new()))
                        .1
                        .insert((session.clone(), request.sub_id.clone()));
                }
            }
        }
        for key in &plan.limited {
            assignments
                .entry(*key)
                .or_insert_with(|| (false, BTreeSet::new()))
                .0 = true;
        }
        assignments
    }

    fn changed_plan_coverage(previous: &RelayPlan, next: &RelayPlan) -> BTreeSet<CoverageKey> {
        let previous = Self::plan_coverage_assignments(previous);
        let next = Self::plan_coverage_assignments(next);
        previous
            .keys()
            .chain(next.keys())
            .filter(|key| previous.get(*key) != next.get(*key))
            .copied()
            .collect()
    }

    fn refresh_evidence_for_coverage_keys(
        &mut self,
        keys: &BTreeSet<CoverageKey>,
        effects: &mut Vec<Effect>,
    ) {
        if keys.is_empty() {
            return;
        }
        let observations: BTreeSet<_> = self
            .handles
            .iter()
            .filter_map(|(handle, state)| {
                self.wire_atoms_for_handle(*handle, &state.acquisition)
                    .iter()
                    .any(|atom| keys.contains(&nmp_store::coverage_key(atom)))
                    .then_some(state.observation)
            })
            .collect();
        let histories: BTreeSet<_> = self
            .histories
            .iter()
            .filter_map(|(history, state)| {
                state
                    .handle_ids
                    .iter()
                    .copied()
                    .any(|handle| {
                        let branch = state.branch_of.get(&handle).copied().unwrap_or_default();
                        state
                            .acquisitions_by_branch
                            .get(branch)
                            .is_some_and(|acquisition| {
                                self.wire_atoms_for_handle(handle, acquisition)
                                    .iter()
                                    .any(|atom| keys.contains(&nmp_store::coverage_key(atom)))
                            })
                    })
                    .then_some(*history)
            })
            .collect();
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
        let ordinary = self
            .handles
            .iter()
            .flat_map(|(id, state)| self.wire_atoms_for_handle(*id, &state.acquisition));
        let history = self
            .histories
            .values()
            .flat_map(|state| state.handle_ids.iter().copied())
            .flat_map(|id| {
                let state = self
                    .histories
                    .get(&self.history_by_handle[&id])
                    .expect("history handle maps to a live session");
                // A window handle contributes wire work under ITS OWN
                // branch's opening-time decision, never a sibling branch's.
                let branch = state.branch_of.get(&id).copied().unwrap_or_default();
                state
                    .acquisitions_by_branch
                    .get(branch)
                    .map(|acquisition| self.wire_atoms_for_handle(id, acquisition))
                    .unwrap_or_default()
            });
        ordinary.chain(history).collect()
    }

    fn wire_atoms_for_handle(
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

    /// Compile an isolated plan through the same router/directory/admission/
    /// cap path as a live recompile, without mutating live wire,
    /// attribution, diagnostics, or any handle. Used once for `MaxAge` and
    /// for staged history projection.
    ///
    /// TRAP, since wire ids became allocated tokens (#899): this builds a
    /// FRESH `Router`, whose mint counter also starts at zero, so its plan's
    /// `SubId`s collide BY VALUE with the live router's tokens while
    /// identifying entirely unrelated filters. That is benign only because
    /// every consumer of a retained shadow plan (`plan_is_fresh_for`, and
    /// `acquisition_evidence` via a scope's retained evaluation plan)
    /// reads sessions, `absorbed`, and `limited` — never `sub_id`. Correlating
    /// a shadow plan's `sub_id` with a live one would alias silently; if that
    /// is ever needed, give this router its own token namespace first.
    pub(super) fn shadow_plan_for(&self, demand: BTreeSet<ContextualAtom>) -> RelayPlan {
        let admitted = demand
            .into_iter()
            .map(|mut atom| {
                atom.routing_evidence.retain(|evidence| {
                    self.admission
                        .admits(&evidence.relay, super::Declarer::SomeoneElse)
                        .is_ok()
                });
                atom
            })
            .collect();
        let mut router = Router::new(RuleRegistry::default_widen_only());
        // The SAME budget the live recompile plans within, deliberately.
        // A shadow plan feeds `plan_is_fresh_for`, which refuses to call a
        // `limited` atom fresh -- so an unbudgeted shadow would call an atom
        // fresh that the live plan had refused to request at all.
        let _ = router.compile(&admitted, &self.routing_facts, self.compile_budget());
        router.plan().clone()
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
    ) -> Result<HandleAcquisition, PersistenceError> {
        let mut scopes = self.resolver.demand_scopes(id);
        if let Some((_, freshness)) = scopes.first_mut() {
            *freshness = root_freshness;
        }
        let candidate_plan = scopes
            .iter()
            .any(|(_, freshness)| matches!(freshness, Freshness::MaxAge { .. }))
            .then(|| {
                let mut candidate_demand = self.wire_demand();
                candidate_demand.extend(
                    scopes
                        .iter()
                        .filter(|(_, freshness)| *freshness != Freshness::CacheOnly)
                        .flat_map(|(atoms, _)| atoms.iter().cloned()),
                );
                self.shadow_plan_for(candidate_demand)
            });
        let mut decided = Vec::with_capacity(scopes.len());
        for (atoms, freshness) in scopes {
            decided.push(match freshness {
                Freshness::Live => ScopeAcquisition::Live,
                Freshness::CacheOnly => ScopeAcquisition::CacheOnly(RelayPlan::default()),
                Freshness::MaxAge { seconds } => {
                    let plan = candidate_plan
                        .as_ref()
                        .expect("a MaxAge scope built the candidate plan");
                    if self.plan_is_fresh_for(&atoms, plan, seconds)? {
                        ScopeAcquisition::CoverageSatisfied(plan.clone())
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
        let auth_status = self.auth_status_map();
        let finished_stored_events = self.finished_stored_events();
        let mut parts = Vec::with_capacity(scopes.len());
        for ((atoms, _), decision) in scopes.into_iter().zip(&acquisition.scopes) {
            let plan = decision
                .evidence_plan()
                .unwrap_or_else(|| self.router.plan());
            parts.push(evidence::acquisition_evidence(
                &atoms,
                plan,
                self.resolver.store(),
                &self.connected_relays,
                &auth_status,
                &self.ever_connected_relays,
                &finished_stored_events,
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

    /// Unanimous current-assignment freshness. Presence of a matching event
    /// is deliberately irrelevant: a coverage row proves the question was
    /// checked, so an empty cached result can satisfy `MaxAge` too.
    ///
    /// Fallible (#763), and not merely for tidiness: `false` here means
    /// "the cache is not fresh enough, go to the network", which is a real
    /// decision made about real coverage rows. A store read that could not
    /// answer has decided nothing, so it leaves through `Err` and the caller
    /// degrades — it never gets to vote `false`.
    pub(super) fn plan_is_fresh_for(
        &self,
        atoms: &BTreeSet<ContextualAtom>,
        plan: &RelayPlan,
        max_age_seconds: u64,
    ) -> Result<bool, PersistenceError> {
        if atoms.is_empty() {
            return Ok(false);
        }
        let cutoff = Timestamp::from(self.clock.as_secs().saturating_sub(max_age_seconds));
        for atom in atoms {
            let key = nmp_store::coverage_key(atom);
            if plan.limited.contains(&key) {
                return Ok(false);
            }
            let covering: Vec<&RelaySessionKey> = plan
                .reqs
                .iter()
                .filter_map(|(session, reqs)| {
                    reqs.iter()
                        .any(|request| request.absorbed.contains(&key))
                        .then_some(session)
                })
                .collect();
            if covering.is_empty() {
                return Ok(false);
            }
            let floor = Timestamp::from(atom.filter.since.unwrap_or(0));
            for session in covering {
                let proven = self
                    .resolver
                    .store()
                    .get_coverage(key, &session.relay)?
                    .is_some_and(|interval| interval.from <= floor && interval.through >= cutoff);
                if !proven {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Gate every network-sourced selector hint/provenance URL before it can
    /// become a router candidate.
    ///
    /// A relay hint in an `e`/`p`/`a` tag is the cheapest thing in Nostr to
    /// forge, and a row's observed-source provenance is arrival rather than
    /// authorship, so both are always [`Declarer::SomeoneElse`] however
    /// familiar the relay they name looks. Operator-configured lanes never
    /// travel this path; they are a trusted declaration made elsewhere.
    pub(super) fn admit_projected_routing_evidence(
        &mut self,
        demand: &BTreeSet<ContextualAtom>,
    ) -> BTreeSet<ContextualAtom> {
        let mut rejected_now = BTreeSet::new();
        let admitted = demand
            .iter()
            .cloned()
            .map(|mut atom| {
                let atom_selection = atom.filter.hash();
                atom.routing_evidence.retain(|evidence| {
                    let admitted = self
                        .admission
                        .admits(&evidence.relay, super::Declarer::SomeoneElse)
                        .is_ok();
                    if !admitted {
                        rejected_now.insert((atom_selection, evidence.clone()));
                    }
                    admitted
                });
                atom
            })
            .collect();
        let newly_rejected = rejected_now
            .difference(&self.rejected_projected_evidence)
            .count() as u64;
        self.discovered_private_relays_rejected = self
            .discovered_private_relays_rejected
            .saturating_add(newly_rejected);
        self.rejected_projected_evidence = rejected_now;
        admitted
    }

    /// One exact request is abandoned. It may no longer earn coverage or
    /// produce a settlement fact.
    pub(super) fn abandon_sub(&mut self, sub_id: &SubId) {
        self.attribution.discard_sub(sub_id);
        self.active_request_evidence
            .retain(|_, request| request.sub_id != *sub_id);
        self.live_wire_requests
            .retain(|(_, candidate), _| candidate != sub_id);
    }

    /// A session dropped. Every attributed request on it is dead; replay
    /// creates fresh request revisions after reconnect.
    pub(super) fn abandon_session_subs(&mut self, session: &RelaySessionKey) {
        self.attribution.clear_session(session);
        self.active_request_evidence
            .retain(|_, request| request.session != *session);
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
        absorbed: BTreeSet<CoverageKey>,
        effects: &mut Vec<Effect>,
    ) {
        let stale_closes = self.cancel_nip77_repair_for_plan(&plan_sub_id, effects);
        if !stale_closes.is_empty() {
            effects.push(Effect::Wire(WireDelta {
                ops: vec![(RelaySessionKey::public(probed.url().clone()), stale_closes)],
            }));
        }

        if let Some(prior) = prior_live_sub_id.as_ref() {
            self.active_nip77_live
                .insert(plan_sub_id.clone(), prior.clone());
        }

        let live_filter = ConcreteFilter {
            limit: Some(0),
            ..filter.clone()
        };
        let live_sub_id = self.mint_nip77_role_sub_id(&plan_sub_id, NIP77_LIVE_ROLE, &live_filter);
        let public_session = RelaySessionKey::public(probed.url().clone());
        self.record_observed_request(
            &public_session,
            &live_sub_id,
            &live_filter,
            absorbed.clone(),
            false,
            EventFailureTarget::ThisSend,
        );
        self.pending_neg_handoffs.insert(
            live_sub_id.clone(),
            PendingNegHandoff {
                probed,
                plan_sub_id,
                live_sub_id: live_sub_id.clone(),
                prior_live_sub_id,
                filter,
                absorbed,
                started_at: self.clock,
            },
        );
        effects.push(Effect::Wire(WireDelta {
            ops: vec![(public_session, vec![WireOp::Req(live_sub_id, live_filter)])],
        }));
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
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

        let pending: Vec<SubId> = self
            .pending_neg_handoffs
            .iter()
            .filter(|(_, handoff)| &handoff.plan_sub_id == plan_sub_id)
            .map(|(live_id, _)| live_id.clone())
            .collect();
        for live_id in pending {
            self.pending_neg_handoffs.remove(&live_id);
            self.abandon_sub(&live_id);
            closes.insert(live_id);
        }

        let neg_ids: Vec<SubId> = self
            .neg_sessions
            .iter()
            .filter(|(_, session)| &session.plan_sub_id == plan_sub_id)
            .map(|(neg_id, _)| neg_id.clone())
            .collect();
        for neg_id in &neg_ids {
            if let Some(session) = self.neg_sessions.remove(neg_id) {
                self.abandon_sub(neg_id);
                effects.push(Effect::NegClose(session.relay, neg_id.clone()));
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
        let temporary: Vec<SubId> = self
            .pending_backfills
            .iter()
            .filter(|(_, request)| match request {
                TemporaryReq::MissingIds {
                    plan_sub_id: owner, ..
                }
                | TemporaryReq::Backlog { plan_sub_id: owner }
                | TemporaryReq::BacklogActivatesLive {
                    plan_sub_id: owner, ..
                } => owner == plan_sub_id,
            })
            .map(|(sub_id, _)| sub_id.clone())
            .collect();
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
        if let Some(prior) = handoff.prior_live_sub_id.as_ref() {
            if prior != &handoff.live_sub_id {
                self.abandon_sub(prior);
                effects.push(Effect::Wire(WireDelta {
                    ops: vec![(
                        RelaySessionKey::public(handoff.probed.url().clone()),
                        vec![WireOp::Close(prior.clone())],
                    )],
                }));
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
            absorbed,
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
        let local_rows = match self.resolver.store().query(&neg_filter.to_nostr()) {
            Ok(rows) => rows,
            Err(e) => {
                self.degrade_store(e, effects);
                let owner = plan_sub_id.clone();
                self.start_backlog_req(
                    plan_sub_id,
                    neg_filter,
                    absorbed,
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
        let attribution_send = self.record_observed_request(
            &public_session,
            &neg_sub_id,
            &neg_filter,
            absorbed.clone(),
            false,
            EventFailureTarget::ThisSend,
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
        self.neg_sessions.insert(
            neg_sub_id.clone(),
            NegSession {
                plan_sub_id,
                relay: probed.url().clone(),
                filter: neg_filter.clone(),
                absorbed,
                attribution_send,
                started_at: self.clock,
                reconciler,
            },
        );
        effects.push(Effect::NegOpen(probed, neg_sub_id, neg_filter, initial_hex));
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
        relay: &RelayUrl,
        sub_id: &SubId,
        handle: Option<TransportRelayHandle>,
        accepted: bool,
        reason: Option<String>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        match (frame, accepted) {
            // Acceptance means the worker owns the bytes. A probe and a
            // continuing round have no further reducer state to advance --
            // they already advanced, and now honestly.
            (Nip77Frame::Probe, true) | (Nip77Frame::Continue, true) => {}
            (Nip77Frame::Open, true) => {
                let Some(session) = self.neg_sessions.get(sub_id) else {
                    return effects;
                };
                let filter_hash = session.filter.hash();
                let public_session = RelaySessionKey::public(relay.clone());
                effects.extend(self.on_wire_request_handoff(
                    &public_session,
                    sub_id,
                    filter_hash,
                    handle,
                    true,
                    None,
                ));
            }
            (Nip77Frame::Probe, false) => {
                if self
                    .prober
                    .refuse_probe(relay, &crate::core::wire_sub_id_string(sub_id))
                {
                    // The projected capability verdict just moved back to
                    // `unknown`; it is observable state, so publish it rather
                    // than waiting for an unrelated recompile.
                    effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
                }
            }
            (Nip77Frame::Open, false) => {
                let Some(session) = self.neg_sessions.remove(sub_id) else {
                    return effects;
                };
                let filter_hash = session.filter.hash();
                let public_session = RelaySessionKey::public(relay.clone());
                // Consume the still-pending request evidence as refused. This
                // is the one place an app can learn that a NIP-77 question was
                // never placed, and it is emitted instead of -- never beside --
                // the `RelayRequest` an accepted handoff would have produced.
                effects.extend(self.on_wire_request_handoff(
                    &public_session,
                    sub_id,
                    filter_hash,
                    handle,
                    false,
                    Some(reason.unwrap_or_else(|| "transport refused NEG-OPEN".to_string())),
                ));
                // No `NEG-CLOSE`: the relay never saw a `NEG-OPEN`, so closing
                // would be a frame about a session that does not exist -- and
                // would compete for the very envelope room that was just
                // refused.
                self.neg_session_backlog_fallback(sub_id, session, &mut effects);
            }
            (Nip77Frame::Continue, false) => {
                let Some(session) = self.neg_sessions.remove(sub_id) else {
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
                self.neg_session_fallback_to_req(sub_id.clone(), session, &mut effects);
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
        let step = session.reconciler.step(message_hex);
        match step {
            Ok(NegStep::Continue(next_hex)) => {
                effects.push(Effect::NegMsg(relay, sub_id, next_hex));
            }
            Ok(NegStep::Done(need_ids)) => {
                let session = self
                    .neg_sessions
                    .remove(&sub_id)
                    .expect("just matched via get_mut above -- still present");
                self.finish_neg_session(sub_id, relay, session, need_ids, effects);
            }
            Err(_) => {
                // A malformed/unexpected reconcile payload from an
                // untrusted relay: abandon this reconciliation and fall
                // back to a plain REQ for the same filter -- the same
                // recovery path as the liveness-deadline/NEG-ERR cases,
                // never a silent read-gap.
                if let Some(session) = self.neg_sessions.remove(&sub_id) {
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
            if self.credit_neg_coverage(&sub_id, attribution_send, completed_at, &relay, effects) {
                self.emit_request_settled(
                    attribution_send,
                    completed_at,
                    RequestTerminal::Nip77,
                    effects,
                );
            } else {
                self.retire_request_evidence(attribution_send);
            }
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
            // sub carries no coverage credit of its own anyway (`absorbed`
            // is empty below; its typed `TemporaryReq::MissingIds` owner
            // unlocks `sub_id`'s credit at EOSE).
            let backfill_sub =
                self.mint_nip77_role_sub_id(&plan_sub_id, NIP77_MISSING_ROLE, &backfill);
            self.pending_backfills.insert(
                backfill_sub.clone(),
                TemporaryReq::MissingIds {
                    plan_sub_id,
                    neg_sub_id: sub_id.clone(),
                    attribution_send,
                    completed_at,
                },
            );
            // No coverage credit of its OWN for this one-shot id-set fetch
            // -- `absorbed` is deliberately empty; it targets exactly the
            // ids negentropy already proved, it is not itself a proof over
            // any atom's shape (the credit it unlocks is `sub_id`'s).
            self.record_observed_request(
                &RelaySessionKey::public(relay.clone()),
                &backfill_sub,
                &backfill,
                BTreeSet::new(),
                false,
                EventFailureTarget::Correlated(attribution_send),
            );
            effects.push(Effect::Wire(WireDelta {
                ops: vec![(
                    RelaySessionKey::public(relay.clone()),
                    vec![WireOp::Req(backfill_sub, backfill)],
                )],
            }));
        }
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
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
    ) -> bool {
        // Negentropy sessions are opened exclusively on the Public session
        // (#8), so their credit resolves through the same Public-session
        // attribution key `open_neg_session` recorded under.
        let attributed = self.attribution.attribute_correlated_completion(
            &RelaySessionKey::public(relay.clone()),
            &wire_sub_id_string(sub_id),
            attribution_send,
            completed_at,
        );
        let settled = attributed
            .is_some_and(|completed| self.persist_attributed_completion(completed, relay, effects));
        self.refresh_all_observation_evidence(effects);
        self.refresh_all_history_evidence(effects);
        settled
    }

    /// The one facts-before-claims persistence door shared by ordinary EOSE
    /// and NEG completion. A poisoned completion performs no store I/O.
    /// Every retained shape is resolved before one atomic request-level
    /// coverage transaction starts, and success effects are emitted only
    /// after that whole transaction commits.
    pub(super) fn persist_attributed_completion(
        &mut self,
        mut completed: CompletedAttribution,
        relay: &RelayUrl,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let Some(claims) = completed.eligible_claims().map(|claims| claims.to_vec()) else {
            return false;
        };
        if claims.is_empty() {
            return true;
        }

        let mut batch = Vec::with_capacity(claims.len());
        for (key, interval) in &claims {
            let Some(atom) = self.attribution.shape_of(*key) else {
                completed.poison(CoveragePoison::MissingShape);
                return false;
            };
            batch.push((atom, relay.clone(), *interval));
        }

        if let Err(error) = self.resolver.store_mut().record_coverage(&batch) {
            completed.poison(CoveragePoison::CoverageCommitFailed);
            self.degrade_store(error, effects);
            return false;
        }

        for (key, interval) in claims {
            effects.push(Effect::RecordCoverage(key, relay.clone(), interval));
        }
        true
    }

    /// Start one unlimited one-shot backlog REQ under a role-separated id.
    /// It never aliases the live NIP-01 id or the NIP-77 session id.
    pub(super) fn start_backlog_req(
        &mut self,
        plan_sub_id: SubId,
        filter: ConcreteFilter,
        absorbed: BTreeSet<CoverageKey>,
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
        self.pending_backfills
            .insert(backlog_sub_id.clone(), request);
        self.record_observed_request(
            &RelaySessionKey::public(relay.clone()),
            &backlog_sub_id,
            &filter,
            absorbed,
            false,
            EventFailureTarget::ThisSend,
        );
        ops.push(WireOp::Req(backlog_sub_id, filter));
        effects.push(Effect::Wire(WireDelta {
            ops: vec![(RelaySessionKey::public(relay), ops)],
        }));
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
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
            absorbed,
            ..
        } = handoff;
        let owner = plan_sub_id.clone();
        self.start_backlog_req(
            plan_sub_id,
            filter,
            absorbed,
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
            session.absorbed,
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
            .filter_map(|handle| self.history_by_handle.get(handle).copied())
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
        if demand_changed || force_recompile {
            self.recompile(effects);
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::committed_projection_recompile(phase_started.elapsed());

        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        if demand_changed || force_broad_refresh {
            self.refresh_all_observations(effects);
        } else {
            self.apply_committed_row_changes(affected.iter().copied(), &row_changes, effects);
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::committed_live_projection(phase_started.elapsed());

        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        if demand_changed || force_broad_refresh {
            self.refresh_all_histories(effects);
        } else {
            for id in affected_histories {
                if !self.try_apply_committed_history_row_changes(id, &row_changes, effects) {
                    self.refresh_history(id, WindowLoad::Idle, effects);
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
                        sources: sources.clone(),
                    },
                );
                added.insert(
                    row.event.id,
                    Row {
                        event: {
                            #[cfg(feature = "bench-instrumentation")]
                            crate::ingest_attribution::projection_event_clone();
                            row.event.clone()
                        },
                        sources,
                    },
                );
            }
            for row in &changes.provenance_grew {
                if !matches(&row.event) {
                    continue;
                }
                if let Some(remembered) = state.last_rows.get_mut(&row.event.id) {
                    let prior_len = remembered.sources.len();
                    remembered
                        .sources
                        .extend(row.observed_relays.iter().cloned());
                    if remembered.sources.len() != prior_len {
                        sources_grew.insert(row.event.id);
                    }
                }
            }

            let changed_current: BTreeSet<_> =
                added.keys().chain(sources_grew.iter()).copied().collect();
            let mut delta = Vec::with_capacity(changed_current.len() + removed.len());
            for event_id in changed_current {
                if let Some(row) = added.remove(&event_id) {
                    delta.push(RowDelta::Added(row));
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
        let mut added = BTreeMap::<EventId, Row>::new();

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
                    sources: sources.clone(),
                },
            );
            added.insert(
                row.event.id,
                Row {
                    event: {
                        #[cfg(feature = "bench-instrumentation")]
                        crate::ingest_attribution::projection_event_clone();
                        row.event.clone()
                    },
                    sources,
                },
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
                    added
                        .remove(event_id)
                        .expect("new direct row came from committed insertion"),
                )),
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
                        created_at: row.event.created_at.as_secs(),
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
                    .map(|(event_id, row)| (row.event.created_at.as_secs(), *event_id))
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
    /// projection, deliberately NOT in `EventStore::query` (which must keep
    /// returning every current match: unlimited Derived-node recompute,
    /// negentropy, and ingest callers rely on its FULL match set. Explicitly
    /// limited Derived nodes use `query_newest` at their own projection seam;
    /// that is a separate NIP-01 event-selection operation, not a mutation of
    /// `query()`'s complete-set contract.
    /// For this projection alone, each root atom may be pre-bounded through
    /// `EventStore::query_newest`; taking N newest from each atom is exact
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
    /// merged/persisted by `EventStore::insert`'s dedup path) rather than
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
                Some(limit) => self.resolver.store().query_newest(&filter, limit)?,
                None => self.resolver.store().query(&filter)?,
            };
            for se in rows {
                if let Some(pinned) = &pinned_relays {
                    if !se.provenance.visible_under_pin(pinned) {
                        continue;
                    }
                }
                by_id.entry(se.event.id).or_insert_with(|| Row {
                    event: se.event,
                    sources: se.provenance.seen.into_keys().collect(),
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
                    .map(|(event_id, row)| (row.event.created_at.as_secs(), *event_id))
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
