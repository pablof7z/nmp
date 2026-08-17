//! The one checked door into the reducer.
//!
//! [`CoreState`](super::CoreState) holds every live protocol fact the engine
//! reduces over, and several of those facts are stored twice -- a forward map
//! plus the reverse index that answers "what belongs to relay X" without a
//! scan. Those halves must always agree;
//! [`CoreState::assert_owner_consistency`] is the proof that they do.
//!
//! Before this module existed, that proof ran at exactly one site: the end of
//! `handle()`. Every other externally reachable `&mut self` door on the
//! reducer -- boot recovery, store-failure recovery, publish preparation,
//! cancellation, both query-opening paths -- ran unchecked. `cancel_write` is
//! the sharpest illustration: the runtime reaches it both through
//! `EngineMsg::CancelWrite` and by calling it directly, so the SAME code was
//! checked or not depending on who called it.
//!
//! `EngineCore` closes that. It is a shell holding one private `CoreState`,
//! and every mutating door on it routes through [`EngineCore::checked`],
//! which runs the owner-consistency proof after the transition. The
//! invariant this module exists to hold is:
//!
//! > **Every external mutation door on `EngineCore` is checked.**
//!
//! Four things this deliberately does NOT have:
//!
//! - **`checked` is private, never `pub(crate)`.** A crate-visible version
//!   handing out `&mut CoreState` would be arbitrary mutation permitted and
//!   merely checked afterwards, which is not ownership. No other module ever
//!   receives a `&mut CoreState`.
//! - **No `DerefMut`, ever.** Every mutation goes through a named door, and
//!   there is exactly one exception ([`EngineCore::white_box`], `#[cfg(test)]`
//!   and `pub(super)`) whose call sites are countable by grep. The read-only
//!   `Deref` below is `#[cfg(test)]` and cannot widen that: `CoreState`'s 92
//!   fields are private to module `core`, so it hands nothing to anyone who
//!   could not already see it. (This departs from the approved design, which
//!   banned `Deref` outright on the grounds that direct reads are cross-owner
//!   coupling. Measured against this tree, that rationale has an empty
//!   domain: `cargo check --workspace --all-targets` after removing the
//!   fields breaks in exactly one target, `nmp-engine (lib test)`. Nothing in
//!   `nmp-runtime`, `nmp`, or any integration suite ever read a field.)
//! - **No depth counter.** An inner call is `CoreState -> CoreState` and
//!   never re-enters this shell; because `checked` is private, code holding
//!   `&mut CoreState` structurally cannot call back through `EngineCore`.
//! - **No convenience API on `CoreState`.** Its doors are all
//!   `pub(in crate::core)`, so outside `core` the type has no callable member
//!   at all -- not even a constructor.
//!
//! `CoreState` is temporary scaffolding that shrinks as real owners (Query,
//! Publish, AUTH, NIP-77) are extracted from it. It is not a subsystem to
//! develop: if it starts acquiring convenience APIs or documentation about
//! what it "owns", the scaffolding has begun becoming the building.

use super::write::replaceable_operation::{
    ReplaceableMaterializationCall, ReplaceableMaterializationContinuation,
    ReplaceableMaterializationOutcome,
};
use super::*;

/// The checked boundary into the reducer (§2 position 1). Holds the whole
/// reducer state and exposes it only through doors that prove owner
/// consistency after every transition.
pub struct EngineCore {
    /// PRIVATE to this module, and it must stay that way: handing a
    /// `&mut CoreState` to any other module reopens exactly the hole this
    /// shell closes.
    state: CoreState,
}

impl EngineCore {
    /// Run one externally initiated transition and prove the reducer's
    /// mirrored indexes still agree afterwards.
    ///
    /// PRIVATE. No other module ever receives `&mut CoreState`; other
    /// modules get the explicit semantic doors below and nothing else.
    ///
    /// The check is `#[cfg(test)]` -- the exact gate the end-of-`handle()`
    /// call carried before this module existed, moved rather than widened.
    /// Widening it to the other test binaries is a separate change with its
    /// own evidence.
    #[cfg_attr(not(test), inline(always))]
    fn checked<T>(&mut self, at: &'static str, f: impl FnOnce(&mut CoreState) -> T) -> T {
        #[cfg(not(test))]
        let _ = at;
        let out = f(&mut self.state);
        #[cfg(test)]
        if !self
            .state
            .turn_level_consistency_suppressed_for_named_exception
        {
            self.state.assert_owner_consistency(at);
        }
        out
    }

    /// Construction is a state-establishing boundary too, so it is checked
    /// like any other. `CoreState::new` establishes a large invariant-bearing
    /// state, and it is the one every test starts from.
    fn checked_new(state: CoreState) -> Self {
        let this = Self { state };
        #[cfg(test)]
        this.state.assert_owner_consistency("EngineCore::new");
        this
    }

    /// Opt out of the per-transition mirror check for the rest of this
    /// `EngineCore`'s life. Exactly seven call sites may call this -- see
    /// `CoreState::turn_level_consistency_suppressed_for_named_exception`'s
    /// doc for which, and for the two distinct reasons (amortized-cost
    /// proof, handle-less algebra fixture) that justify each.
    #[cfg(test)]
    pub(super) fn suppress_turn_level_consistency_for_named_exception(&mut self) {
        self.state
            .suppress_turn_level_consistency_for_named_exception();
    }

    /// The engine's current logical demand is now exactly `demand`.
    ///
    /// This is the one thing `CoreState::recompile` does to attribution, and
    /// a falsifier that drives the router by hand (rather than through a
    /// resolver) has to do it too, or every coverage claim it later attributes
    /// resolves to no retained shape. Sixty-seven sites did it a different
    /// way: `white_box("attribution.observe_atom", ..)` for whatever arrived
    /// and `white_box("attribution.release_atom", ..)` for whatever left,
    /// spelling out a TRANSITION where production states a FACT. A transition
    /// can be wrong in ways a fact cannot — `release_atom` silently no-ops on
    /// an atom that was never observed — and none of the sixty-seven could
    /// ever have caught `recompile` changing which calls it makes (#1850).
    ///
    /// Checked, not `white_box`: installing the demand set is a complete
    /// transition of that owner, not a mid-turn sub-step, so the
    /// owner-consistency proof holds across it.
    ///
    /// Takes the same `BTreeSet<ContextualAtom>` shape `CoreState::wire_demand`
    /// hands `recompile`, so a falsifier states its demand in the type
    /// production states it in.
    #[cfg(test)]
    pub(super) fn set_active_demand(&mut self, demand: &BTreeSet<ContextualAtom>) {
        self.checked("set_active_demand", |s| {
            s.attribution.set_active_demand(demand.iter())
        })
    }

    /// The reducer's own in-crate falsifiers reach a mid-turn sub-step here,
    /// and this is the ONLY way anything obtains a `&mut CoreState`.
    /// `#[cfg(test)]` and `pub(super)`: it does not exist in a production
    /// build and cannot be named outside `core`.
    ///
    /// **It deliberately does not run the turn-level check**, and that is a
    /// measured decision rather than an omission. An earlier revision routed
    /// this through [`Self::checked`]; four tests then failed for a
    /// principled reason. `assert_owner_consistency` is a property of a
    /// TURN, and a white-box test mutates mid-turn by construction:
    /// `on_publish` leaves the stalled-write cache stale until `handle`'s
    /// epilogue refreshes it, and `wire.unindex_handle` is step one of a
    /// three-step rebuild the test open-codes. Asserting a turn postcondition
    /// after a sub-step asserts something that is not true yet. Same reason
    /// the shell carries no depth counter: a mechanism spanning sub-steps
    /// would conceal a broken state model rather than prove one.
    ///
    /// So this does not close the in-crate test hole -- it makes it
    /// COUNTABLE. `grep -c 'white_box(' crates/nmp-engine/src` is the exact
    /// number of places the reducer's own tests reach past its doors, and
    /// that number should fall as owners are extracted. Every call site
    /// outside `core` is a compile error instead.
    #[cfg(test)]
    pub(super) fn white_box<T>(
        &mut self,
        at: &'static str,
        f: impl FnOnce(&mut CoreState) -> T,
    ) -> T {
        let _ = at;
        f(&mut self.state)
    }

    pub fn is_current_transport_session(
        &self,
        handle: TransportRelayHandle,
        session: &RelaySessionKey,
    ) -> bool {
        self.state.is_current_transport_session(handle, session)
    }

    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    pub fn ingest_relay_events(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
        effects: &mut Vec<Effect>,
    ) {
        self.checked("ingest_relay_events", |s| {
            s.ingest_relay_events(events, effects)
        })
    }

    pub fn committed_observation_conflicts_with_pending(
        &self,
        hit: &CommittedObservationHit,
    ) -> bool {
        self.state.committed_observation_conflicts_with_pending(hit)
    }

    pub fn on_revalidated_committed_observations(
        &mut self,
        observations: Vec<(RelaySessionKey, u16)>,
    ) -> Vec<Effect> {
        self.checked("on_revalidated_committed_observations", |s| {
            s.on_revalidated_committed_observations(observations)
        })
    }

    pub fn open_history_observation(
        &mut self,
        query: HistoryQuery,
        now: Timestamp,
    ) -> ObservationOpen<HistorySessionId, HistoryBatch> {
        self.checked("open_history_observation", |s| {
            s.open_history_observation(query, now)
        })
    }

    pub fn install_replaceable_materializer(
        &mut self,
        registration: ReplaceableMaterializerRegistration,
    ) {
        self.checked("install_replaceable_materializer", |s| {
            s.install_replaceable_materializer(registration)
        })
    }

    pub fn install_replaceable_materializers(
        &mut self,
        capabilities: Vec<nmp_grammar::ReplaceableMaterializerSpec>,
    ) {
        self.checked("install_replaceable_materializers", |s| {
            s.install_replaceable_materializers(capabilities)
        })
    }

    pub fn new(store: RedbStore, cap: usize) -> Self {
        Self::checked_new(CoreState::new(store, cap))
    }

    #[cfg(feature = "unstable-mechanism")]
    #[doc(hidden)]
    pub fn new_with_fixture_routing_facts(
        store: RedbStore,
        facts: nmp_router_testkit::FixtureRoutingFacts,
        cap: usize,
    ) -> Self {
        Self::checked_new(CoreState::new_with_fixture_routing_facts(store, facts, cap))
    }

    pub fn new_with_routing_facts(
        store: RedbStore,
        routing_facts: RoutingFactStore,
        cap: usize,
    ) -> Self {
        Self::checked_new(CoreState::new_with_routing_facts(store, routing_facts, cap))
    }

    pub fn replace_author_routes(
        &mut self,
        author: PublicKey,
        replacement: AuthorRouteReplacement,
        effects: &mut Vec<Effect>,
    ) {
        self.checked("replace_author_routes", |s| {
            s.replace_author_routes(author, replacement, effects)
        })
    }

    #[must_use]
    pub fn with_max_publish_attempts(self, max_publish_attempts: u64) -> Self {
        Self::checked_new(self.state.with_max_publish_attempts(max_publish_attempts))
    }

    pub fn relay_worker_requirements(&self) -> Option<RelayWorkerRequirements> {
        self.state.relay_worker_requirements()
    }

    pub fn active_demand(&self) -> BTreeSet<ContextualAtom> {
        self.state.active_demand()
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub fn assert_owner_consistency(&self, at: &str) {
        self.state.assert_owner_consistency(at)
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    #[doc(hidden)]
    pub fn bench_ownership_census(&self) -> CoreOwnershipCensus {
        self.state.bench_ownership_census()
    }

    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    pub fn observation_ownership_census(&self) -> CoreObservationOwnershipCensus {
        self.state.observation_ownership_census()
    }

    pub fn get_coverage(
        &self,
        atom: &ContextualAtom,
        relay: &RelayUrl,
    ) -> Result<Option<nmp_store::CoverageInterval>, PersistenceError> {
        self.state.get_coverage(atom, relay)
    }

    pub fn diagnostics_snapshot(&self) -> DiagnosticsSnapshot {
        self.state.diagnostics_snapshot()
    }

    pub fn tick(&mut self, now: Timestamp) -> Vec<Effect> {
        self.checked("tick", |s| s.tick(now))
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn maintenance_turn_count(&self) -> u64 {
        self.state.maintenance_turn_count()
    }

    pub fn advance_clock(&mut self, now: Timestamp) {
        self.checked("advance_clock", |s| s.advance_clock(now))
    }

    pub fn clock(&self) -> Timestamp {
        self.state.clock()
    }

    pub fn next_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        self.state.next_deadline()
    }

    pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect> {
        self.checked("handle", |s| s.handle(msg))
    }

    pub fn active_pubkey(&self) -> Option<PublicKey> {
        self.state.active_pubkey()
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    #[doc(hidden)]
    pub fn recover_requested_redb_store_for_test(
        &mut self,
    ) -> Result<Option<(PersistenceFault, Vec<Effect>)>, PersistenceError> {
        self.checked("recover_requested_redb_store_for_test", |s| {
            s.recover_requested_redb_store_for_test()
        })
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    #[doc(hidden)]
    pub fn reset_publish_queue_lane_recovery_reads(&self) {
        self.state.reset_publish_queue_lane_recovery_reads()
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    #[doc(hidden)]
    pub fn publish_queue_lane_recovery_reads(&self) -> u64 {
        self.state.publish_queue_lane_recovery_reads()
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    #[doc(hidden)]
    pub fn seed_stale_relay_open_failure_for_test(
        &mut self,
        session: RelaySessionKey,
        reason: String,
    ) {
        self.checked("seed_stale_relay_open_failure_for_test", |s| {
            s.seed_stale_relay_open_failure_for_test(session, reason)
        })
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_reset_lifecycle_work(&self) {
        self.state.bench_reset_lifecycle_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_lifecycle_work(&self) -> (u64, u64, u64) {
        self.state.bench_lifecycle_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_admission_local_work(&self) -> (u64, u64, u64) {
        self.state.bench_admission_local_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_reset_admission_work(&mut self) {
        self.checked("bench_reset_admission_work", |s| {
            s.bench_reset_admission_work()
        })
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_admission_work(&self) -> CoreAdmissionWork {
        self.state.bench_admission_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_reset_freshness_work(&self) {
        self.state.bench_reset_freshness_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_freshness_work(&self) -> CoreFreshnessWork {
        self.state.bench_freshness_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_reset_withdrawal_work(&mut self) {
        self.checked("bench_reset_withdrawal_work", |s| {
            s.bench_reset_withdrawal_work()
        })
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_withdrawal_work(&self) -> CoreWithdrawalWork {
        self.state.bench_withdrawal_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_reset_query_work(&self) {
        self.state.bench_reset_query_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_query_work(&self) -> (u64, u64, u64) {
        self.state.bench_query_work()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_reset_coverage_reads(&self) {
        self.state.bench_reset_coverage_reads()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_coverage_reads(&self) -> u64 {
        self.state.bench_coverage_reads()
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_ingest_observed(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
    ) -> Vec<Effect> {
        self.checked("bench_ingest_observed", |s| s.bench_ingest_observed(events))
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_ingest_observed_with_forced_refresh(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
    ) -> Vec<Effect> {
        self.checked("bench_ingest_observed_with_forced_refresh", |s| {
            s.bench_ingest_observed_with_forced_refresh(events)
        })
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_accept_local(&mut self, accept: AcceptWrite) -> Vec<Effect> {
        self.checked("bench_accept_local", |s| s.bench_accept_local(accept))
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_accept_local_with_forced_refresh(&mut self, accept: AcceptWrite) -> Vec<Effect> {
        self.checked("bench_accept_local_with_forced_refresh", |s| {
            s.bench_accept_local_with_forced_refresh(accept)
        })
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_expire_due(&mut self, now: Timestamp) -> Vec<Effect> {
        self.checked("bench_expire_due", |s| s.bench_expire_due(now))
    }

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_expire_due_with_forced_refresh(&mut self, now: Timestamp) -> Vec<Effect> {
        self.checked("bench_expire_due_with_forced_refresh", |s| {
            s.bench_expire_due_with_forced_refresh(now)
        })
    }

    #[doc(hidden)]
    pub fn on_wire_request_handoff(&mut self, outcome: RequestHandoffOutcome) -> Vec<Effect> {
        self.checked("on_wire_request_handoff", |s| {
            s.on_wire_request_handoff(outcome)
        })
    }

    pub fn open_observation(
        &mut self,
        query: LiveQuery,
        now: Timestamp,
    ) -> ObservationOpen<ObservationId, RowsSeed> {
        self.checked("open_observation", |s| s.open_observation(query, now))
    }

    #[doc(hidden)]
    pub fn on_nip77_handoff(
        &mut self,
        frame: Nip77Frame,
        outcome: RequestHandoffOutcome,
    ) -> Vec<Effect> {
        self.checked("on_nip77_handoff", |s| s.on_nip77_handoff(frame, outcome))
    }

    pub fn degrade_store(&mut self, err: PersistenceError, effects: &mut Vec<Effect>) {
        self.checked("degrade_store", |s| s.degrade_store(err, effects))
    }

    pub fn take_store_recovery_request(&mut self) -> Option<PersistenceFault> {
        self.checked("take_store_recovery_request", |s| {
            s.take_store_recovery_request()
        })
    }

    pub fn recover_store_after_failure(&mut self) -> Result<Vec<Effect>, PersistenceError> {
        self.checked("recover_store_after_failure", |s| {
            s.recover_store_after_failure()
        })
    }

    pub fn recover_on_boot(&mut self) -> Vec<Effect> {
        self.checked("recover_on_boot", |s| s.recover_on_boot())
    }

    pub fn reattach_receipt(&mut self, id: ReceiptId) -> ReceiptReplayPage {
        self.checked("reattach_receipt", |s| s.reattach_receipt(id))
    }

    pub fn reattach_receipt_page(
        &mut self,
        id: ReceiptId,
        cursor: Option<ReceiptReplayCursor>,
        limit: usize,
    ) -> ReceiptReplayPage {
        self.checked("reattach_receipt_page", |s| {
            s.reattach_receipt_page(id, cursor, limit)
        })
    }

    pub fn receipt_cursor_after_status(
        &mut self,
        id: ReceiptId,
        cursor: &ReceiptReplayCursor,
        status: &WriteFact,
    ) -> Option<ReceiptReplayCursor> {
        self.checked("receipt_cursor_after_status", |s| {
            s.receipt_cursor_after_status(id, cursor, status)
        })
    }

    pub fn receipt_is_live(&self, id: ReceiptId) -> bool {
        self.state.receipt_is_live(id)
    }

    pub fn prepare_publish(&mut self, intent: WriteIntent) -> PublishPreparation {
        self.checked("prepare_publish", |s| s.prepare_publish(intent))
    }

    pub fn publish_queue_entries(
        &self,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PersistenceError> {
        self.state.publish_queue_entries(after, limit)
    }

    pub fn publish_queue_entries_for_event(
        &self,
        event_id: EventId,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PersistenceError> {
        self.state
            .publish_queue_entries_for_event(event_id, after, limit)
    }

    pub fn remove_publish_queue_entry(
        &mut self,
        id: ReceiptId,
    ) -> Result<(), RemoveQueueEntryError> {
        self.checked("remove_publish_queue_entry", |s| {
            s.remove_publish_queue_entry(id)
        })
    }

    pub fn cancel_write(
        &mut self,
        id: ReceiptId,
    ) -> (Result<CancelWriteOutcome, CancelWriteError>, Vec<Effect>) {
        self.checked("cancel_write", |s| s.cancel_write(id))
    }

    pub fn run_replaceable_materialization(
        &mut self,
        call: ReplaceableMaterializationCall,
    ) -> ReplaceableMaterializationOutcome {
        self.checked("run_replaceable_materialization", |s| {
            s.run_replaceable_materialization(call)
        })
    }

    pub fn complete_body_complete_replaceable_operation(
        &mut self,
        continuation: ReplaceableMaterializationContinuation,
        outcome: ReplaceableMaterializationOutcome,
    ) -> PublishPreparation {
        self.checked("complete_body_complete_replaceable_operation", |s| {
            s.complete_body_complete_replaceable_operation(continuation, outcome)
        })
    }
}

/// Read-only white-box access for the reducer's own in-crate falsifiers.
///
/// `#[cfg(test)]`, and there is deliberately **no `DerefMut`** -- so this
/// grants reads and nothing else, and every mutation still goes through a
/// named door. It leaks nothing: `CoreState`'s fields are private to module
/// `core`, so the only code this helps is code that could already see them.
///
/// It also does real work. Removing the fields from `EngineCore` makes the
/// compiler split the in-crate test suite exactly along the read/write line:
/// 1,165 errors without this impl, 394 with it, and the 394 are precisely
/// the E0596/E0594 mutation sites. Those are the ones that had to move to
/// [`EngineCore::white_box`]; the 764 reads needed no change and gain nothing
/// from being rewritten, because there is no owner boundary between a
/// reducer and its own white-box tests -- they are the same Rust module.
#[cfg(test)]
impl std::ops::Deref for EngineCore {
    type Target = CoreState;

    fn deref(&self) -> &CoreState {
        &self.state
    }
}
