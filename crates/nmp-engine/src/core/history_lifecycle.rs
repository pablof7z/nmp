//! Expandable observation-window lifecycle and projection.
//!
//! This module owns staged window growth, commit/rollback, bounded history
//! reconciliation, and mutation projection for active history sessions.

use super::*;

impl<S: EventStore> EngineCore<S> {
    pub(super) fn on_subscribe_history(
        &mut self,
        query: HistoryQuery,
        sink: Box<dyn HistorySink>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let (handle, _) = match self.resolver.subscribe(query.initial_demand()) {
            Ok(value) => value,
            Err(error) => {
                self.degrade_store(error, &mut effects);
                return effects;
            }
        };
        let handle_id = handle.id();
        let acquisition = self.decide_handle_acquisition(handle_id, handle.freshness());
        let id = HistorySessionId(self.next_history_id);
        self.next_history_id = self.next_history_id.wrapping_add(1).max(1);
        self.history_by_handle.insert(handle_id, id);
        self.histories.insert(
            id,
            HistoryState {
                target_rows: query.page_size(),
                query,
                acquisition,
                handles: vec![handle],
                handle_ids: BTreeSet::from([handle_id]),
                live_handle_id: handle_id,
                acquisitions: BTreeMap::new(),
                sink,
                acquired_tie_seconds: BTreeSet::new(),
                last_rows: BTreeMap::new(),
                order: BTreeSet::new(),
                last_evidence: None,
                projection_complete: false,
                load: WindowLoad::Idle,
                pending_load: None,
            },
        );

        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories_except(id, &mut effects);
        self.refresh_history(id, WindowLoad::Idle, &mut effects);
        effects
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
        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
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

        // Each opened acquisition is tagged with its kind for the #486
        // supersede-close: `Some(second)` for the tie-second REQ, `None` for
        // the older-range REQ.
        let mut opened: Vec<(QueryHandle, Option<u64>)> = Vec::new();
        let boundary_second = boundary.created_at.as_secs();
        if needs_tie {
            if let Some(tie) = query.tie_second_demand(boundary_second) {
                match self.resolver.subscribe(tie) {
                    Ok((handle, _)) => opened.push((handle, Some(boundary_second))),
                    Err(error) => {
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
        }
        if let Some(older) = query.older_demand(boundary_second, needed) {
            match self.resolver.subscribe(older) {
                Ok((handle, _)) => opened.push((handle, None)),
                Err(error) => {
                    for (handle, _) in opened {
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
            for (handle, kind) in opened {
                let handle_id = handle.id();
                state.handle_ids.insert(handle_id);
                state.handles.push(handle);
                state.acquisitions.insert(handle_id, kind);
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
        // attribution, diagnostics, other projections, or any sink.
        let shadow_plan = self.history_shadow_plan(id);
        let requesting = self.history_batch(id, Vec::new(), WindowLoad::Requesting);
        let added = match self.advance_history_projection(id, boundary, old_len, &shadow_plan) {
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
        let preflight_relays = self
            .histories
            .get(&id)
            .filter(|state| state.acquisition.contributes_wire())
            .map(|_| shadow_plan.reqs.keys().cloned().collect())
            .unwrap_or_default();
        effects.push(Effect::PreflightHistoryRelays(preflight_relays));
        effects.push(Effect::HistoryLoadResult(id, Ok(())));
        effects
    }

    /// Stage a single `AtBound { max }` frame beat: the window is already at
    /// its declared ceiling, so `request_rows` cannot grow it, but the caller
    /// still gets one delivered fact. It rides the same staged commit path as
    /// a real advance (empty relay preflight, no opened handles, no target
    /// change) so it conflates identically and rolls back cleanly if the
    /// runtime never accepts it.
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
        vec![
            Effect::PreflightHistoryRelays(BTreeSet::new()),
            Effect::HistoryLoadResult(id, Ok(())),
        ]
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
            let live = state.live_handle_id;
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
                    if **handle == live || current.contains(handle) {
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
            }
        }

        let mut effects = Vec::new();
        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories_except(id, &mut effects);

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
                state.sink.on_history(batch.clone());
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
    pub(super) fn history_shadow_plan(&self, id: HistorySessionId) -> RelayPlan {
        match self.histories.get(&id).map(|state| &state.acquisition) {
            Some(HandleAcquisition::CoverageSatisfied(plan)) => plan.clone(),
            Some(HandleAcquisition::CacheOnly(plan)) => plan.clone(),
            Some(HandleAcquisition::Live) | None => self.shadow_plan_for(self.wire_demand()),
        }
    }

    pub(super) fn refresh_all_histories(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<_> = self.histories.keys().copied().collect();
        for id in ids {
            self.refresh_history(id, WindowLoad::Idle, effects);
        }
    }

    pub(super) fn refresh_all_histories_except(
        &mut self,
        except: HistorySessionId,
        effects: &mut Vec<Effect>,
    ) {
        let ids: Vec<_> = self
            .histories
            .keys()
            .copied()
            .filter(|id| *id != except)
            .collect();
        for id in ids {
            self.refresh_history(id, WindowLoad::Idle, effects);
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
        let len = state.last_rows.len();
        if changed {
            let batch = self.history_batch(id, deltas, load);
            if let Some(state) = self.histories.get(&id) {
                state.sink.on_history(batch.clone());
            }
            effects.push(Effect::EmitHistory(id, batch));
        }
        Some(len)
    }

    pub(super) fn history_rows_and_evidence_for(
        &self,
        id: HistorySessionId,
    ) -> Result<(BTreeMap<EventId, Row>, AcquisitionEvidence), PersistenceError> {
        let state = self
            .histories
            .get(&id)
            .expect("history projection requires a live session");
        let primary = *state
            .handle_ids
            .first()
            .expect("history session always owns its initial resolver handle");
        let root_atoms = self.resolver.root_atoms(primary);
        let subtree_atoms = self.history_subtree_atoms(id);
        let pinned_relays = match (
            state.query.live_query().0.cache,
            &state.query.live_query().0.source,
        ) {
            (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays),
            _ => None,
        };
        let mut by_id = BTreeMap::new();
        for mut atom in root_atoms {
            atom.limit = None;
            #[cfg(test)]
            self.history_store_queries
                .set(self.history_store_queries.get().saturating_add(1));
            let filter = atom.to_nostr();
            let rows = match pinned_relays {
                Some(relays) => self.resolver.store().query_newest_observed_by(
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
                by_id.entry(stored.event.id).or_insert_with(|| Row {
                    event: stored.event,
                    sources: stored.provenance.seen.into_keys().collect(),
                });
            }
        }
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
        let auth_status = self.auth_status_map();
        let evidence_plan = state
            .acquisition
            .evidence_plan()
            .unwrap_or_else(|| self.router.plan());
        let evidence = evidence::acquisition_evidence(
            &subtree_atoms,
            evidence_plan,
            self.resolver.store(),
            &self.connected_relays,
            &auth_status,
            &self.ever_connected_relays,
        );
        Ok((by_id, evidence))
    }

    /// Every active acquisition atom owned by one coordinated history
    /// partition: initial bounded root, exact unbounded tie seconds, bounded
    /// older ranges, and every interior Derived dependency. Set union keeps
    /// shared atoms deduplicated while preserving distinct scoped windows.
    pub(super) fn history_subtree_atoms(&self, id: HistorySessionId) -> BTreeSet<ContextualAtom> {
        self.histories
            .get(&id)
            .into_iter()
            .flat_map(|state| state.handle_ids.iter().copied())
            .flat_map(|handle| self.resolver.subtree_atoms(handle))
            .collect()
    }

    pub(super) fn advance_history_projection(
        &mut self,
        id: HistorySessionId,
        before: nmp_store::EventCursor,
        old_len: usize,
        plan: &RelayPlan,
    ) -> Result<(HistoryBatch, usize), PersistenceError> {
        let state = self
            .histories
            .get(&id)
            .expect("history advance requires a live session");
        let primary = *state
            .handle_ids
            .first()
            .expect("history session always owns its initial resolver handle");
        let root_atoms = self.resolver.root_atoms(primary);
        let subtree_atoms = self.history_subtree_atoms(id);
        let needed = state.target_rows.saturating_sub(state.last_rows.len());
        let pinned_relays = match (
            state.query.live_query().0.cache,
            &state.query.live_query().0.source,
        ) {
            (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays),
            _ => None,
        };
        let mut candidates = BTreeMap::<EventId, Row>::new();
        for mut atom in root_atoms {
            atom.limit = None;
            #[cfg(test)]
            self.history_store_queries
                .set(self.history_store_queries.get().saturating_add(1));
            let filter = atom.to_nostr();
            let rows = match pinned_relays {
                Some(relays) => self
                    .resolver
                    .store()
                    .query_newest_before_observed_by(&filter, relays, before, needed)?,
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
                candidates.entry(stored.event.id).or_insert_with(|| Row {
                    event: stored.event,
                    sources: stored.provenance.seen.into_keys().collect(),
                });
            }
        }
        let mut ordered: Vec<Row> = candidates.into_values().collect();
        ordered.sort_by(|a, b| {
            nip01_newest_first(
                (a.event.created_at.as_secs(), &a.event.id),
                (b.event.created_at.as_secs(), &b.event.id),
            )
        });
        ordered.truncate(needed);
        let auth_status = self.auth_status_map();
        let evidence = evidence::acquisition_evidence(
            &subtree_atoms,
            plan,
            self.resolver.store(),
            &self.connected_relays,
            &auth_status,
            &self.ever_connected_relays,
        );

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
        let Some(state) = self.histories.get(&id) else {
            return true;
        };
        let Some(primary) = state.handle_ids.first().copied() else {
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
        let pinned_relays = match (
            state.query.live_query().0.cache,
            &state.query.live_query().0.source,
        ) {
            (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays.clone()),
            _ => None,
        };
        let eligible = |sources: &BTreeSet<RelayUrl>| {
            pinned_relays
                .as_ref()
                .is_none_or(|relays| sources.iter().any(|relay| relays.contains(relay)))
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
                    || !eligible(&changed.observed_relays)
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
                        event: changed.event.clone(),
                        sources: changed.observed_relays.clone(),
                    }),
                );
            }
        }

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
                if !matches(&row.event) || !eligible(&row.observed_relays) {
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
                    event: row.event.clone(),
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
                } else if pinned_relays.is_some() && eligible(&row.observed_relays) {
                    // An event already cached from an ineligible relay can
                    // enter a Strict projection when this committed duplicate
                    // is its first eligible observation. Treat that transition
                    // as an affected-row insertion, then let the same bounded
                    // order rebalance decide whether it belongs in top-N.
                    remember(row.event.id, state, &mut before);
                    let projected = strict_promotions
                        .remove(&row.event.id)
                        .expect("eligible Strict promotion was prefetched");
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
            #[cfg(test)]
            self.history_store_queries
                .set(self.history_store_queries.get().saturating_add(1));
            let queried = match pinned_relays.as_ref() {
                Some(relays) => self.resolver.store().query_newest_before_any_observed_by(
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
        if deltas.is_empty() {
            return true;
        }
        let batch = self.history_batch(id, deltas, WindowLoad::Idle);
        if let Some(state) = self.histories.get(&id) {
            state.sink.on_history(batch.clone());
        }
        effects.push(Effect::EmitHistory(id, batch));
        true
    }
}
