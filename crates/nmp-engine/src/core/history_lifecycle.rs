//! Expandable observation-window lifecycle and projection.
//!
//! This module owns staged window growth, commit/rollback, bounded history
//! reconciliation, and mutation projection for active history sessions.

use super::*;

/// Every open history window, and the handle-to-window index that must
/// mirror it exactly.
///
/// Fields are PRIVATE and this is a sibling module of `query` and `write`,
/// so I4 -- `by_handle[h] == id` iff `h` is one of session `id`'s handles --
/// is maintained here or not at all (#1606 step 2).
///
/// Before this owner existed the invariant was hand-maintained at SEVEN
/// sites: two that link (open, advance) and five that unlink (two open
/// failure paths, unsubscribe, superseded tie-seconds, load rollback). Three
/// of the unlink sites open-coded the same three-collection dance across
/// `by_handle`, `HistoryState::handles` and `HistoryState::handle_ids`.
///
/// It holds state and its invariant, and nothing else: no `store`, no
/// `resolver`, no `router`, no `Effect`. Resolver withdrawal and wire
/// teardown are orchestration and stay at the call sites -- which is also
/// what keeps a real difference between those sites visible rather than
/// collapsed; see `retire`.
#[derive(Default)]
pub(super) struct HistorySessions {
    sessions: HashMap<HistorySessionId, HistoryState>,
    by_handle: HashMap<HandleId, HistorySessionId>,
    next_id: u64,
}

#[cfg(any(test, feature = "test-instrumentation"))]
pub(super) struct HistorySessionCounts {
    pub(super) sessions: usize,
    pub(super) handles: usize,
}

/// The two shapes the unlink callers already hold: a set from the rollback
/// path and a vec from the superseded path. A trait rather than forcing one
/// of them to convert, because the conversion is what an open-coded unlink
/// avoids and re-introducing it would be a step backwards.
pub(super) trait HandleSet {
    fn ids(&self) -> Vec<HandleId>;
    fn holds(&self, handle: &HandleId) -> bool;
}

impl HandleSet for BTreeSet<HandleId> {
    fn ids(&self) -> Vec<HandleId> {
        self.iter().copied().collect()
    }
    fn holds(&self, handle: &HandleId) -> bool {
        self.contains(handle)
    }
}

impl HandleSet for Vec<HandleId> {
    fn ids(&self) -> Vec<HandleId> {
        self.clone()
    }
    fn holds(&self, handle: &HandleId) -> bool {
        self.contains(handle)
    }
}

/// One window's bounded canonical row set: which rows the window projects,
/// and their canonical newest-first order, as ONE fact.
///
/// Fields are PRIVATE, so I5 -- `rows` and `order` name exactly the same
/// event ids, and every `order` key carries its row's own `created_at` -- is
/// maintained here or not at all (#1921).
///
/// Before this owner existed the two collections were separate
/// `HistoryState` fields whose doc comment claimed they had "same
/// membership", and `CoreState` mutated them by hand at THIRTEEN sites
/// across four methods, in four spellings: remove-the-row-then-rebuild-and-
/// remove-its-key (five, one of which read `order`'s tail to choose the row
/// and carried `.expect("history order and membership stay identical")` --
/// an invariant named in a panic message because no type held it),
/// insert-the-key-then-insert-the-row (five), assign-both-whole-sets (one),
/// and two that overwrote a row in place through `last_rows`' `get_mut` and
/// never touched `order` at all.
///
/// All thirteen were correct, and those last two are worth naming for why:
/// NIP-01 commits `created_at` into the event id, so a same-id overwrite
/// cannot move the order key -- a fact that lives two crates away from the
/// lines relying on it, and that nothing near them stated.
/// [`Self::insert`] re-keys from the row it displaces, unconditionally, so
/// the fact stops being load-bearing.
///
/// Every mutating door returns the value it displaced. The committed-batch
/// path has to remember each touched row's pre-state to roll back a failed
/// backfill, and it built that journal by reading each row before writing
/// it; returning the displaced value makes the journal fall out of the
/// mutation instead.
///
/// It holds state and its invariant, and nothing else: no `store`, no
/// `resolver`, no `Effect`. This owner takes no store parameter on any door
/// -- not `&RedbStore`, not `&mut RedbStore` -- because the reads that fill a
/// window are the caller's transaction, and what crosses back is a `Row`.
#[derive(Default)]
pub(super) struct HistoryRows {
    /// The bounded canonical payload set. History delivery is latest-wins,
    /// so every emitted frame must be able to stand alone after intermediate
    /// deltas are overwritten.
    rows: BTreeMap<EventId, Row>,
    /// The same membership, ordered canonically newest-first. This makes
    /// top/bottom rebalance O(log max_rows), never an O(total) sort after
    /// every committed row mutation.
    order: BTreeSet<(Reverse<u64>, EventId)>,
}

impl HistoryRows {
    /// This row's canonical order key.
    fn key(row: &Row) -> (Reverse<u64>, EventId) {
        (Reverse(row.created_at().as_secs()), row.id())
    }

    /// Project `row`, displacing and returning whatever this window held
    /// under the same id. Re-keys `order` from the displaced row's OWN
    /// `created_at`, so a replacement whose timestamp moved cannot orphan
    /// the key it replaced.
    pub(super) fn insert(&mut self, row: Row) -> Option<Row> {
        let key = Self::key(&row);
        let previous = self.rows.insert(row.id(), row);
        if let Some(previous) = previous.as_ref() {
            self.order.remove(&Self::key(previous));
        }
        self.order.insert(key);
        previous
    }

    /// Stop projecting `id`, returning the row the window held.
    pub(super) fn remove(&mut self, id: &EventId) -> Option<Row> {
        let removed = self.rows.remove(id);
        if let Some(removed) = removed.as_ref() {
            self.order.remove(&Self::key(removed));
        }
        removed
    }

    /// Replace the whole projection, the full-refresh oracle's door. The
    /// order index is derived here rather than passed in, so a caller cannot
    /// hand over a membership and an order that disagree.
    pub(super) fn replace_all(&mut self, rows: BTreeMap<EventId, Row>) {
        self.order = rows.values().map(Self::key).collect();
        self.rows = rows;
    }

    /// Evict from the oldest end until the window projects at most `target`
    /// rows, returning what was evicted, oldest first.
    pub(super) fn truncate_to(&mut self, target: usize) -> Vec<Row> {
        let mut evicted = Vec::new();
        while self.rows.len() > target {
            let Some((_, event_id)) = self.order.iter().next_back().copied() else {
                break;
            };
            let row = self
                .remove(&event_id)
                .expect("the order index and the row set name one membership");
            evicted.push(row);
        }
        evicted
    }

    pub(super) fn get(&self, id: &EventId) -> Option<&Row> {
        self.rows.get(id)
    }

    pub(super) fn contains(&self, id: &EventId) -> bool {
        self.rows.contains_key(id)
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Every projected event id, in id order -- the removed-delta scan,
    /// which compares membership and never consults position.
    pub(super) fn ids(&self) -> impl Iterator<Item = &EventId> + '_ {
        self.rows.keys()
    }

    /// The projection as the app receives it: every row, canonical
    /// newest-first. Fusing the two collections HERE is what makes the walk
    /// total -- the open-coded version was a `filter_map` over `order` that
    /// silently dropped any row `order` named and `rows` had lost, so the
    /// one corruption it could have reported was the one it swallowed.
    pub(super) fn newest_first(&self) -> impl Iterator<Item = &Row> + '_ {
        self.order.iter().map(|(_, event_id)| {
            self.rows
                .get(event_id)
                .expect("the order index and the row set name one membership")
        })
    }

    /// The oldest projected row's cursor: the window's current canonical
    /// boundary, and the point a backfill reads below.
    pub(super) fn boundary(&self) -> Option<nmp_store::EventCursor> {
        self.order
            .iter()
            .next_back()
            .map(|(Reverse(created_at), event_id)| {
                nmp_store::EventCursor::new(Timestamp::from(*created_at), *event_id)
            })
    }

    /// I5, checked: one membership, and every key carrying its own row's
    /// timestamp. Lives with the fields it constrains rather than in the
    /// session owner above, which cannot see them.
    #[cfg(any(test, feature = "test-instrumentation"))]
    fn assert_consistent(&self, at: &str, id: HistorySessionId) {
        assert_eq!(
            self.rows.len(),
            self.order.len(),
            "{at}: history session {id:?} holds {} rows but orders {}",
            self.rows.len(),
            self.order.len()
        );
        for (Reverse(created_at), event_id) in &self.order {
            let row = self.rows.get(event_id).unwrap_or_else(|| {
                panic!("{at}: history session {id:?} orders row {event_id}, which it does not hold")
            });
            assert_eq!(
                row.created_at().as_secs(),
                *created_at,
                "{at}: history session {id:?} orders row {event_id} at {created_at}, but the row itself is at {}",
                row.created_at().as_secs()
            );
        }
    }
}

impl HistorySessions {
    pub(super) fn new() -> Self {
        Self {
            next_id: 1,
            ..Self::default()
        }
    }

    /// Install one window and link every handle it opened with.
    ///
    /// A duplicate handle is refused rather than relinked: every handle here
    /// is either freshly opened by this call's own caller or, for
    /// `link_advance_handles`, freshly opened by an advance, so an existing
    /// `by_handle` entry means some earlier session was never properly
    /// retired -- relinking it here would silently hand that session's
    /// handle to a second window while the first still believed it owned it
    /// (compare `owner_index.rs`'s `insert`, which refuses the identical
    /// case for the same reason).
    pub(super) fn open(
        &mut self,
        state: HistoryState,
        handle_ids: impl IntoIterator<Item = HandleId>,
    ) -> HistorySessionId {
        let id = HistorySessionId(self.next_id);
        // Checked, not wrapping. Exhausting a u64 at one mint per history
        // session is not reachable by this process -- which is an argument
        // for the width, not for silently re-minting an id a still-live
        // `by_handle` entry could still be addressed to.
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("history session ids are exhausted; ids must never be reused");
        for handle_id in handle_ids {
            assert!(
                self.by_handle.insert(handle_id, id).is_none(),
                "HistorySessions: handle {handle_id:?} was already linked to a session when session {id:?} opened"
            );
        }
        self.sessions.insert(id, state);
        id
    }

    /// Remove one window and unlink every handle it still holds.
    ///
    /// Returns the window so the caller can decide what its handles need:
    /// the two `open_history_observation` failure paths withdraw them from
    /// the resolver only, because they run BEFORE the first
    /// `attach_wire_handle` and there is no wire demand to tear down;
    /// `on_unsubscribe_history` must also detach wire handles and withdraw
    /// demand. That difference is a rule, not an oversight, and it stays at
    /// the call sites where it is visible.
    pub(super) fn retire(&mut self, id: HistorySessionId) -> Option<HistoryState> {
        let state = self.sessions.remove(&id)?;
        for handle_id in &state.handle_ids {
            let owner = self.by_handle.remove(handle_id).unwrap_or_else(|| {
                panic!(
                    "HistorySessions: retiring session {id:?} found no by_handle entry for its own handle {handle_id:?}"
                )
            });
            assert_eq!(
                owner, id,
                "HistorySessions: retiring session {id:?} found handle {handle_id:?} indexed under a different session"
            );
        }
        Some(state)
    }

    /// Index handles an advance just pushed onto the window.
    ///
    /// The window half of that push happens under a `get_mut` borrow, so
    /// this closes the index half the moment the borrow ends. Both halves of
    /// I4 are still inside this module.
    pub(super) fn link_advance_handles(&mut self, id: HistorySessionId, handle_ids: &[HandleId]) {
        for handle_id in handle_ids {
            assert!(
                self.by_handle.insert(*handle_id, id).is_none(),
                "HistorySessions: handle {handle_id:?} was already linked to a session when session {id:?}'s advance opened it"
            );
        }
    }

    /// Unlink handles a window no longer holds, from all three collections
    /// at once. The superseded-tie-second and load-rollback paths each
    /// open-coded this; now neither can forget a half.
    pub(super) fn unlink_handles<S>(&mut self, id: HistorySessionId, handle_ids: &S)
    where
        S: HandleSet + ?Sized,
    {
        for handle_id in handle_ids.ids() {
            let owner = self.by_handle.remove(&handle_id).unwrap_or_else(|| {
                panic!(
                    "HistorySessions: unlinking session {id:?} found no by_handle entry for handle {handle_id:?}"
                )
            });
            assert_eq!(
                owner, id,
                "HistorySessions: unlinking session {id:?} found handle {handle_id:?} indexed under a different session"
            );
        }
        // Every caller holds `id` live across this call (the commit and
        // rollback paths both `expect` it immediately before and after), so
        // a missing session here is the mirror disagreeing with itself, not
        // a benign teardown race -- tolerating it used to mean the reverse
        // edges above were already dropped while the window's own
        // `handles`/`handle_ids` silently kept the stale entries.
        let state = self.sessions.get_mut(&id).unwrap_or_else(|| {
            panic!("HistorySessions: unlink_handles targeted session {id:?}, which is not live")
        });
        state
            .handles
            .retain(|handle| !handle_ids.holds(&handle.id()));
        state.handle_ids.retain(|handle| !handle_ids.holds(handle));
    }

    pub(super) fn get(&self, id: HistorySessionId) -> Option<&HistoryState> {
        self.sessions.get(&id)
    }

    pub(super) fn get_mut(&mut self, id: HistorySessionId) -> Option<&mut HistoryState> {
        self.sessions.get_mut(&id)
    }

    /// The window, for the paths that already established it is live. Panics
    /// exactly where the `self.histories[&id]` index it replaces did.
    pub(super) fn expect_live(&self, id: HistorySessionId) -> &HistoryState {
        self.sessions
            .get(&id)
            .expect("history session remains live")
    }

    /// The window a resolver handle belongs to.
    pub(super) fn session_for_handle(&self, handle: HandleId) -> Option<HistorySessionId> {
        self.by_handle.get(&handle).copied()
    }

    /// Exact structural consistency for I4 and for a window's own
    /// membership/order agreement, by identity rather than by count.
    ///
    /// I4: every handle a session reports in `handle_ids` must resolve
    /// back through `by_handle` to that SAME session, and every `by_handle`
    /// entry must point at a session that still reports the handle it names.
    /// `counts()` -- checked elsewhere -- verifies totals; it cannot see one
    /// handle indexed under the wrong session, because that swap preserves
    /// every count it reports (same reasoning as `OwnerIndexed::
    /// assert_consistent` and `RequestAttempts::assert_consistent`).
    ///
    /// Membership/order is I5 and lives on [`HistoryRows`], which owns both
    /// collections; this walk delegates to it per session rather than
    /// restating it, because the fields it constrains are private there.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fn assert_consistent(&self, at: &str) {
        for (id, state) in &self.sessions {
            state.rows.assert_consistent(at, *id);
            for handle_id in &state.handle_ids {
                let owner = self.by_handle.get(handle_id).unwrap_or_else(|| {
                    panic!(
                        "{at}: history session {id:?} reports handle {handle_id:?}, which has no by_handle entry"
                    )
                });
                assert_eq!(
                    owner, id,
                    "{at}: history session {id:?} reports handle {handle_id:?}, which by_handle indexes under a different session"
                );
            }
        }
        for (handle_id, id) in &self.by_handle {
            let state = self.sessions.get(id).unwrap_or_else(|| {
                panic!(
                    "{at}: by_handle names session {id:?} for handle {handle_id:?}, which is not live"
                )
            });
            assert!(
                state.handle_ids.contains(handle_id),
                "{at}: by_handle names handle {handle_id:?} under session {id:?}, which does not report it"
            );
        }
    }

    pub(super) fn ids(&self) -> Vec<HistorySessionId> {
        self.sessions.keys().copied().collect()
    }

    /// Every history handle with the acquisition decision its branch froze,
    /// for the root's wire-ownership rebuild. A read-only iterator: the
    /// rebuild counts, it does not reshape a window.
    pub(super) fn wire_attachments(
        &self,
    ) -> impl Iterator<Item = (HandleId, &HandleAcquisition)> + '_ {
        self.sessions.values().flat_map(|state| {
            state.handle_ids.iter().filter_map(move |handle_id| {
                let branch = state.branch_of.get(handle_id).copied().unwrap_or_default();
                state
                    .acquisitions_by_branch
                    .get(branch)
                    .map(|acquisition| (*handle_id, acquisition))
            })
        })
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fn counts(&self) -> HistorySessionCounts {
        HistorySessionCounts {
            sessions: self.sessions.len(),
            handles: self.by_handle.len(),
        }
    }

}

impl CoreState {
    /// The refused-open unwind, shared by `open_history_observation`'s two
    /// fallible projections (canonical rows, opening evidence). A window is
    /// installed whole or not at all, so both retire the just-created session
    /// and withdraw its handles from the resolver.
    ///
    /// Resolver-ONLY, deliberately: both callers run BEFORE the first
    /// `attach_wire_handle`, so there is no wire demand to withdraw.
    /// `on_unsubscribe_history` does both and stays separate for exactly that
    /// reason — #1695 checked whether the three unwinds could collapse into
    /// one and found that difference is a rule, not duplication. These two
    /// were byte-identical apart from the message, which is the kind of
    /// duplication the next divergence bug comes from.
    fn refuse_history_open(
        &mut self,
        id: HistorySessionId,
        _error: PersistenceError,
        reason: String,
        mut effects: Vec<Effect>,
    ) -> ObservationOpen<HistorySessionId, HistoryBatch> {
        let withdrawn = self
            .history
            .retire(id)
            .map(|state| state.live_handle_ids)
            .unwrap_or_default();
        for handle_id in withdrawn {
            let delta = self.resolver.unsubscribe(handle_id);
            self.consume_resolver_delta(delta);
        }
        self.flush_consumed_resolver_closes(&mut effects);
        ObservationOpen::Refused { reason, effects }
    }

    pub(in crate::core) fn open_history_observation(
        &mut self,
        query: HistoryQuery,
        now: Timestamp,
    ) -> ObservationOpen<HistorySessionId, HistoryBatch> {
        let mut effects = Vec::new();
        // Every branch's live-top acquisition opens before the session
        // exists. A failure part-way through withdraws what was already
        // opened: a window is installed whole or not at all.
        let mut handles = Vec::new();
        for branch in query.initial_demands() {
            match self.resolver.subscribe(&self.store, branch) {
                SubscribeOutcome::Opened { handle, delta } => {
                    self.consume_resolver_delta(delta);
                    handles.push(handle);
                }
                SubscribeOutcome::Refused { error, delta } => {
                    self.consume_resolver_delta(delta);
                    for handle in handles {
                        let delta = self.resolver.unsubscribe(handle.id());
                        self.consume_resolver_delta(delta);
                    }
                    self.flush_consumed_resolver_closes(&mut effects);
                    let reason = format!("canonical history resolution failed: {error}");
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
            match self.decide_handle_acquisition(branch, freshness, now) {
                Ok(acquisition) => acquisitions_by_branch.push(acquisition),
                Err(error) => {
                    for handle in handles {
                        let delta = self.resolver.unsubscribe(handle.id());
                        self.consume_resolver_delta(delta);
                    }
                    self.flush_consumed_resolver_closes(&mut effects);
                    let reason = format!("history freshness decision failed: {error}");
                    return ObservationOpen::Refused { reason, effects };
                }
            }
        }
        let live_handle_ids: Vec<HandleId> = handles.iter().map(|handle| handle.id()).collect();
        let mut branch_of = BTreeMap::new();
        for (index, handle) in handles.iter().enumerate() {
            branch_of.insert(handle.id(), index);
        }
        let handle_ids: BTreeSet<HandleId> = live_handle_ids.iter().copied().collect();
        let linked = handle_ids.clone();
        let id = self.history.open(
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
                rows: HistoryRows::default(),
                last_evidence: None,
                projection_complete: false,
                load: WindowLoad::Idle,
                pending_load: None,
            },
            linked,
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
                return self.refuse_history_open(id, error, reason, effects);
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
                return self.refuse_history_open(id, error, reason, effects);
            }
        };
        let attachments: Vec<_> = {
            let state = self.history.expect_live(id);
            state
                .live_handle_ids
                .iter()
                .enumerate()
                .map(|(branch, handle)| (*handle, state.acquisitions_by_branch[branch].clone()))
                .collect()
        };
        let mut diagnostics_changed = false;
        for (handle, acquisition) in attachments {
            diagnostics_changed |= self.attach_wire_handle(handle, &acquisition, &mut effects);
        }
        self.flush_consumed_resolver_closes(&mut effects);
        if diagnostics_changed {
            effects.push(Effect::DiagnosticsChanged);
        }
        if self.wire_admission_needed() {
            effects.push(Effect::ArmWireAdmission);
        }
        let seed = self
            .apply_history_projection(id, current, evidence, WindowLoad::Idle)
            .expect("a new history session has no prior projection and always yields one seed");
        ObservationOpen::Opened { id, seed, effects }
    }

    pub(in crate::core) fn on_subscribe_history(&mut self, query: HistoryQuery) -> Vec<Effect> {
        match self.open_history_observation(query, self.clock) {
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

    pub(in crate::core) fn on_unsubscribe_history(&mut self, id: HistorySessionId) -> Vec<Effect> {
        let Some(state) = self.history.retire(id) else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        let mut closing = Vec::new();
        // Unlike the open-failure unwinds, this window has been through
        // `attach_wire_handle`, so its wire demand must come down too.
        for handle in state.handles {
            closing.extend(self.detach_wire_handle(handle.id()));
            let resolver_delta = self.resolver.unsubscribe(handle.id());
            self.consume_resolver_delta(resolver_delta);
        }
        self.flush_consumed_resolver_closes(&mut effects);
        self.withdraw_wire_demand(closing, &mut effects);
        effects
    }

    /// Declaratively raise this window's row target (#485). Monotonic,
    /// idempotent, and clamped to the declared `max_rows`. Replaces the old
    /// `on_load_older` continuation-token door: there is no token to validate,
    /// no generation to go stale, and no `LoadInProgress`/`AtBound`/
    /// `NoBoundary` error — an in-flight advance simply raises the target, and
    /// being at the bound is a frame fact, not an error.
    pub(in crate::core) fn on_request_rows(
        &mut self,
        id: HistorySessionId,
        at_least: usize,
    ) -> Vec<Effect> {
        let Some(state) = self.history.get(id) else {
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
                self.history
                    .get_mut(id)
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
    pub(in crate::core) fn window_boundary(
        &self,
        id: HistorySessionId,
    ) -> Option<nmp_store::EventCursor> {
        Some(self.history.get(id)?.rows.boundary()?)
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
    pub(in crate::core) fn stage_history_advance(
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
                .history
                .get(id)
                .expect("advance requires a live session");
            let prior_target = state.target_rows;
            let old_len = state.rows.len();
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
            let state = self.history.get_mut(id).expect("history remains live");
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
            let state = self.history.get_mut(id).expect("history remains live");
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
            match self.resolver.subscribe(&self.store, demand) {
                SubscribeOutcome::Opened { handle, delta } => {
                    self.consume_resolver_delta(delta);
                    opened.push((branch, handle, kind));
                }
                SubscribeOutcome::Refused { error: _, delta } => {
                    self.consume_resolver_delta(delta);
                    for (_, handle, _) in opened {
                        let delta = self.resolver.unsubscribe(handle.id());
                        self.consume_resolver_delta(delta);
                    }
                    self.flush_consumed_resolver_closes(&mut effects);
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
            let mut linked = Vec::with_capacity(opened.len());
            let state = self
                .history
                .get_mut(id)
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
                state
                    .pending_load
                    .as_mut()
                    .expect("load was staged before opening resolver handles")
                    .opened_handle_ids
                    .push(handle_id);
                linked.push(handle_id);
            }
            // The other half of I4, once the window borrow ends.
            self.history.link_advance_handles(id, &linked);
        }

        let attachments: Vec<_> = {
            let state = self.history.expect_live(id);
            state
                .pending_load
                .as_ref()
                .expect("load remains staged after resolver handles open")
                .opened_handle_ids
                .iter()
                .filter_map(|handle| {
                    let branch = state.branch_of.get(handle).copied()?;
                    Some((*handle, state.acquisitions_by_branch.get(branch)?.clone()))
                })
                .collect()
        };
        let mut diagnostics_changed = false;
        for (handle, acquisition) in attachments {
            diagnostics_changed |= self.attach_wire_handle(handle, &acquisition, &mut effects);
        }
        self.flush_consumed_resolver_closes(&mut effects);
        if diagnostics_changed {
            effects.push(Effect::DiagnosticsChanged);
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
                        RowDelta::Added(row) => Some(row.id()),
                        RowDelta::Updated(_)
                        | RowDelta::SourcesGrew { .. }
                        | RowDelta::Removed(_) => None,
                    })
                    .collect();
                let pending = self
                    .history
                    .get_mut(id)
                    .expect("history remains live during staged advance")
                    .pending_load
                    .as_mut()
                    .expect("load remains staged until runtime acknowledgement");
                pending.added_row_ids = added_row_ids;
                pending.staged_batches = vec![requesting, batch];
                added
            }
            Err(_error) => {
                if let Some(state) = self.history.get_mut(id) {
                    state.projection_complete = false;
                }
                effects.extend(self.on_rollback_history_load(id));
                effects.push(Effect::HistoryLoadResult(
                    id,
                    Err(HistoryAdvanceError::StoreUnavailable),
                ));
                return effects;
            }
        };
        debug_assert!(added <= needed);
        // #1886: attaching a handle only marks its atoms PENDING. Admission
        // is the transition that compiles a pending atom into a wire REQ, and
        // the runtime runs it only from the deadline this effect arms. Every
        // other `attach_wire_handle` caller arms here; the staged advance did
        // not, so the tie-second and older-range REQs it built sat pending
        // until some unrelated demand change happened to arm admission --
        // which, on a plain scroll, is the NEXT advance's commit withdrawing
        // this one's superseded handles. Hence "the first advance does
        // nothing". Armed on the success path only: both failure paths above
        // return through `on_rollback_history_load`, whose
        // `withdraw_wire_demand` arms for whatever the detach left pending.
        if self.wire_admission_needed() {
            effects.push(Effect::ArmWireAdmission);
        }
        effects.push(Effect::HistoryLoadResult(id, Ok(())));
        effects
    }

    /// Stage a single `AtBound { max }` frame beat: the window is already at
    /// its declared ceiling, so `request_rows` cannot grow it, but the caller
    /// still gets one delivered fact. It rides the same staged commit path as
    /// a real advance (no opened handles and no target change) so it conflates
    /// identically and rolls back cleanly if the runtime never accepts it.
    pub(in crate::core) fn stage_history_atbound(
        &mut self,
        id: HistorySessionId,
        max: usize,
    ) -> Vec<Effect> {
        let (prior_target, prior_load, prior_evidence, prior_projection_complete) = {
            let state = self.history.get(id).expect("history remains live");
            (
                state.target_rows,
                state.load,
                state.last_evidence.clone(),
                state.projection_complete,
            )
        };
        let batch = self.history_batch(id, Vec::new(), WindowLoad::AtBound { max });
        let state = self.history.get_mut(id).expect("history remains live");
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

    pub(in crate::core) fn on_commit_history_load(&mut self, id: HistorySessionId) -> Vec<Effect> {
        if !self
            .history
            .get(id)
            .is_some_and(|state| state.pending_load.is_some())
        {
            return Vec::new();
        }
        let mut effects = Vec::new();

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
                .history
                .get(id)
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
                .history
                .get(id)
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
        let mut withdrawn = Vec::new();
        if !superseded.is_empty() {
            for handle_id in &superseded {
                withdrawn.extend(self.detach_wire_handle(*handle_id));
                let resolver_delta = self.resolver.unsubscribe(*handle_id);
                self.consume_resolver_delta(resolver_delta);
            }
            // One door for all three collections; neither this path nor the
            // rollback below can forget a half any more.
            self.history.unlink_handles(id, &superseded);
            let state = self
                .history
                .get_mut(id)
                .expect("committed history remained live");
            for handle_id in &superseded {
                state.handle_ids.remove(handle_id);
                state.acquisitions.remove(handle_id);
                state.branch_of.remove(handle_id);
            }
        }

        self.flush_consumed_resolver_closes(&mut effects);
        self.withdraw_wire_demand(withdrawn, &mut effects);

        let (made_progress, target, len, has_boundary) = {
            let state = self
                .history
                .get_mut(id)
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
                state.rows.len(),
                !state.rows.is_empty(),
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

    pub(in crate::core) fn on_rollback_history_load(
        &mut self,
        id: HistorySessionId,
    ) -> Vec<Effect> {
        let Some(pending) = self
            .history
            .get_mut(id)
            .and_then(|state| state.pending_load.take())
        else {
            return Vec::new();
        };

        let mut effects = Vec::new();
        let opened: BTreeSet<_> = pending.opened_handle_ids.iter().copied().collect();
        let mut withdrawn = Vec::new();
        for handle_id in &opened {
            withdrawn.extend(self.detach_wire_handle(*handle_id));
            let resolver_delta = self.resolver.unsubscribe(*handle_id);
            self.consume_resolver_delta(resolver_delta);
        }
        self.history.unlink_handles(id, &opened);
        let state = self
            .history
            .get_mut(id)
            .expect("rollback target remained live while staged handles closed");
        state
            .acquisitions
            .retain(|handle, _| !opened.contains(handle));
        state.branch_of.retain(|handle, _| !opened.contains(handle));
        if let Some(second) = pending.acquired_tie_second {
            state.acquired_tie_seconds.remove(&second);
        }
        for event_id in pending.added_row_ids {
            state.rows.remove(&event_id);
        }
        state.target_rows = pending.prior_target_rows;
        state.load = pending.prior_load;
        state.last_evidence = pending.prior_evidence;
        state.projection_complete = pending.prior_projection_complete;

        self.flush_consumed_resolver_closes(&mut effects);
        self.withdraw_wire_demand(withdrawn, &mut effects);
        effects
    }

    /// Compile the resolver's current (possibly staged-history) demand into
    /// an isolated plan. A history advance changes only the outer time
    /// window of an already-live descriptor, so every discovery dependency
    /// is already represented by the initial session; shadow planning never
    /// needs to mutate the widen-only discovery subscription.
    pub(in crate::core) fn history_shadow_plans(&self, id: HistorySessionId) -> Vec<RelayPlan> {
        let Some(state) = self.history.get(id) else {
            return Vec::new();
        };
        let handles_by_branch = self.history_handles_by_branch(id);
        state
            .acquisitions_by_branch
            .iter()
            .enumerate()
            .map(|(branch, acquisition)| {
                let wire: BTreeSet<_> = handles_by_branch
                    .get(branch)
                    .into_iter()
                    .flatten()
                    .flat_map(|handle| self.wire_atoms_for_handle(*handle, acquisition))
                    .collect();
                if wire.is_empty() {
                    RelayPlan::default()
                } else {
                    self.shadow_plan_for(wire)
                }
            })
            .collect()
    }

    /// Every resolver handle this session holds open, grouped by canonical
    /// branch: branch `i`'s live-top acquisition plus whatever tie-second and
    /// older-range acquisitions are currently open for that same branch.
    fn history_handles_by_branch(&self, id: HistorySessionId) -> Vec<Vec<HandleId>> {
        let Some(state) = self.history.get(id) else {
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

    pub(in crate::core) fn refresh_all_histories(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<_> = self.history.ids();
        for id in ids {
            self.refresh_history(id, WindowLoad::Idle, effects);
        }
    }

    /// Refresh only acquisition evidence after a coverage-only mutation.
    /// The current bounded rows remain authoritative unless a prior store
    /// failure marked the projection incomplete, in which case the full
    /// refresh oracle repairs it before evidence is emitted.
    ///
    /// #1646: the production door for every AUTH transition, mirroring
    /// [`Self::refresh_all_observation_evidence`] for history sessions.
    pub(in crate::core) fn refresh_all_history_evidence(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<_> = self.history.ids();
        for id in ids {
            self.refresh_history_evidence(id, effects);
        }
    }

    pub(in crate::core) fn history_batch(
        &mut self,
        id: HistorySessionId,
        deltas: Vec<RowDelta>,
        load: WindowLoad,
    ) -> HistoryBatch {
        let state = self
            .history
            .get_mut(id)
            .expect("history batch requires a live session");
        state.load = load;
        let rows = state.rows.newest_first().cloned().collect();
        HistoryBatch {
            rows,
            deltas,
            evidence: state.last_evidence.clone().unwrap_or_default(),
            load,
        }
    }

    pub(in crate::core) fn refresh_history(
        &mut self,
        id: HistorySessionId,
        load: WindowLoad,
        effects: &mut Vec<Effect>,
    ) -> Option<usize> {
        let (current, evidence) = match self.history_rows_and_evidence_for(id) {
            Ok(value) => value,
            Err(_error) => {
                if let Some(state) = self.history.get_mut(id) {
                    state.projection_complete = false;
                }
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
        let state = self.history.get_mut(id)?;
        let current_rows = current.clone();
        let mut deltas = Vec::new();
        for (event_id, row) in current {
            match state.rows.get(&event_id) {
                None => deltas.push(RowDelta::Added(row)),
                Some(previous) if previous.signature() != row.signature() => {
                    deltas.push(RowDelta::Updated(row));
                }
                Some(previous) if previous.sources != row.sources => {
                    deltas.push(RowDelta::SourcesGrew {
                        id: event_id,
                        sources: row.sources,
                    });
                }
                Some(_) => {}
            }
        }
        for event_id in state.rows.ids() {
            if !current_rows.contains_key(event_id) {
                deltas.push(RowDelta::Removed(*event_id));
            }
        }
        let changed = !deltas.is_empty()
            || state.last_evidence.as_ref() != Some(&evidence)
            || state.load != load;
        state.rows.replace_all(current_rows);
        state.last_evidence = Some(evidence);
        state.projection_complete = true;
        if changed {
            Some(self.history_batch(id, deltas, load))
        } else {
            None
        }
    }

    pub(in crate::core) fn refresh_history_evidence(
        &mut self,
        id: HistorySessionId,
        effects: &mut Vec<Effect>,
    ) {
        let Some(state) = self.history.get(id) else {
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
            Err(_error) => {
                return;
            }
        };
        let Some(state) = self.history.get_mut(id) else {
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
        let Some(state) = self.history.get(id) else {
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

    pub(in crate::core) fn history_rows_and_evidence_for(
        &self,
        id: HistorySessionId,
    ) -> Result<(BTreeMap<EventId, Row>, Vec<AcquisitionEvidence>), PersistenceError> {
        let rows = self.history_rows_for(id)?;
        let evidence = self.history_evidence_for(id)?;
        Ok((rows, evidence))
    }

    pub(in crate::core) fn history_rows_for(
        &self,
        id: HistorySessionId,
    ) -> Result<BTreeMap<EventId, Row>, PersistenceError> {
        let state = self
            .history
            .get(id)
            .expect("history projection requires a live session");
        let mut by_id: BTreeMap<EventId, Row> = BTreeMap::new();
        for (branch, live) in state.live_handle_ids.iter().enumerate() {
            let declaration = &state.query.live_query().branches()[branch];
            let pinned_relays = match (declaration.cache, &declaration.routing) {
                (CacheMode::Strict, ReadRouting::Explicit(relays)) => {
                    Some(relays.iter().cloned().collect::<BTreeSet<_>>())
                }
                _ => None,
            };
            for mut atom in self.resolver.root_atoms(*live) {
                atom.limit = None;
                let filter = atom.to_nostr();
                // Taking the window target from EVERY branch is exact: a row
                // outside one branch's newest `target_rows` already has that
                // many newer witnesses in that same branch, so it can never
                // belong to the global newest `target_rows` either.
                let rows = match pinned_relays.as_ref() {
                    Some(relays) => {
                        self.store
                            .query_newest_under_pin(&filter, relays, state.target_rows)?
                    }
                    None => self.store.query_newest(&filter, state.target_rows)?,
                };
                for stored in rows {
                    let sources: BTreeSet<RelayUrl> = stored.provenance.seen.into_keys().collect();
                    match by_id.entry(stored.event.id) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(row_from_stored_event(
                                stored.event,
                                stored
                                    .provenance
                                    .local
                                    .as_ref()
                                    .map_or(SigState::Signed, |local| local.sig_state),
                                sources,
                            ));
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
                .map(|(event_id, row)| (row.created_at().as_secs(), *event_id))
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

    pub(in crate::core) fn advance_history_projection(
        &mut self,
        id: HistorySessionId,
        before: nmp_store::EventCursor,
        old_len: usize,
        plans: &[RelayPlan],
    ) -> Result<(HistoryBatch, usize), PersistenceError> {
        let state = self
            .history
            .get(id)
            .expect("history advance requires a live session");
        let needed = state.target_rows.saturating_sub(state.rows.len());
        let mut candidates = BTreeMap::<EventId, Row>::new();
        for (branch, live) in state.live_handle_ids.iter().enumerate() {
            let declaration = &state.query.live_query().branches()[branch];
            let pinned_relays = match (declaration.cache, &declaration.routing) {
                (CacheMode::Strict, ReadRouting::Explicit(relays)) => {
                    Some(relays.iter().cloned().collect::<BTreeSet<_>>())
                }
                _ => None,
            };
            for mut atom in self.resolver.root_atoms(*live) {
                atom.limit = None;
                let filter = atom.to_nostr();
                let rows = match pinned_relays.as_ref() {
                    Some(relays) => self
                        .store
                        .query_newest_before_under_pin(&filter, relays, before, needed)?,
                    None => self.store.query_newest_before(&filter, before, needed)?,
                };
                for stored in rows {
                    let sources: BTreeSet<RelayUrl> = stored.provenance.seen.into_keys().collect();
                    match candidates.entry(stored.event.id) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(row_from_stored_event(
                                stored.event,
                                stored
                                    .provenance
                                    .local
                                    .as_ref()
                                    .map_or(SigState::Signed, |local| local.sig_state),
                                sources,
                            ));
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
                (a.created_at().as_secs(), &a.id()),
                (b.created_at().as_secs(), &b.id()),
            )
        });
        ordered.truncate(needed);
        let by_branch = self.history_handles_by_branch(id);
        let mut evidence: Vec<AcquisitionEvidence> = Vec::with_capacity(by_branch.len());
        for (branch, handles) in by_branch.into_iter().enumerate() {
            // `?`: this whole advance is already all-or-nothing on a store
            // failure, and a coverage read is no different from the row
            // reads above it (#763).
            evidence.push(self.acquisition_evidence_for_scopes_with_plan(
                self.history_branch_demand_scopes(&handles),
                &self.history.expect_live(id).acquisitions_by_branch[branch],
                plans.get(branch).unwrap_or(&RelayPlan::default()),
            )?);
        }

        let state = self
            .history
            .get_mut(id)
            .expect("history remains live during synchronous projection");
        let mut deltas = Vec::with_capacity(ordered.len());
        for row in ordered {
            state.rows.insert(row.clone());
            deltas.push(RowDelta::Added(row));
        }
        state.last_evidence = Some(evidence);
        state.projection_complete = true;
        let added = state.rows.len().saturating_sub(old_len);
        let batch = self.history_batch(id, deltas, WindowLoad::Returned { added });
        Ok((batch, added))
    }

    /// Apply one committed store batch to any stable bounded history window,
    /// including Strict, derived, and multi-root selections. Only touched
    /// rows plus the exact newly exposed lower segment are visited: the
    /// canonical order index identifies eviction/backfill boundaries without
    /// sorting or replaying the retained window.
    pub(in crate::core) fn try_apply_committed_history_row_changes(
        &mut self,
        id: HistorySessionId,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) -> bool {
        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        let Some(state) = self.history.get(id) else {
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
            return state.rows.is_empty();
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
        let pinned_relays = match (declaration.cache, &declaration.routing) {
            (CacheMode::Strict, ReadRouting::Explicit(relays)) => {
                Some(relays.iter().cloned().collect::<BTreeSet<_>>())
            }
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
        let original_boundary = state.rows.boundary();
        let mut before = BTreeMap::<EventId, Option<Row>>::new();
        let mut visible_removals = 0usize;
        let mut strict_promotions = BTreeMap::<EventId, Row>::new();
        if pinned_relays.is_some() {
            for changed in &changes.provenance_grew {
                if !matches(&changed.event)
                    || !visible_under_pin(changed)
                    || state.rows.contains(&changed.event.id)
                {
                    continue;
                }
                let current = match self.store.query(&nostr::Filter::new().id(changed.event.id)) {
                    Ok(mut rows) => rows.pop().map(|stored| {
                        let signature_state = stored
                            .provenance
                            .local
                            .as_ref()
                            .map_or(SigState::Signed, |local| local.sig_state);
                        row_from_stored_event(
                            stored.event,
                            signature_state,
                            stored.provenance.seen.into_keys().collect(),
                        )
                    }),
                    Err(_error) => {
                        self.history
                            .get_mut(id)
                            .expect("history remained live after affected-row read failure")
                            .projection_complete = false;
                        return true;
                    }
                };
                strict_promotions.insert(
                    changed.event.id,
                    current.unwrap_or_else(|| {
                        row_from_stored_event(
                            {
                                #[cfg(feature = "bench-instrumentation")]
                                crate::ingest_attribution::projection_event_clone();
                                changed.event.clone()
                            },
                            changed.signature_state,
                            changed.observed_relays.clone(),
                        )
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
                .history
                .get_mut(id)
                .expect("history remained live during committed mutation");
            for event in &changes.removed {
                if let Some(row) = state.rows.remove(&event.id) {
                    before.entry(event.id).or_insert(Some(row));
                    visible_removals = visible_removals.saturating_add(1);
                }
            }
            for row in &changes.inserted {
                if !matches(&row.event) || !visible_under_pin(row) {
                    continue;
                }
                let event_id = row.event.id;
                let remembered = row_from_stored_event(
                    {
                        #[cfg(feature = "bench-instrumentation")]
                        crate::ingest_attribution::projection_event_clone();
                        row.event.clone()
                    },
                    row.signature_state,
                    row.observed_relays.clone(),
                );
                let previous = state.rows.insert(remembered);
                before.entry(event_id).or_insert(previous);
            }
            for row in &changes.provenance_grew {
                if !matches(&row.event) {
                    continue;
                }
                if let Some(mut grown) = state.rows.get(&row.event.id).cloned() {
                    grown.sources.extend(row.observed_relays.iter().cloned());
                    grown.set_signature(row_signature_from_store_state(
                        &row.event,
                        row.signature_state,
                    ));
                    let previous = state.rows.insert(grown);
                    before.entry(row.event.id).or_insert(previous);
                } else if pinned_relays.is_some() && visible_under_pin(row) {
                    // An event already cached from an unpinned relay can
                    // enter a Strict projection when this committed duplicate
                    // is its first observation by a pinned one. Treat that
                    // transition as an affected-row insertion, then let the
                    // same bounded order rebalance decide whether it belongs
                    // in top-N.
                    let projected = strict_promotions
                        .remove(&row.event.id)
                        .expect("a Strict promotion visible under the pin was prefetched");
                    let previous = state.rows.insert(projected);
                    before.entry(row.event.id).or_insert(previous);
                }
            }
            for row in &changes.updated {
                if !matches(&row.event) || !visible_under_pin(row) {
                    continue;
                }
                if state.rows.contains(&row.event.id) {
                    let previous = state.rows.insert(row_from_stored_event(
                        row.event.clone(),
                        row.signature_state,
                        row.observed_relays.clone(),
                    ));
                    before.entry(row.event.id).or_insert(previous);
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
            let queried = match pinned_relays.as_ref() {
                Some(relays) => self.store.query_newest_before_any_under_pin(
                    &filters,
                    relays,
                    boundary,
                    visible_removals,
                ),
                None => self
                    .store
                    .query_newest_before_any(&filters, boundary, visible_removals),
            };
            let rows = match queried {
                Ok(rows) => rows,
                Err(_error) => {
                    let state = self
                        .history
                        .get_mut(id)
                        .expect("history remained live after failed backfill");
                    for (event_id, prior) in before {
                        match prior {
                            Some(prior) => state.rows.insert(prior),
                            None => state.rows.remove(&event_id),
                        };
                    }
                    state.projection_complete = false;
                    return true;
                }
            };
            let state = self
                .history
                .get_mut(id)
                .expect("history remained live during exact backfill");
            for stored in rows {
                let event_id = stored.event.id;
                if state.rows.contains(&event_id) {
                    continue;
                }
                before.entry(event_id).or_insert(None);
                let sources: BTreeSet<_> = stored.provenance.seen.into_keys().collect();
                let signature_state = stored
                    .provenance
                    .local
                    .as_ref()
                    .map_or(SigState::Signed, |local| local.sig_state);
                let row = row_from_stored_event(stored.event, signature_state, sources.clone());
                let remembered = row.clone();
                state.rows.insert(remembered);
            }
        }

        {
            let state = self
                .history
                .get_mut(id)
                .expect("history remained live during canonical truncation");
            for evicted in state.rows.truncate_to(target_rows) {
                before.entry(evicted.id()).or_insert(Some(evicted));
            }
        }

        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::history_projection_apply(phase_started.elapsed());
        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();

        let state = self
            .history
            .get(id)
            .expect("history remained live after committed rebalance");
        let mut deltas = Vec::new();
        for (event_id, prior) in &before {
            match (prior, state.rows.get(event_id)) {
                (None, Some(current)) => deltas.push(RowDelta::Added(current.clone())),
                (Some(_), None) => deltas.push(RowDelta::Removed(*event_id)),
                (Some(prior), Some(current)) if prior.signature() != current.signature() => {
                    deltas.push(RowDelta::Updated(current.clone()));
                }
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

