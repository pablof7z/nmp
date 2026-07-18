//! Live-query planning, relay repair, and row projection.
//!
//! This module owns subscription lifetimes, router recompilation, discovery,
//! NIP-77 handoff/repair, and committed-store mutations projected to observers.

use super::*;

impl<S: EventStore> EngineCore<S> {
    // ---- subscribe / unsubscribe / re-root ------------------------------

    pub(super) fn on_subscribe(&mut self, query: LiveQuery, sink: Box<dyn RowSink>) -> Vec<Effect> {
        let mut effects = Vec::new();
        // Graph construction can read the store (a `Derived` binding resolves
        // its inner query). On a persistence failure (issue #122) degrade to
        // read-only and install NO handle rather than panic — the observer
        // simply receives no rows.
        let (qh, _delta) = match self.resolver.subscribe(query) {
            Ok(v) => v,
            Err(e) => {
                self.degrade_store(e, &mut effects);
                return effects;
            }
        };
        let id = qh.id();
        let acquisition = self.decide_handle_acquisition(id, qh.freshness());
        self.handles.insert(
            id,
            HandleState {
                _handle: qh,
                acquisition,
                sink,
                last_rows: BTreeMap::new(),
                last_evidence: None,
                projection_complete: false,
            },
        );
        self.recompile(&mut effects);
        // A wire-contributing query can change the capped greedy source plan
        // for every existing query. A suppressed query leaves that plan
        // untouched but still needs its initial cache/evidence frame.
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
        effects
    }

    pub(super) fn on_unsubscribe(&mut self, id: HandleId) -> Vec<Effect> {
        let _delta = self.resolver.unsubscribe(id);
        self.handles.remove(&id);
        let mut effects = Vec::new();
        self.recompile(&mut effects);
        // Removing one query can free capped-plan capacity and therefore
        // change the planned sources of every surviving handle.
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
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
        #[cfg(test)]
        self.router_compiles
            .set(self.router_compiles.get().saturating_add(1));
        let mut demand = self.wire_demand();
        self.sync_discovery(&demand, effects);
        // `sync_discovery` may replace the internal discovery handle. Re-read
        // the immutable handle union so that new/withdrawn discovery work is
        // reflected in this same compile.
        demand = self.wire_demand();
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
                .compile(&admitted_demand, self.directory.as_ref(), self.cap);
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
                .map(Effect::EnsureRelay),
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
                        let absorbed = self
                            .router
                            .plan()
                            .reqs
                            .get(session)
                            .and_then(|reqs| reqs.iter().find(|r| &r.sub_id == sub_id))
                            .map(|r| r.absorbed.clone())
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
                                self.attribution
                                    .record_send(session, sub_id, filter, absorbed);
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

    /// The exact atom union currently owned by handles whose immutable
    /// opening-time freshness decision is `Live`, plus the engine's ordinary
    /// internal discovery handle. Suppressed handles still own their graph
    /// and cache projection, but are absent from this wire truth.
    pub(super) fn wire_demand(&self) -> BTreeSet<ContextualAtom> {
        let ordinary = self
            .handles
            .iter()
            .filter(|(_, state)| state.acquisition.contributes_wire())
            .flat_map(|(id, _)| self.resolver.subtree_atoms(*id));
        let history = self
            .histories
            .values()
            .filter(|state| state.acquisition.contributes_wire())
            .flat_map(|state| state.handle_ids.iter().copied())
            .flat_map(|id| self.resolver.subtree_atoms(id));
        let discovery = self
            .discovery_handle
            .iter()
            .flat_map(|handle| self.resolver.subtree_atoms(handle.id()));
        ordinary.chain(history).chain(discovery).collect()
    }

    /// Compile an isolated plan through the same router/directory/admission/
    /// cap path as a live recompile, without mutating live wire,
    /// attribution, diagnostics, or any handle. Used once for `MaxAge` and
    /// for staged history projection.
    pub(super) fn shadow_plan_for(&self, demand: BTreeSet<ContextualAtom>) -> RelayPlan {
        let admitted = demand
            .into_iter()
            .map(|mut atom| {
                atom.routing_evidence
                    .retain(|evidence| self.admission.admits_discovered(&evidence.relay));
                atom
            })
            .collect();
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let _ = router.compile(&admitted, self.directory.as_ref(), self.cap);
        router.plan().clone()
    }

    /// Freeze one handle's opening-time wire participation. An unsatisfied
    /// `MaxAge` becomes `Live` once and stays there; a satisfied one retains
    /// its exact evaluation plan for evidence and is never re-evaluated.
    pub(super) fn decide_handle_acquisition(
        &self,
        id: HandleId,
        freshness: Freshness,
    ) -> HandleAcquisition {
        match freshness {
            Freshness::Live => HandleAcquisition::Live,
            Freshness::CacheOnly => HandleAcquisition::CacheOnly(RelayPlan::default()),
            Freshness::MaxAge { seconds } => {
                let atoms = self.resolver.subtree_atoms(id);
                let mut candidate_demand = self.wire_demand();
                candidate_demand.extend(atoms.iter().cloned());
                let plan = self.shadow_plan_for(candidate_demand);
                if self.plan_is_fresh_for(&atoms, &plan, seconds) {
                    HandleAcquisition::CoverageSatisfied(plan)
                } else {
                    HandleAcquisition::Live
                }
            }
        }
    }

    /// Unanimous current-assignment freshness. Presence of a matching event
    /// is deliberately irrelevant: a coverage row proves the question was
    /// checked, so an empty cached result can satisfy `MaxAge` too.
    pub(super) fn plan_is_fresh_for(
        &self,
        atoms: &BTreeSet<ContextualAtom>,
        plan: &RelayPlan,
        max_age_seconds: u64,
    ) -> bool {
        if atoms.is_empty() {
            return false;
        }
        let cutoff = Timestamp::from(self.clock.as_secs().saturating_sub(max_age_seconds));
        atoms.iter().all(|atom| {
            let key = nmp_store::coverage_key(atom);
            if plan.limited.contains(&key) {
                return false;
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
            !covering.is_empty()
                && covering.into_iter().all(|session| {
                    let floor = Timestamp::from(atom.filter.since.unwrap_or(0));
                    self.resolver
                        .store()
                        .get_coverage(key, &session.relay)
                        .is_some_and(|interval| {
                            interval.from <= floor && interval.through >= cutoff
                        })
                })
        })
    }

    /// Gate every network-sourced selector hint/provenance URL before it
    /// can become a router candidate. Operator-configured lanes remain
    /// trusted and bypass this path, matching kind:10002 admission policy.
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
                    let admitted = self.admission.admits_discovered(&evidence.relay);
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

    /// The self-bootstrapping outbox (M5, `docs/known-gaps.md`'s
    /// "RelayDirectory" gap): keep an internal kind:10002 discovery
    /// subscription open covering EVERY author current demand has EVER
    /// referenced whose write relays `self.directory` didn't know yet at the
    /// time -- never a permanent/whole-graph scan (still bounded by "every
    /// author this session has actually demanded content for"). Called at
    /// the top of every `recompile` (i.e. on every subscribe/unsubscribe/
    /// re-root/ingest).
    ///
    /// WIDEN-ONLY (`docs/known-gaps.md`'s kind:10002 over-fetch finding: 7112
    /// events received against a 39-author resolved set, root-caused to THIS
    /// function -- see the finding's investigation notes): a newly-demanded
    /// author with unknown relays widens the subscription; an author whose
    /// relays just became known is deliberately left IN the filter rather
    /// than dropped. Reopening on every shrink was the actual bug -- an
    /// author leaving `needed` the moment their kind:10002 resolves used to
    /// tear down and reopen the ENTIRE subscription (dropping that one
    /// author from a fresh, differently-shaped filter), and to a NIP-01
    /// relay an overwriting Req on an already-open sub-id is
    /// indistinguishable from a brand-new subscription: it replies with a
    /// full EOSE replay of every event still matching the new filter. Over N
    /// authors resolving one at a time that is a triangular-number amount of
    /// redelivered events (N+(N-1)+...+1), not O(N) -- exactly the
    /// mechanism behind the 7112-for-39 finding. Leaving a resolved author
    /// in the filter a while longer is widen-safe (matches(wider) ⊇
    /// matches(narrower), the same proof obligation `nmp_router::coalesce`'s
    /// `AuthorUnion` rule already carries) -- it can only mean a few extra,
    /// already-known kind:10002 deliveries for that author, never a
    /// structural over-fetch. The subscription is only ever torn down when
    /// `needed` goes fully empty (every demanded author has resolved, or
    /// none are demanded at all) -- at that point there is nothing left this
    /// discovery sub is for, so it closes rather than idling forever.
    ///
    /// Deliberately reuses the ordinary resolver subscribe/unsubscribe
    /// machinery rather than hand-rolling a parallel subscription system:
    /// the discovery atom this produces (`kinds:[10002], authors:{covered}`)
    /// is just another entry in `resolver.active_demand()`, so the router's
    /// EXISTING discovery-kind eligibility is what routes it to the
    /// configured indexers -- no router-side change was needed for that half
    /// at all. A content atom for an author with no known write relays
    /// simply routes nowhere in the meantime (never an indexer fallback --
    /// "indexers are never a content fallback").
    pub(super) fn sync_discovery(
        &mut self,
        wire_demand: &BTreeSet<ContextualAtom>,
        effects: &mut Vec<Effect>,
    ) {
        let needed: BTreeSet<PubkeyHex> = wire_demand
            .iter()
            .cloned()
            .filter_map(|atom| atom.filter.authors)
            .flatten()
            // NOT `write_relays(..).is_empty()`: that collapses "known,
            // declares zero write relays" into the same signal as "never
            // resolved", which kept a discovery subscription open FOREVER
            // for an author who genuinely has no write relays (ledger #20).
            // `knows_write_relays` distinguishes the two; only a genuinely
            // unresolved author still needs discovery.
            .filter(|author| !self.directory.knows_write_relays(author))
            .collect();

        if needed.is_empty() {
            if self.discovery_handle.is_none() && self.discovery_authors.is_empty() {
                return; // already closed -- nothing to do.
            }
            // Every previously-needed author has resolved (or nothing was
            // ever demanded): nothing left for this sub to cover, so close
            // it. Its `Drop` impl only ENQUEUES the withdrawal; there is
            // nothing to replace it with, so flush explicitly.
            self.discovery_handle = None;
            self.discovery_authors = BTreeSet::new();
            let _ = self.resolver.poll_pending_drops();
            return;
        }

        if needed.is_subset(&self.discovery_authors) {
            // Nothing NEW to cover -- leave the existing subscription
            // exactly as-is, even though it may now be wider than strictly
            // required (see this fn's doc: that's the whole point).
            return;
        }

        // Widen: union in whatever's newly needed and reopen with the
        // WIDENED set. Its `Drop` impl only ENQUEUES the old withdrawal;
        // `resolver.subscribe`'s own drain-on-entry flushes it before
        // building the new atom.
        self.discovery_authors = self.discovery_authors.union(&needed).cloned().collect();
        self.discovery_handle = None;
        let query = LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([NIP65_RELAY_LIST_KIND])),
            authors: Some(Binding::Literal(self.discovery_authors.clone())),
            ..Filter::default()
        });
        // Building the internal discovery subscription can read the store.
        // On a persistence failure (issue #122) degrade to read-only and
        // open no discovery sub rather than panic.
        match self.resolver.subscribe(query) {
            Ok((handle, _delta)) => self.discovery_handle = Some(handle),
            Err(e) => self.degrade_store(e, effects),
        }
    }

    /// After ingesting a possible kind:10002 event for `author`, re-read the
    /// store's CURRENT winning relay-list event for them -- never trust the
    /// just-arrived frame directly. `EventStore::query` only ever returns
    /// the current replaceable-event winner (`nmp-store`'s own contract), so
    /// this is correct regardless of cross-relay arrival order: a stale/
    /// older copy that already lost the replaceable race at `insert` time
    /// can never overwrite the directory with worse data than what the
    /// store itself considers authoritative.
    pub(super) fn ingest_relay_list_winner(
        &mut self,
        author: nostr::PublicKey,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([NIP65_RELAY_LIST_KIND])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        };
        // Re-reading the store's current relay-list winner can fail on I/O
        // (issue #122): degrade to read-only rather than panic. The
        // directory simply isn't updated for this author on this frame.
        let winner = match self.resolver.store().query(&filter.to_nostr()) {
            Ok(rows) => rows.into_iter().next(),
            Err(e) => {
                self.degrade_store(e, effects);
                return false;
            }
        };
        let Some(winner) = winner else {
            return false;
        };
        // Relay admission (issue #121): these relays are DISCOVERED — parsed
        // straight off a network-sourced (validly-signed, but untrusted-
        // content) kind:10002. Gate them on host classification + the
        // operator's opt-in local allowlist BEFORE they become routable
        // `Nip65Write`/`Nip65Read` lanes. A rejected relay never enters the
        // directory, so it never becomes a router candidate and never reaches
        // `pool.ensure_open` — the SSRF / forced-Tor path is closed
        // structurally, not filtered downstream.
        //
        // FORWARD GUARD: this is currently the SOLE network-discovery path
        // into the relay directory. ANY future network-sourced relay ingest —
        // a kind:10050 DM-inbox list, nprofile/nevent relay hints, a
        // provenance "seen here" lane, etc. — MUST route its parsed relays
        // through `self.admission.filter_discovered(..)` before calling
        // `directory.ingest_*`, or the structural exclusion proven here is
        // silently lost for that new source. Discovery is untrusted;
        // operator config (the `LiveDirectory` builder lanes) is not and is
        // deliberately NOT gated here.
        let (write_relays, write_rejected) = self
            .admission
            .filter_discovered(parse_nip65_write_relays(&winner.event));
        let (read_relays, read_rejected) = self
            .admission
            .filter_discovered(parse_nip65_read_relays(&winner.event));
        self.discovered_private_relays_rejected = self
            .discovered_private_relays_rejected
            .saturating_add(write_rejected + read_rejected);
        let author = author.to_hex();
        let before_known = self.directory.knows_write_relays(&author);
        let before_write = self.directory.write_relays(&author);
        let before_read = self.directory.read_relays(&author);
        self.directory
            .ingest_write_relays(author.clone(), write_relays);
        self.directory
            .ingest_read_relays(author.clone(), read_relays);
        before_known != self.directory.knows_write_relays(&author)
            || before_write != self.directory.write_relays(&author)
            || before_read != self.directory.read_relays(&author)
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
        let live_sub_id = nip77_role_sub_id(&plan_sub_id, NIP77_LIVE_ROLE, &live_filter);
        let public_session = RelaySessionKey::public(probed.url().clone());
        self.attribution.record_send(
            &public_session,
            &live_sub_id,
            &live_filter,
            absorbed.clone(),
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
            self.attribution.discard_sub(&live_id);
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
                self.attribution.discard_sub(neg_id);
                effects.push(Effect::NegClose(session.relay, neg_id.clone()));
            }
        }

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
                    self.attribution.discard_sub(&neg_sub_id);
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
                    self.attribution.discard_sub(&live_sub_id);
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
                            self.attribution.discard_sub(&prior);
                            closes.insert(prior);
                        }
                    }
                }
                Some(TemporaryReq::Backlog { .. }) | None => {}
            }
            self.attribution.discard_sub(&sub_id);
            closes.insert(sub_id);
        }

        closes.into_iter().map(WireOp::Close).collect()
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
        self.attribution.discard_sub(&active);
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
                self.attribution.discard_sub(prior);
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
        // Seeding the reconciler reads the local store's holdings for this
        // shape. On an I/O failure (issue #122) degrade to read-only and do
        // not open the session rather than panic — the `Close` pushed above
        // still stands, so the sub-id is simply released.
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
            .map(|se| (se.event.created_at.as_secs(), se.event.id))
            .collect();
        let (reconciler, initial_hex) = Reconciler::open(&local_ids);

        let neg_sub_id = nip77_role_sub_id(&plan_sub_id, NIP77_NEG_ROLE, &neg_filter);

        let attribution_send = self.attribution.record_send(
            &RelaySessionKey::public(probed.url().clone()),
            &neg_sub_id,
            &neg_filter,
            absorbed.clone(),
        );
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
            self.credit_neg_coverage(&sub_id, attribution_send, completed_at, &relay, effects);
            self.attribution.discard_sub(&sub_id);
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
            let backfill_sub = nip77_role_sub_id(&plan_sub_id, NIP77_MISSING_ROLE, &backfill);
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
            self.attribution.record_send(
                &RelaySessionKey::public(relay.clone()),
                &backfill_sub,
                &backfill,
                BTreeSet::new(),
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
    ) {
        // Negentropy sessions are opened exclusively on the Public session
        // (#8), so their credit resolves through the same Public-session
        // attribution key `open_neg_session` recorded under.
        let attributed = self.attribution.attribute_correlated_completion(
            &RelaySessionKey::public(relay.clone()),
            &wire_sub_id_string(sub_id),
            attribution_send,
            completed_at,
        );
        for (key, interval) in attributed {
            if let Some(shape) = self.attribution.shape_of(key) {
                if let Err(e) = self
                    .resolver
                    .store_mut()
                    .record_coverage(&shape, relay, interval)
                {
                    // Coverage-watermark persistence failed (issue #122):
                    // degrade to read-only, claim no watermark that did not
                    // land, and do not panic.
                    self.degrade_store(e, effects);
                    continue;
                }
                effects.push(Effect::RecordCoverage(key, relay.clone(), interval));
            }
        }
        self.refresh_all_handle_evidence(effects);
        self.refresh_all_history_evidence(effects);
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
        let backlog_sub_id = nip77_role_sub_id(&plan_sub_id, NIP77_FALLBACK_ROLE, &filter);
        let relay = plan_sub_id.0.clone();
        let mut ops = Vec::new();
        if self
            .pending_backfills
            .insert(backlog_sub_id.clone(), request)
            .is_some()
        {
            self.attribution.discard_sub(&backlog_sub_id);
            ops.push(WireOp::Close(backlog_sub_id.clone()));
        }
        self.attribution.record_send(
            &RelaySessionKey::public(relay.clone()),
            &backlog_sub_id,
            &filter,
            absorbed,
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
        self.attribution.discard_sub(&sub_id);
        let owner = session.plan_sub_id.clone();
        self.start_backlog_req(
            session.plan_sub_id,
            session.filter,
            session.absorbed,
            TemporaryReq::Backlog { plan_sub_id: owner },
            effects,
        );
    }

    pub(super) fn refresh_all_handles(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<HandleId> = self.handles.keys().copied().collect();
        self.refresh_handles(ids, effects);
    }

    /// Refresh only acquisition evidence after a coverage-only mutation.
    /// Coverage cannot change canonical rows, so a complete projection can
    /// retain its remembered row set and avoid reopening the store's event
    /// indexes. An incomplete projection still falls back to the full oracle.
    pub(super) fn refresh_all_handle_evidence(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<HandleId> = self.handles.keys().copied().collect();
        for id in ids {
            self.refresh_handle_evidence(id, effects);
        }
    }

    pub(super) fn refresh_handles(
        &mut self,
        ids: impl IntoIterator<Item = HandleId>,
        effects: &mut Vec<Effect>,
    ) {
        for id in ids {
            // The resolver also owns internal handles (notably the
            // self-bootstrap discovery query). They participate in graph
            // invalidation but have no app projection state here. Reject
            // them before `refresh_handle` opens any store read.
            if self.handles.contains_key(&id) {
                self.refresh_handle(id, effects);
            }
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
    /// extra evidence the resolver's `delta` cannot see. Relay ingest is the
    /// only such caller today: an NIP-65 directory winner can change the
    /// capped source plan even when the resolver's own demand shape is
    /// unchanged (`force_recompile`), and a locally-pending write getting
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
        if demand_changed || force_recompile {
            self.recompile(effects);
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::committed_projection_recompile(phase_started.elapsed());

        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        if demand_changed || force_broad_refresh {
            self.refresh_all_handles(effects);
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
        ids: impl IntoIterator<Item = HandleId>,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) {
        for id in ids {
            if !self.handles.contains_key(&id) {
                continue;
            }
            if !self.try_apply_committed_row_changes(id, changes, effects) {
                self.refresh_handle(id, effects);
            }
        }
    }

    /// Returns `true` when the handle was fully and exactly handled without a
    /// store read (including the no-visible-change case), `false` when the
    /// caller must fall back to `refresh_handle`.
    pub(super) fn try_apply_committed_row_changes(
        &mut self,
        id: HandleId,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let root_atoms = self.resolver.root_atoms(id);
        // One currently-resolved root atom is not enough to prove this is
        // an ordinary projection: a Derived/SetOp query can momentarily
        // resolve to one root while still owning interior dependency atoms.
        // Keep those shapes on the full-refresh oracle until their
        // incremental algebra is proven independently.
        if root_atoms.len() != 1 || self.resolver.subtree_atoms(id).len() != 1 {
            return false;
        }
        let atom = root_atoms
            .first()
            .expect("one-root projection has one concrete atom");
        let Some(state) = self.handles.get(&id) else {
            return true;
        };
        if state._handle.cache() == CacheMode::Strict
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
                .handles
                .get_mut(&id)
                .expect("handle remained live during synchronous projection");
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
            #[cfg(feature = "bench-instrumentation")]
            let sink_started = std::time::Instant::now();
            #[cfg(feature = "bench-instrumentation")]
            let sink_delta_count = delta.len();
            state.sink.on_rows(delta.clone());
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::row_sink_delivery(sink_started.elapsed(), sink_delta_count);
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
            .handles
            .get_mut(&id)
            .expect("handle remained live during synchronous projection");
        state.last_rows = current;
        #[cfg(feature = "bench-instrumentation")]
        let sink_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let sink_delta_count = delta.len();
        state.sink.on_rows(delta.clone());
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::row_sink_delivery(sink_started.elapsed(), sink_delta_count);
        effects.push(Effect::EmitRows(id, delta, evidence));
        true
    }

    /// Recompute `id`'s current row set + acquisition evidence; emit (and
    /// synchronously deliver to its sink) `Effect::EmitRows` only if either
    /// changed since the last refresh -- and, when something DID change, the
    /// row payload is ALWAYS just the incremental added/sources-grew/removed
    /// delta against `state.last_rows`, never the full current set (see
    /// `RowDelta`'s doc: this is what keeps a long-running subscription's
    /// total delivered row volume ~O(distinct rows) instead of O(rows²)).
    /// Evidence can change with no row change at all (a watermark advancing,
    /// or a source's link status flipping) -- that case still emits,
    /// carrying an EMPTY row delta alongside the new evidence. #105:
    /// per-id provenance growth is detected the SAME way -- a plain value
    /// compare of `state.last_rows`'s remembered source set against this
    /// recompute's -- so a lifecycle-driven recompute of some OTHER
    /// handle's query (`refresh_all_handles`, e.g. on ANY subscribe/
    /// unsubscribe) can never spuriously emit a `SourcesGrew` for a row
    /// whose provenance did not actually change.
    pub(super) fn refresh_handle(&mut self, id: HandleId, effects: &mut Vec<Effect>) {
        // A read failure while snapshotting this handle's rows (issue #122)
        // degrades to read-only: leave the handle's LAST delivered rows
        // untouched (never fabricate a phantom retraction from a failed
        // read) and surface the degrade on diagnostics instead of panicking.
        let (current, evidence) = match self.rows_and_evidence_for(id) {
            Ok(v) => v,
            Err(e) => {
                if let Some(state) = self.handles.get_mut(&id) {
                    state.projection_complete = false;
                }
                self.degrade_store(e, effects);
                return;
            }
        };
        let Some(state) = self.handles.get_mut(&id) else {
            return;
        };
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
            return;
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
        #[cfg(feature = "bench-instrumentation")]
        let sink_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let sink_delta_count = delta.len();
        state.sink.on_rows(delta.clone());
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::row_sink_delivery(sink_started.elapsed(), sink_delta_count);
        effects.push(Effect::EmitRows(id, delta, evidence));
    }

    fn refresh_handle_evidence(&mut self, id: HandleId, effects: &mut Vec<Effect>) {
        let Some(state) = self.handles.get(&id) else {
            return;
        };
        if !state.projection_complete {
            self.refresh_handle(id, effects);
            return;
        }

        let evidence = self.handle_evidence_for(id);
        let Some(state) = self.handles.get_mut(&id) else {
            return;
        };
        if state.last_evidence.as_ref() == Some(&evidence) {
            return;
        }
        state.last_evidence = Some(evidence.clone());
        state.sink.on_rows(Vec::new());
        effects.push(Effect::EmitRows(id, Vec::new(), evidence));
    }

    fn handle_evidence_for(&self, id: HandleId) -> AcquisitionEvidence {
        let subtree_atoms = self.resolver.subtree_atoms(id);
        self.handle_evidence_for_atoms(id, &subtree_atoms)
    }

    fn handle_evidence_for_atoms(
        &self,
        id: HandleId,
        subtree_atoms: &BTreeSet<ContextualAtom>,
    ) -> AcquisitionEvidence {
        let auth_status = self.auth_status_map();
        let evidence_plan = self
            .handles
            .get(&id)
            .and_then(|state| state.acquisition.evidence_plan())
            .unwrap_or_else(|| self.router.plan());
        evidence::acquisition_evidence(
            subtree_atoms,
            evidence_plan,
            self.resolver.store(),
            &self.connected_relays,
            &auth_status,
            &self.ever_connected_relays,
        )
    }

    /// The query's current matching row set (by id) + its
    /// [`AcquisitionEvidence`] -- an internal snapshot `refresh_handle`
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
    /// Because `refresh_handle` diffs THIS truncated snapshot against
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
    /// #107: `CacheMode::Strict` applies the pinned cache projection here --
    /// a cached row is returned only when its unioned provenance set
    /// intersects the handle's own pinned relay set (`Row.sources`, #105's
    /// existing field; no new store mechanism). This is read off THIS
    /// handle's own `QueryHandle::cache()`, never the shared graph node's --
    /// two handles sharing the identical (cache-free-deduped) acquisition
    /// key may still disagree on `cache` (Fable's ruling: cache is excluded
    /// from `AcquisitionKey`), so an Agnostic and a Strict handle over the
    /// same pinned selection MUST project different row sets despite
    /// sharing one graph/wire/coverage underneath. The pinned relay set
    /// itself comes from `subtree_atoms`' `source` -- Fable's ruling B
    /// ("uniform per Demand, not subtree") guarantees every atom in a
    /// single handle's subtree carries the SAME declared `SourceAuthority`,
    /// so any one atom's `source` is authoritative for the whole handle.
    /// `CacheMode::Strict` is only meaningful over a `SourceAuthority::
    /// Pinned` selection (the Contract: "pinned cache policy is part of
    /// source identity") -- over any other source there is no pinned relay
    /// set to intersect against, so Strict is a no-op there, identical to
    /// Agnostic.
    pub(super) fn rows_and_evidence_for(
        &self,
        id: HandleId,
    ) -> Result<(BTreeMap<EventId, Row>, AcquisitionEvidence), PersistenceError> {
        let subtree_atoms = self.resolver.subtree_atoms(id);
        let pinned_relays: Option<&BTreeSet<RelayUrl>> = self
            .handles
            .get(&id)
            .filter(|state| state._handle.cache() == CacheMode::Strict)
            .and_then(|_| {
                subtree_atoms.iter().find_map(|atom| match &atom.source {
                    SourceAuthority::Pinned(relays) => Some(relays),
                    _ => None,
                })
            });

        let root_atoms = self.resolver.root_atoms(id);
        let row_limit = effective_row_limit(&root_atoms);
        let mut by_id: BTreeMap<EventId, Row> = BTreeMap::new();
        for atom in &root_atoms {
            #[cfg(test)]
            self.projection_store_queries
                .set(self.projection_store_queries.get().saturating_add(1));
            let filter = atom.to_nostr();
            let rows = match row_limit {
                Some(limit) => self.resolver.store().query_newest(&filter, limit)?,
                None => self.resolver.store().query(&filter)?,
            };
            for se in rows {
                if let Some(relays) = pinned_relays {
                    if !se
                        .provenance
                        .seen
                        .keys()
                        .any(|relay| relays.contains(relay))
                    {
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
        // per-atom) contract. `refresh_handle`'s diff then maintains the
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
        let evidence = self.handle_evidence_for_atoms(id, &subtree_atoms);
        Ok((by_id, evidence))
    }
}
