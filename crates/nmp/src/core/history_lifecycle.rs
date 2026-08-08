//! Expandable observation-window lifecycle and projection.
//!
//! This module owns staged window growth, commit/rollback, bounded history
//! reconciliation, and mutation projection for active history sessions.

use super::*;

impl<S: EventStore> EngineCore<S> {
    pub(crate) fn open_history_observation(
        &mut self,
        query: HistoryQuery,
    ) -> ObservationOpen<HistorySessionId, HistoryBatch> {
        let mut effects = Vec::new();
        // Every branch's live-top acquisition opens before the session
        // exists. A failure part-way through withdraws what was already
        // opened: a window is installed whole or not at all.
        let mut handles = Vec::new();
        for branch in query.initial_demands() {
            match self.resolver.subscribe(branch) {
                Ok((handle, _)) => handles.push(handle),
                Err(error) => {
                    for handle in handles {
                        let _ = self.resolver.unsubscribe(handle.id());
                    }
                    let reason = format!("canonical history resolution failed: {error}");
                    self.degrade_store(error, &mut effects);
                    return ObservationOpen::Refused { reason, effects };
                }
            }
        }
        // Before the session exists, for the same reason the subscribe loop
        // above unwinds: a coverage read that cannot answer refuses the open
        // outright (#763) rather than deciding `Live` on a failure and
        // presenting that as a policy decision the app asked for.
        let mut acquisitions_by_branch = Vec::with_capacity(handles.len());
        for index in 0..handles.len() {
            let (branch, freshness) = (handles[index].id(), handles[index].freshness());
            match self.decide_handle_acquisition(branch, freshness) {
                Ok(acquisition) => acquisitions_by_branch.push(acquisition),
                Err(error) => {
                    for handle in handles {
                        let _ = self.resolver.unsubscribe(handle.id());
                    }
                    let reason = format!("history freshness decision failed: {error}");
                    self.degrade_store(error, &mut effects);
                    return ObservationOpen::Refused { reason, effects };
                }
            }
        }
        let id = HistorySessionId(self.next_history_id);
        self.next_history_id = self.next_history_id.wrapping_add(1).max(1);
        let live_handle_ids: Vec<HandleId> = handles.iter().map(|handle| handle.id()).collect();
        let mut branch_of = BTreeMap::new();
        for (index, handle) in handles.iter().enumerate() {
            self.history_by_handle.insert(handle.id(), id);
            branch_of.insert(handle.id(), index);
        }
        let handle_ids = live_handle_ids.iter().copied().collect();
        self.histories.insert(
            id,
            HistoryState {
                target_rows: query.page_size(),
                query,
                acquisitions_by_branch,
                handles,
                handle_ids,
                live_handle_ids,
                branch_of,
                acquisitions: BTreeMap::new(),
                acquired_tie_seconds: BTreeSet::new(),
                last_rows: BTreeMap::new(),
                order: BTreeSet::new(),
                last_evidence: None,
                projection_complete: false,
                load: WindowLoad::Idle,
                pending_load: None,
            },
        );

        // As with ordinary observations, canonical materialization is the
        // transaction's fallible gate. It runs before router compilation, so
        // refusal cannot create a speculative relay plan or perturb a sibling
        // observer. The just-created resolver/session owner is then removed
        // inside this same reducer turn.
        let current = match self.history_rows_for(id) {
            Ok(current) => current,
            Err(error) => {
                let reason = format!("canonical history projection failed: {error}");
                let withdrawn = self
                    .histories
                    .remove(&id)
                    .map(|state| state.live_handle_ids)
                    .unwrap_or_default();
                for handle_id in withdrawn {
                    self.history_by_handle.remove(&handle_id);
                    let _ = self.resolver.unsubscribe(handle_id);
                }
                self.degrade_store(error, &mut effects);
                return ObservationOpen::Refused { reason, effects };
            }
        };

        // The opening evidence frame reads coverage and refuses the open on a
        // failed read (#763), the same unwind the canonical row projection
        // above takes -- a window is installed whole or not at all, and a
        // frame whose sources read "nothing proven" because nothing could be
        // read is not a window.
        let evidence = match self.history_evidence_for(id) {
            Ok(evidence) => evidence,
            Err(error) => {
                let reason = format!("history evidence projection failed: {error}");
                let withdrawn = self
                    .histories
                    .remove(&id)
                    .map(|state| state.live_handle_ids)
                    .unwrap_or_default();
                for handle_id in withdrawn {
                    self.history_by_handle.remove(&handle_id);
                    let _ = self.resolver.unsubscribe(handle_id);
                }
                self.degrade_store(error, &mut effects);
                return ObservationOpen::Refused { reason, effects };
            }
        };
        if self.wire_admission_needed() {
            effects.push(Effect::ArmWireAdmission);
        }
        let seed = self
            .apply_history_projection(id, current, evidence, WindowLoad::Idle)
            .expect("a new history session has no prior projection and always yields one seed");
        ObservationOpen::Opened { id, seed, effects }
    }

    pub(super) fn on_subscribe_history(&mut self, query: HistoryQuery) -> Vec<Effect> {
        match self.open_history_observation(query) {
            ObservationOpen::Opened {
                id,
                seed,
                mut effects,
            } => {
                effects.push(Effect::EmitHistory(id, seed));
                effects
            }
            ObservationOpen::Refused { effects, .. } => effects,
        }
    }

    pub(super) fn on_unsubscribe_history(&mut self, id: HistorySessionId) -> Vec<Effect> {
        let Some(state) = self.histories.remove(&id) else {
            return Vec::new();
        };
        for handle in state.handles {
            self.history_by_handle.remove(&handle.id());
            let _ = self.resolver.unsubscribe(handle.id());
        }
        let mut effects = Vec::new();
        self.withdraw_wire_demand(&mut effects);
        effects
    }

    /// Declaratively raise this window's row target (#485). Monotonic,
    /// idempotent, and clamped to the declared `max_rows`. Replaces the old
    /// `on_load_older` continuation-token door: there is no token to validate,
    /// no generation to go stale, and no `LoadInProgress`/`AtBound`/
    /// `NoBoundary` error — an in-flight advance simply raises the target, and
    /// being at the bound is a frame fact, not an error.
    pub(super) fn on_request_rows(&mut self, id: HistorySessionId, at_least: usize) -> Vec<Effect> {
        let Some(state) = self.histories.get(&id) else {
            // The session was withdrawn concurrently. The facade keeps a
            // window's session alive for its whole lifetime, so this is only
            // reachable as a benign teardown race — report Ok, do nothing.
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        };
        let max = state.query.max_rows();
        let old_target = state.target_rows;
        let new_target = old_target.max(at_least).min(max);

        // A staged advance is already in flight. This is only reachable when a
        // caller drives `request_rows` between stage and commit (the runtime
        // commits within one command, so between commands there is never a
        // lingering pending load). Raise the target and defer: the post-commit
        // continuation converges the window to it.
        if state.pending_load.is_some() {
            if new_target != old_target {
                self.histories
                    .get_mut(&id)
                    .expect("history remains live")
                    .target_rows = new_target;
            }
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        }

        if new_target == old_target {
            // Raising the target cannot grow the window.
            if old_target == max {
                // At the declared bound: emit exactly one `AtBound` frame beat
                // (a FACT, never an error) through the normal staged
                // EmitHistory path so mailbox conflation applies uniformly.
                return self.stage_history_atbound(id, max);
            }
            // At or below the current target and below the bound: a pure
            // no-op. Any still-unfilled gap converges through the live
            // acquisition and the post-commit continuation, not a re-request.
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        }

        // Real growth: raise the target and stage one advance toward it.
        self.stage_history_advance(id, new_target)
    }

    /// The canonical older boundary of one window: its oldest retained row in
    /// NIP-01 newest-first order (`created_at ASC`, then `event_id DESC`).
    /// This is the cursor an advance fetches strictly older than. `None` when
    /// the window holds no rows yet.
    pub(super) fn window_boundary(&self, id: HistorySessionId) -> Option<nmp_store::EventCursor> {
        let state = self.histories.get(&id)?;
        state
            .last_rows
            .iter()
            .max_by(|(a_id, a), (b_id, b)| {
                nip01_newest_first(
                    (a.event.created_at.as_secs(), a_id),
                    (b.event.created_at.as_secs(), b_id),
                )
            })
            .map(|(event_id, row)| nmp_store::EventCursor::new(row.event.created_at, *event_id))
    }

    /// Stage one bounded advance toward `new_target`, opening the tie-second
    /// and older-range acquisitions for the current boundary and projecting
    /// the newly exposed lower segment as a prospective plan. Nothing becomes
    /// observable until the runtime's synchronous reply receiver accepts
    /// success and commits (`on_commit_history_load`); on any staging failure
    /// the prior projection is restored exactly (`on_rollback_history_load`)
    /// and the collapsed advance error is reported.
    ///
    /// The advance chunk is the actual shortfall (`target - held`), not a
    /// fixed page size, so a single `request_rows(at_least)` asks the wire for
    /// exactly the rows it still needs.
    pub(super) fn stage_history_advance(
        &mut self,
        id: HistorySessionId,
        new_target: usize,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let boundary = self.window_boundary(id);

        let (
            query,
            prior_target,
            prior_load,
            prior_evidence,
            prior_projection_complete,
            needs_tie,
            old_len,
            needed,
        ) = {
            let state = self
                .histories
                .get(&id)
                .expect("advance requires a live session");
            let prior_target = state.target_rows;
            let old_len = state.last_rows.len();
            let effective_target = new_target.max(prior_target);
            let needed = effective_target.saturating_sub(old_len);
            let needs_tie = boundary.as_ref().is_some_and(|cursor| {
                !state
                    .acquired_tie_seconds
                    .contains(&cursor.created_at.as_secs())
            });
            (
                state.query.clone(),
                prior_target,
                state.load,
                state.last_evidence.clone(),
                state.projection_complete,
                needs_tie,
                old_len,
                needed,
            )
        };

        // Raise the target now: `history_rows_and_evidence_for` /
        // `advance_history_projection` both read `target_rows`.
        {
            let state = self.histories.get_mut(&id).expect("history remains live");
            state.target_rows = state.target_rows.max(new_target);
        }

        let Some(boundary) = boundary else {
            // No retained rows: there is no older boundary to fetch behind.
            // The target is raised; the live acquisition and future committed
            // rows fill toward it. Nothing to stage now.
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        };
        if needed == 0 {
            // The retained set already satisfies the target (an auto-fill call
            // raced a refresh). Nothing to stage.
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        }

        {
            let state = self.histories.get_mut(&id).expect("history remains live");
            state.pending_load = Some(PendingHistoryLoad {
                prior_target_rows: prior_target,
                prior_load,
                prior_evidence,
                prior_projection_complete,
                acquired_tie_second: needs_tie.then_some(boundary.created_at.as_secs()),
                opened_handle_ids: Vec::new(),
                added_row_ids: Vec::new(),
                staged_batches: Vec::new(),
            });
        }

        // Each opened acquisition is tagged with its canonical branch and its
        // kind for the #486 supersede-close: `Some(second)` for the
        // tie-second REQ, `None` for the older-range REQ. EVERY branch gets
        // its own tie/older acquisition — the window boundary is global, but
        // the selection that must be re-asked at it is per branch.
        let mut opened: Vec<(usize, QueryHandle, Option<u64>)> = Vec::new();
        let boundary_second = boundary.created_at.as_secs();
        let mut staged: Vec<(usize, nmp_grammar::Demand, Option<u64>)> = Vec::new();
        if needs_tie {
            staged.extend(
                query
                    .tie_second_demands(boundary_second)
                    .into_iter()
                    .map(|(branch, demand)| (branch, demand, Some(boundary_second))),
            );
        }
        staged.extend(
            query
                .older_demands(boundary_second, needed)
                .into_iter()
                .map(|(branch, demand)| (branch, demand, None)),
        );
        for (branch, demand, kind) in staged {
            match self.resolver.subscribe(demand) {
                Ok((handle, _)) => opened.push((branch, handle, kind)),
                Err(error) => {
                    for (_, handle, _) in opened {
                        let _ = self.resolver.unsubscribe(handle.id());
                    }
                    self.degrade_store(error, &mut effects);
                    effects.extend(self.on_rollback_history_load(id));
                    effects.push(Effect::HistoryLoadResult(
                        id,
                        Err(HistoryAdvanceError::StoreUnavailable),
                    ));
                    return effects;
                }
            }
        }

        {
            let state = self
                .histories
                .get_mut(&id)
                .expect("history remains live during synchronous advance");
            if needs_tie {
                state.acquired_tie_seconds.insert(boundary_second);
            }
            for (branch, handle, kind) in opened {
                let handle_id = handle.id();
                state.handle_ids.insert(handle_id);
                state.handles.push(handle);
                state.acquisitions.insert(handle_id, kind);
                state.branch_of.insert(handle_id, branch);
                self.history_by_handle.insert(handle_id, id);
                state
                    .pending_load
                    .as_mut()
                    .expect("load was staged before opening resolver handles")
                    .opened_handle_ids
                    .push(handle_id);
            }
        }

        // Build the prospective plan without touching live router,
        // attribution, diagnostics, other projections, or delivery.
        let shadow_plans = self.history_shadow_plans(id);
        let requesting = self.history_batch(id, Vec::new(), WindowLoad::Requesting);
        let added = match self.advance_history_projection(id, boundary, old_len, &shadow_plans) {
            Ok((batch, added)) => {
                let added_row_ids = batch
                    .deltas
                    .iter()
                    .filter_map(|delta| match delta {
                        RowDelta::Added(row) => Some(row.event.id),
                        RowDelta::SourcesGrew { .. } | RowDelta::Removed(_) => None,
                    })
                    .collect();
                let pending = self
                    .histories
                    .get_mut(&id)
                    .expect("history remains live during staged advance")
                    .pending_load
                    .as_mut()
                    .expect("load remains staged until runtime acknowledgement");
                pending.added_row_ids = added_row_ids;
                pending.staged_batches = vec![requesting, batch];
                added
            }
            Err(error) => {
                if let Some(state) = self.histories.get_mut(&id) {
                    state.projection_complete = false;
                }
                self.degrade_store(error, &mut effects);
                effects.extend(self.on_rollback_history_load(id));
                effects.push(Effect::HistoryLoadResult(
                    id,
                    Err(HistoryAdvanceError::StoreUnavailable),
                ));
                return effects;
            }
        };
        debug_assert!(added <= needed);
        effects.push(Effect::HistoryLoadResult(id, Ok(())));
        effects
    }

    /// Stage a single `AtBound { max }` frame beat: the window is already at
    /// its declared ceiling, so `request_rows` cannot grow it, but the caller
    /// still gets one delivered fact. It rides the same staged commit path as
    /// a real advance (no opened handles and no target change) so it conflates
    /// identically and rolls back cleanly if the runtime never accepts it.
    pub(super) fn stage_history_atbound(
        &mut self,
        id: HistorySessionId,
        max: usize,
    ) -> Vec<Effect> {
        let (prior_target, prior_load, prior_evidence, prior_projection_complete) = {
            let state = self.histories.get(&id).expect("history remains live");
            (
                state.target_rows,
                state.load,
                state.last_evidence.clone(),
                state.projection_complete,
            )
        };
        let batch = self.history_batch(id, Vec::new(), WindowLoad::AtBound { max });
        let state = self.histories.get_mut(&id).expect("history remains live");
        state.pending_load = Some(PendingHistoryLoad {
            prior_target_rows: prior_target,
            prior_load,
            prior_evidence,
            prior_projection_complete,
            acquired_tie_second: None,
            opened_handle_ids: Vec::new(),
            added_row_ids: Vec::new(),
            staged_batches: vec![batch],
        });
        vec![Effect::HistoryLoadResult(id, Ok(()))]
    }

    pub(super) fn on_commit_history_load(&mut self, id: HistorySessionId) -> Vec<Effect> {
        if !self
            .histories
            .get(&id)
            .is_some_and(|state| state.pending_load.is_some())
        {
            return Vec::new();
        }

        // #486: retire the historical tie/older acquisitions the session no
        // longer needs, so a deep scroll of K advances never accumulates O(K)
        // live relay subscriptions. Three classes of handle are KEPT open:
        //   * the permanent live-top demand (`live_handle_id`);
        //   * the advance now committing (its own just-opened handles); and
        //   * the tie-second REQ for the CURRENT window boundary second — a
        //     dense same-second boundary keeps that second as the boundary
        //     across several advances (its `needs_tie` gate stays satisfied
        //     without re-opening), and closing its REQ before the boundary has
        //     descended below it could drop a not-yet-projected same-second
        //     row (the #474 tie-second correctness class). It is retired only
        //     once the boundary moves strictly older, at which point every
        //     in-store row at that second is already projected as interior.
        // Every OTHER acquisition — older-range REQs (always re-requestable, so
        // never a permanent gap) and tie REQs for seconds no longer the
        // boundary — is retired here. `acquired_tie_seconds` is deliberately
        // retained (that is the coverage evidence) so a later advance never
        // re-requests a tie second already covered. The recompile just below
        // re-diffs the demand and emits the wire CLOSEs for the dropped handles.
        let superseded: Vec<HandleId> = {
            let state = self
                .histories
                .get(&id)
                .expect("committed history remained live");
            let current: BTreeSet<HandleId> = state
                .pending_load
                .as_ref()
                .expect("commit checked the staged history load")
                .opened_handle_ids
                .iter()
                .copied()
                .collect();
            let live: BTreeSet<HandleId> = state.live_handle_ids.iter().copied().collect();
            let boundary_second = self
                .window_boundary(id)
                .map(|cursor| cursor.created_at.as_secs());
            let state = self
                .histories
                .get(&id)
                .expect("committed history remained live");
            state
                .acquisitions
                .iter()
                .filter(|(handle, kind)| {
                    if live.contains(handle) || current.contains(handle) {
                        return false;
                    }
                    // Keep the tie REQ whose second is still the boundary.
                    !matches!((kind, boundary_second), (Some(second), Some(b)) if *second == b)
                })
                .map(|(handle, _)| *handle)
                .collect()
        };
        if !superseded.is_empty() {
            for handle_id in &superseded {
                self.history_by_handle.remove(handle_id);
                let _ = self.resolver.unsubscribe(*handle_id);
            }
            let state = self
                .histories
                .get_mut(&id)
                .expect("committed history remained live");
            state
                .handles
                .retain(|handle| !superseded.contains(&handle.id()));
            for handle_id in &superseded {
                state.handle_ids.remove(handle_id);
                state.acquisitions.remove(handle_id);
                state.branch_of.remove(handle_id);
            }
        }

        let mut effects = Vec::new();
        self.withdraw_wire_demand(&mut effects);

        let (made_progress, target, len, has_boundary) = {
            let state = self
                .histories
                .get_mut(&id)
                .expect("committed history remained live");
            let pending = state
                .pending_load
                .take()
                .expect("commit checked the staged history load");
            let made_progress = !pending.added_row_ids.is_empty();
            for batch in pending.staged_batches {
                effects.push(Effect::EmitHistory(id, batch));
            }
            (
                made_progress,
                state.target_rows,
                state.last_rows.len(),
                !state.order.is_empty(),
            )
        };

        // Continuation loop (#485): the committed advance made progress but
        // the target is still unmet and an older boundary remains. Stage the
        // next advance automatically, one at a time — the runtime's commit
        // loop drives this to convergence. The `made_progress` guard makes the
        // loop bounded: an advance that adds no canonical row (store exhausted
        // locally; the older-range wire request already placed) does not
        // re-stage, so it never spins waiting on the network.
        if made_progress && target > len && has_boundary {
            effects.extend(self.stage_history_advance(id, target));
        }
        effects
    }

    pub(super) fn on_rollback_history_load(&mut self, id: HistorySessionId) -> Vec<Effect> {
        let Some(pending) = self
            .histories
            .get_mut(&id)
            .and_then(|state| state.pending_load.take())
        else {
            return Vec::new();
        };

        let opened: BTreeSet<_> = pending.opened_handle_ids.iter().copied().collect();
        for handle_id in &opened {
            self.history_by_handle.remove(handle_id);
            let _ = self.resolver.unsubscribe(*handle_id);
        }
        let state = self
            .histories
            .get_mut(&id)
            .expect("rollback target remained live while staged handles closed");
        state
            .handles
            .retain(|handle| !opened.contains(&handle.id()));
        state.handle_ids.retain(|handle| !opened.contains(handle));
        state
            .acquisitions
            .retain(|handle, _| !opened.contains(handle));
        state.branch_of.retain(|handle, _| !opened.contains(handle));
        if let Some(second) = pending.acquired_tie_second {
            state.acquired_tie_seconds.remove(&second);
        }
        for event_id in pending.added_row_ids {
            if let Some(row) = state.last_rows.remove(&event_id) {
                state
                    .order
                    .remove(&(Reverse(row.event.created_at.as_secs()), event_id));
            }
        }
        state.target_rows = pending.prior_target_rows;
        state.load = pending.prior_load;
        state.last_evidence = pending.prior_evidence;
        state.projection_complete = pending.prior_projection_complete;

        Vec::new()
    }

    /// Compile the resolver's current (possibly staged-history) demand into
    /// an isolated plan. A history advance changes only the outer time
    /// window of an already-live descriptor, so every discovery dependency
    /// is already represented by the initial session; shadow planning never
    /// needs to mutate the widen-only discovery subscription.
    pub(super) fn history_shadow_plans(&self, id: HistorySessionId) -> Vec<RelayPlan> {
        let Some(state) = self.histories.get(&id) else {
            return Vec::new();
        };
        let needs_live = state.acquisitions_by_branch.iter().any(|acquisition| {
            !matches!(
                acquisition.root(),
                Some(ScopeAcquisition::CoverageSatisfied(_)) | Some(ScopeAcquisition::CacheOnly(_))
            )
        });
        let live = needs_live.then(|| self.shadow_plan_for(self.wire_demand()));
        state
            .acquisitions_by_branch
            .iter()
            .map(|acquisition| match acquisition.root() {
                Some(ScopeAcquisition::CoverageSatisfied(plan))
                | Some(ScopeAcquisition::CacheOnly(plan)) => plan.clone(),
                _ => live
                    .clone()
                    .expect("a branch that contributes wire work computed the live shadow plan"),
            })
            .collect()
    }

    /// Every resolver handle this session holds open, grouped by canonical
    /// branch: branch `i`'s live-top acquisition plus whatever tie-second and
    /// older-range acquisitions are currently open for that same branch.
    fn history_handles_by_branch(&self, id: HistorySessionId) -> Vec<Vec<HandleId>> {
        let Some(state) = self.histories.get(&id) else {
            return Vec::new();
        };
        let mut grouped = vec![Vec::new(); state.live_handle_ids.len()];
        for (handle, branch) in &state.branch_of {
            if let Some(slot) = grouped.get_mut(*branch) {
                slot.push(*handle);
            }
        }
        grouped
    }

    pub(super) fn refresh_all_histories(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<_> = self.histories.keys().copied().collect();
        for id in ids {
            self.refresh_history(id, WindowLoad::Idle, effects);
        }
    }

    /// Refresh only acquisition evidence after a coverage-only mutation.
    /// The current bounded rows remain authoritative unless a prior store
    /// failure marked the projection incomplete, in which case the full
    /// refresh oracle repairs it before evidence is emitted.
    pub(super) fn refresh_all_history_evidence(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<_> = self.histories.keys().copied().collect();
        for id in ids {
            self.refresh_history_evidence(id, effects);
        }
    }

    pub(super) fn history_batch(
        &mut self,
        id: HistorySessionId,
        deltas: Vec<RowDelta>,
        load: WindowLoad,
    ) -> HistoryBatch {
        let state = self
            .histories
            .get_mut(&id)
            .expect("history batch requires a live session");
        state.load = load;
        let rows = state
            .order
            .iter()
            .filter_map(|(_, event_id)| state.last_rows.get(event_id).cloned())
            .collect();
        HistoryBatch {
            rows,
            deltas,
            evidence: state.last_evidence.clone().unwrap_or_default(),
            load,
        }
    }

    pub(super) fn refresh_history(
        &mut self,
        id: HistorySessionId,
        load: WindowLoad,
        effects: &mut Vec<Effect>,
    ) -> Option<usize> {
        let (current, evidence) = match self.history_rows_and_evidence_for(id) {
            Ok(value) => value,
            Err(error) => {
                if let Some(state) = self.histories.get_mut(&id) {
                    state.projection_complete = false;
                }
                self.degrade_store(error, effects);
                return None;
            }
        };
        let len = current.len();
        if let Some(batch) = self.apply_history_projection(id, current, evidence, load) {
            effects.push(Effect::EmitHistory(id, batch));
        }
        Some(len)
    }

    fn apply_history_projection(
        &mut self,
        id: HistorySessionId,
        current: BTreeMap<EventId, Row>,
        evidence: Vec<AcquisitionEvidence>,
        load: WindowLoad,
    ) -> Option<HistoryBatch> {
        let state = self.histories.get_mut(&id)?;
        let current_rows = current.clone();
        let current_order = current_rows
            .iter()
            .map(|(event_id, row)| (Reverse(row.event.created_at.as_secs()), *event_id))
            .collect();
        let mut deltas = Vec::new();
        for (event_id, row) in current {
            match state.last_rows.get(&event_id) {
                None => deltas.push(RowDelta::Added(row)),
                Some(previous) if previous.sources != row.sources => {
                    deltas.push(RowDelta::SourcesGrew {
                        id: event_id,
                        sources: row.sources,
                    });
                }
                Some(_) => {}
            }
        }
        for event_id in state.last_rows.keys() {
            if !current_rows.contains_key(event_id) {
                deltas.push(RowDelta::Removed(*event_id));
            }
        }
        let changed = !deltas.is_empty()
            || state.last_evidence.as_ref() != Some(&evidence)
            || state.load != load;
        state.last_rows = current_rows;
        state.order = current_order;
        state.last_evidence = Some(evidence);
        state.projection_complete = true;
        if changed {
            Some(self.history_batch(id, deltas, load))
        } else {
            None
        }
    }

    pub(super) fn refresh_history_evidence(
        &mut self,
        id: HistorySessionId,
        effects: &mut Vec<Effect>,
    ) {
        let Some(state) = self.histories.get(&id) else {
            return;
        };
        if !state.projection_complete {
            self.refresh_history(id, WindowLoad::Idle, effects);
            return;
        }

        // Same rule as `refresh_observation_evidence` (#122/#763): a coverage
        // read that could not answer leaves the last delivered evidence in
        // place and degrades, instead of republishing it as unproven.
        let evidence = match self.history_evidence_for(id) {
            Ok(evidence) => evidence,
            Err(error) => {
                self.degrade_store(error, effects);
                return;
            }
        };
        let Some(state) = self.histories.get_mut(&id) else {
            return;
        };
        if state.last_evidence.as_ref() == Some(&evidence) && state.load == WindowLoad::Idle {
            return;
        }
        state.last_evidence = Some(evidence);
        let batch = self.history_batch(id, Vec::new(), WindowLoad::Idle);
        effects.push(Effect::EmitHistory(id, batch));
    }

    /// One acquisition-evidence entry per canonical branch, in branch order.
    /// A branch's evidence is computed only from ITS OWN handles and its own
    /// opening-time policy decision; no branch's proof can stand in for
    /// another's, and nothing is rolled up into a window-global verdict.
    fn history_evidence_for(
        &self,
        id: HistorySessionId,
    ) -> Result<Vec<AcquisitionEvidence>, PersistenceError> {
        let Some(state) = self.histories.get(&id) else {
            return Ok(Vec::new());
        };
        let by_branch = self.history_handles_by_branch(id);
        let mut evidence = Vec::with_capacity(by_branch.len());
        for (branch, handles) in by_branch.into_iter().enumerate() {
            evidence.push(self.acquisition_evidence_for_scopes(
                self.history_branch_demand_scopes(&handles),
                &state.acquisitions_by_branch[branch],
            )?);
        }
        Ok(evidence)
    }

    pub(super) fn history_rows_and_evidence_for(
        &self,
        id: HistorySessionId,
    ) -> Result<(BTreeMap<EventId, Row>, Vec<AcquisitionEvidence>), PersistenceError> {
        let rows = self.history_rows_for(id)?;
        let evidence = self.history_evidence_for(id)?;
        Ok((rows, evidence))
    }

    pub(super) fn history_rows_for(
        &self,
        id: HistorySessionId,
    ) -> Result<BTreeMap<EventId, Row>, PersistenceError> {
        let state = self
            .histories
            .get(&id)
            .expect("history projection requires a live session");
        let mut by_id: BTreeMap<EventId, Row> = BTreeMap::new();
        for (branch, live) in state.live_handle_ids.iter().enumerate() {
            let declaration = &state.query.live_query().branches()[branch];
            let pinned_relays = match (declaration.cache, &declaration.source) {
                (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays),
                _ => None,
            };
            for mut atom in self.resolver.root_atoms(*live) {
                atom.limit = None;
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.history_store_queries
                    .set(self.history_store_queries.get().saturating_add(1));
                let filter = atom.to_nostr();
                // Taking the window target from EVERY branch is exact: a row
                // outside one branch's newest `target_rows` already has that
                // many newer witnesses in that same branch, so it can never
                // belong to the global newest `target_rows` either.
                let rows = match pinned_relays {
                    Some(relays) => self.resolver.store().query_newest_under_pin(
                        &filter,
                        relays,
                        state.target_rows,
                    )?,
                    None => self
                        .resolver
                        .store()
                        .query_newest(&filter, state.target_rows)?,
                };
                #[cfg(test)]
                self.history_rows_examined.set(
                    self.history_rows_examined
                        .get()
                        .saturating_add(rows.len() as u64),
                );
                for stored in rows {
                    let sources: BTreeSet<RelayUrl> = stored.provenance.seen.into_keys().collect();
                    match by_id.entry(stored.event.id) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(Row {
                                event: stored.event,
                                sources,
                            });
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            entry.get_mut().sources.extend(sources);
                        }
                    }
                }
            }
        }
        // The window bound applies ONCE to the merged union, in canonical
        // newest-first order — never `target_rows` per branch.
        if by_id.len() > state.target_rows {
            let mut ordered: Vec<_> = by_id
                .iter()
                .map(|(event_id, row)| (row.event.created_at.as_secs(), *event_id))
                .collect();
            ordered.sort_by(|a, b| nip01_newest_first((a.0, &a.1), (b.0, &b.1)));
            let keep: BTreeSet<_> = ordered
                .into_iter()
                .take(state.target_rows)
                .map(|(_, event_id)| event_id)
                .collect();
            by_id.retain(|event_id, _| keep.contains(event_id));
        }
        Ok(by_id)
    }

    /// Union ONE branch's active history partitions by structural Demand
    /// boundary. A branch's history handles are time-window variants of that
    /// branch's one descriptor, so their graph shape and opening-time policy
    /// vector are identical while their current root atoms differ. Handles
    /// from a DIFFERENT branch are never combined here — that is exactly the
    /// cross-branch contamination this issue exists to prevent.
    fn history_branch_demand_scopes(
        &self,
        handles: &[HandleId],
    ) -> Vec<(BTreeSet<ContextualAtom>, Freshness)> {
        let mut combined: Vec<(BTreeSet<ContextualAtom>, Freshness)> = Vec::new();
        for handle in handles {
            for (index, (atoms, freshness)) in
                self.resolver.demand_scopes(*handle).into_iter().enumerate()
            {
                if let Some((combined_atoms, existing_freshness)) = combined.get_mut(index) {
                    debug_assert_eq!(
                        *existing_freshness, freshness,
                        "one history descriptor keeps one policy per Demand boundary"
                    );
                    combined_atoms.extend(atoms);
                } else {
                    combined.push((atoms, freshness));
                }
            }
        }
        combined
    }

    pub(super) fn advance_history_projection(
        &mut self,
        id: HistorySessionId,
        before: nmp_store::EventCursor,
        old_len: usize,
        plans: &[RelayPlan],
    ) -> Result<(HistoryBatch, usize), PersistenceError> {
        let state = self
            .histories
            .get(&id)
            .expect("history advance requires a live session");
        let needed = state.target_rows.saturating_sub(state.last_rows.len());
        let mut candidates = BTreeMap::<EventId, Row>::new();
        for (branch, live) in state.live_handle_ids.iter().enumerate() {
            let declaration = &state.query.live_query().branches()[branch];
            let pinned_relays = match (declaration.cache, &declaration.source) {
                (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays),
                _ => None,
            };
            for mut atom in self.resolver.root_atoms(*live) {
                atom.limit = None;
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.history_store_queries
                    .set(self.history_store_queries.get().saturating_add(1));
                let filter = atom.to_nostr();
                let rows = match pinned_relays {
                    Some(relays) => self
                        .resolver
                        .store()
                        .query_newest_before_under_pin(&filter, relays, before, needed)?,
                    None => self
                        .resolver
                        .store()
                        .query_newest_before(&filter, before, needed)?,
                };
                #[cfg(test)]
                self.history_rows_examined.set(
                    self.history_rows_examined
                        .get()
                        .saturating_add(rows.len() as u64),
                );
                for stored in rows {
                    let sources: BTreeSet<RelayUrl> = stored.provenance.seen.into_keys().collect();
                    match candidates.entry(stored.event.id) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(Row {
                                event: stored.event,
                                sources,
                            });
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            entry.get_mut().sources.extend(sources);
                        }
                    }
                }
            }
        }
        // The advance chunk is global: `needed` MORE rows across the merged
        // union, never `needed` per branch.
        let mut ordered: Vec<Row> = candidates.into_values().collect();
        ordered.sort_by(|a, b| {
            nip01_newest_first(
                (a.event.created_at.as_secs(), &a.event.id),
                (b.event.created_at.as_secs(), &b.event.id),
            )
        });
        ordered.truncate(needed);
        let auth_status = self.auth_status_map();
        let finished_stored_events = self.finished_stored_events();
        let by_branch = self.history_handles_by_branch(id);
        let mut evidence: Vec<AcquisitionEvidence> = Vec::with_capacity(by_branch.len());
        for (branch, handles) in by_branch.into_iter().enumerate() {
            let subtree_atoms: BTreeSet<ContextualAtom> = handles
                .iter()
                .flat_map(|handle| self.resolver.subtree_atoms(*handle))
                .collect();
            // `?`: this whole advance is already all-or-nothing on a store
            // failure, and a coverage read is no different from the row
            // reads above it (#763).
            evidence.push(evidence::acquisition_evidence(
                &subtree_atoms,
                plans.get(branch).unwrap_or(&RelayPlan::default()),
                self.resolver.store(),
                &self.connected_relays,
                &auth_status,
                &self.ever_connected_relays,
                &finished_stored_events,
            )?);
        }

        let state = self
            .histories
            .get_mut(&id)
            .expect("history remains live during synchronous projection");
        let mut deltas = Vec::with_capacity(ordered.len());
        for row in ordered {
            let event_id = row.event.id;
            state.last_rows.insert(event_id, row.clone());
            state
                .order
                .insert((Reverse(row.event.created_at.as_secs()), event_id));
            deltas.push(RowDelta::Added(row));
        }
        state.last_evidence = Some(evidence);
        state.projection_complete = true;
        let added = state.last_rows.len().saturating_sub(old_len);
        let batch = self.history_batch(id, deltas, WindowLoad::Returned { added });
        Ok((batch, added))
    }

    /// Apply one committed store batch to any stable bounded history window,
    /// including Strict, derived, and multi-root selections. Only touched
    /// rows plus the exact newly exposed lower segment are visited: the
    /// canonical order index identifies eviction/backfill boundaries without
    /// sorting or replaying the retained window.
    pub(super) fn try_apply_committed_history_row_changes(
        &mut self,
        id: HistorySessionId,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) -> bool {
        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        let Some(state) = self.histories.get(&id) else {
            return true;
        };
        // The incremental algebra below is proven for a single-branch
        // window. A composed window must re-derive the union and its global
        // bound across every branch, so it keeps the full-refresh oracle
        // until its own algebra is proven independently.
        if state.live_handle_ids.len() != 1 {
            return false;
        }
        let Some(primary) = state.live_handle_ids.first().copied() else {
            return false;
        };
        let root_atoms = self.resolver.root_atoms(primary);
        if state.last_evidence.is_none()
            || !state.projection_complete
            || state.pending_load.is_some()
        {
            return false;
        }
        if root_atoms.is_empty() {
            return state.last_rows.is_empty();
        }
        let filters: Vec<_> = root_atoms
            .into_iter()
            .map(|mut atom| {
                atom.limit = None;
                atom.to_nostr()
            })
            .collect();
        let matches = |event: &nostr::Event| {
            filters
                .iter()
                .any(|filter| filter.match_event(event, MatchEventOptions::new()))
        };
        let declaration = &state.query.live_query().branches()[0];
        let pinned_relays = match (declaration.cache, &declaration.source) {
            (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays.clone()),
            _ => None,
        };
        // The one rule, `nmp_store::visible_under_pin`, over the committed
        // row's two projected facts: whether this node accepted the write
        // itself, and which relays carried it
        // (`CommittedCurrentRow::observed_relays` IS `Provenance::seen`'s
        // keys). Our own row is shown under every pin whatever any host has
        // since done with it; another host's row never leaks across one.
        let visible_under_pin = |row: &CommittedCurrentRow| {
            pinned_relays.as_ref().is_none_or(|pinned| {
                nmp_store::visible_under_pin(row.locally_accepted, &row.observed_relays, pinned)
            })
        };
        let target_rows = state.target_rows;
        let original_boundary =
            state
                .order
                .iter()
                .next_back()
                .map(|(Reverse(created_at), event_id)| {
                    nmp_store::EventCursor::new(Timestamp::from(*created_at), *event_id)
                });
        let mut before = BTreeMap::<EventId, Option<Row>>::new();
        let mut visible_removals = 0usize;
        let mut strict_promotions = BTreeMap::<EventId, Row>::new();
        if pinned_relays.is_some() {
            for changed in &changes.provenance_grew {
                if !matches(&changed.event)
                    || !visible_under_pin(changed)
                    || state.last_rows.contains_key(&changed.event.id)
                {
                    continue;
                }
                #[cfg(test)]
                self.history_affected_row_queries
                    .set(self.history_affected_row_queries.get().saturating_add(1));
                let current = match self
                    .resolver
                    .store()
                    .query(&nostr::Filter::new().id(changed.event.id))
                {
                    Ok(mut rows) => rows.pop().map(|stored| Row {
                        event: stored.event,
                        sources: stored.provenance.seen.into_keys().collect(),
                    }),
                    Err(error) => {
                        self.histories
                            .get_mut(&id)
                            .expect("history remained live after affected-row read failure")
                            .projection_complete = false;
                        self.degrade_store(error, effects);
                        return true;
                    }
                };
                strict_promotions.insert(
                    changed.event.id,
                    current.unwrap_or_else(|| Row {
                        event: {
                            #[cfg(feature = "bench-instrumentation")]
                            crate::ingest_attribution::projection_event_clone();
                            changed.event.clone()
                        },
                        sources: changed.observed_relays.clone(),
                    }),
                );
            }
        }

        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::history_projection_setup(phase_started.elapsed());
        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();

        {
            let state = self
                .histories
                .get_mut(&id)
                .expect("history remained live during committed mutation");
            let remember =
                |event_id: EventId,
                 state: &HistoryState,
                 before: &mut BTreeMap<EventId, Option<Row>>| {
                    before
                        .entry(event_id)
                        .or_insert_with(|| state.last_rows.get(&event_id).cloned());
                };

            for event in &changes.removed {
                if !state.last_rows.contains_key(&event.id) {
                    continue;
                }
                remember(event.id, state, &mut before);
                if let Some(row) = state.last_rows.remove(&event.id) {
                    state
                        .order
                        .remove(&(Reverse(row.event.created_at.as_secs()), event.id));
                    visible_removals = visible_removals.saturating_add(1);
                }
            }
            for row in &changes.inserted {
                if !matches(&row.event) || !visible_under_pin(row) {
                    continue;
                }
                let event_id = row.event.id;
                remember(event_id, state, &mut before);
                if let Some(previous) = state.last_rows.remove(&event_id) {
                    state
                        .order
                        .remove(&(Reverse(previous.event.created_at.as_secs()), event_id));
                }
                let remembered = Row {
                    event: {
                        #[cfg(feature = "bench-instrumentation")]
                        crate::ingest_attribution::projection_event_clone();
                        row.event.clone()
                    },
                    sources: row.observed_relays.clone(),
                };
                state
                    .order
                    .insert((Reverse(remembered.event.created_at.as_secs()), event_id));
                state.last_rows.insert(event_id, remembered);
            }
            for row in &changes.provenance_grew {
                if !matches(&row.event) {
                    continue;
                }
                if state.last_rows.contains_key(&row.event.id) {
                    remember(row.event.id, state, &mut before);
                    state
                        .last_rows
                        .get_mut(&row.event.id)
                        .expect("provenance target was checked above")
                        .sources
                        .extend(row.observed_relays.iter().cloned());
                } else if pinned_relays.is_some() && visible_under_pin(row) {
                    // An event already cached from an unpinned relay can
                    // enter a Strict projection when this committed duplicate
                    // is its first observation by a pinned one. Treat that
                    // transition as an affected-row insertion, then let the
                    // same bounded order rebalance decide whether it belongs
                    // in top-N.
                    remember(row.event.id, state, &mut before);
                    let projected = strict_promotions
                        .remove(&row.event.id)
                        .expect("a Strict promotion visible under the pin was prefetched");
                    state.order.insert((
                        Reverse(projected.event.created_at.as_secs()),
                        projected.event.id,
                    ));
                    state.last_rows.insert(projected.event.id, projected);
                }
            }
        }

        // Any visible removal can expose a better row below the PRE-mutation
        // boundary, even when a simultaneous older insertion/restoration has
        // already brought the working set back to `target_rows`. Reconcile
        // exactly once, merge that bounded tail with every committed affected
        // row above, and only then truncate canonically.
        if visible_removals > 0 {
            let boundary =
                original_boundary.expect("a visible removal implies a prior canonical boundary");
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.history_store_queries
                .set(self.history_store_queries.get().saturating_add(1));
            let queried = match pinned_relays.as_ref() {
                Some(relays) => self.resolver.store().query_newest_before_any_under_pin(
                    &filters,
                    relays,
                    boundary,
                    visible_removals,
                ),
                None => self.resolver.store().query_newest_before_any(
                    &filters,
                    boundary,
                    visible_removals,
                ),
            };
            let rows = match queried {
                Ok(rows) => rows,
                Err(error) => {
                    let state = self
                        .histories
                        .get_mut(&id)
                        .expect("history remained live after failed backfill");
                    for (event_id, prior) in before {
                        if let Some(current) = state.last_rows.remove(&event_id) {
                            state
                                .order
                                .remove(&(Reverse(current.event.created_at.as_secs()), event_id));
                        }
                        if let Some(prior) = prior {
                            state
                                .order
                                .insert((Reverse(prior.event.created_at.as_secs()), event_id));
                            state.last_rows.insert(event_id, prior);
                        }
                    }
                    state.projection_complete = false;
                    self.degrade_store(error, effects);
                    return true;
                }
            };
            #[cfg(test)]
            self.history_rows_examined.set(
                self.history_rows_examined
                    .get()
                    .saturating_add(rows.len() as u64),
            );
            let state = self
                .histories
                .get_mut(&id)
                .expect("history remained live during exact backfill");
            for stored in rows {
                let event_id = stored.event.id;
                if state.last_rows.contains_key(&event_id) {
                    continue;
                }
                before
                    .entry(event_id)
                    .or_insert_with(|| state.last_rows.get(&event_id).cloned());
                let sources: BTreeSet<_> = stored.provenance.seen.into_keys().collect();
                let row = Row {
                    event: stored.event,
                    sources: sources.clone(),
                };
                let remembered = row.clone();
                state
                    .order
                    .insert((Reverse(remembered.event.created_at.as_secs()), event_id));
                state.last_rows.insert(event_id, remembered);
            }
        }

        {
            let state = self
                .histories
                .get_mut(&id)
                .expect("history remained live during canonical truncation");
            let remember =
                |event_id: EventId,
                 state: &HistoryState,
                 before: &mut BTreeMap<EventId, Option<Row>>| {
                    before
                        .entry(event_id)
                        .or_insert_with(|| state.last_rows.get(&event_id).cloned());
                };
            while state.last_rows.len() > target_rows {
                let Some((_, event_id)) = state.order.iter().next_back().copied() else {
                    break;
                };
                remember(event_id, state, &mut before);
                let row = state
                    .last_rows
                    .remove(&event_id)
                    .expect("history order and membership stay identical");
                state
                    .order
                    .remove(&(Reverse(row.event.created_at.as_secs()), event_id));
            }
        }

        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::history_projection_apply(phase_started.elapsed());
        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();

        let state = self
            .histories
            .get(&id)
            .expect("history remained live after committed rebalance");
        let mut deltas = Vec::new();
        for (event_id, prior) in &before {
            match (prior, state.last_rows.get(event_id)) {
                (None, Some(current)) => deltas.push(RowDelta::Added(current.clone())),
                (Some(_), None) => deltas.push(RowDelta::Removed(*event_id)),
                (Some(prior), Some(current)) if prior.sources != current.sources => {
                    deltas.push(RowDelta::SourcesGrew {
                        id: *event_id,
                        sources: current.sources.clone(),
                    });
                }
                (None, None) | (Some(_), Some(_)) => {}
            }
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::history_projection_delta(phase_started.elapsed());
        if deltas.is_empty() {
            return true;
        }
        #[cfg(feature = "bench-instrumentation")]
        let batch_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let delta_count = deltas.len();
        let batch = self.history_batch(id, deltas, WindowLoad::Idle);
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::history_projection_batch(
            batch_started.elapsed(),
            delta_count,
            batch.rows.len(),
        );
        effects.push(Effect::EmitHistory(id, batch));
        true
    }
}
