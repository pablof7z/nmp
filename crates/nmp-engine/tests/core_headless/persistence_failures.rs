use super::*;

// ---- fallible persistence doors and recovery indexing ------------------
//
// A fault-injecting `EventStore` whose ONE mutating ingest door (`insert`)
// returns a `PersistenceError` (a stand-in for disk-full / an I/O error on
// the real redb backend) while every OTHER door delegates to a healthy
// in-memory store. This isolates the ingest failure so the falsifiers below
// prove (a) the door surfaces `Err` rather than panicking, and (b) the
// engine degrades the local cache to read-only and emits a diagnostic
// instead of crashing the host app on a relay EVENT frame.
struct FailIngestStore {
    inner: MemoryStore,
    fail_insert: bool,
}

impl FailIngestStore {
    fn armed() -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_insert: true,
        }
    }
}

impl EventStore for FailIngestStore {
    fn compensate_write_with_state(
        &mut self,
        intent_id: nmp_store::IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write_with_state(intent_id, reason)
    }
    fn cancel_ephemeral_receipt(
        &mut self,
        receipt_id: u64,
    ) -> Result<CancelEphemeralOutcome, PersistenceError> {
        self.inner.cancel_ephemeral_receipt(receipt_id)
    }
    fn mark_ephemeral_signed(&mut self, receipt_id: u64) -> Result<bool, PersistenceError> {
        self.inner.mark_ephemeral_signed(receipt_id)
    }
    fn insert(
        &mut self,
        event: nostr::Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        if self.fail_insert {
            return Err(PersistenceError("injected ingest I/O failure".into()));
        }
        self.inner.insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query(filter)
    }
    fn remove(
        &mut self,
        id: nostr::EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.remove(id, reason)
    }
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.expire_due(now)
    }
    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.next_expiration()
    }
    fn record_coverage(
        &mut self,
        atom: &nmp_grammar::ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(atom, relay, proven)
    }
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.inner.get_coverage(key, relay)
    }
    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }
    fn promote_signed(
        &mut self,
        intent_id: nmp_store::IntentId,
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, sig)
    }
    fn compensate_write(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write(intent_id)
    }
    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        self.inner.recover_outbox()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }
    fn record_route_revision(
        &mut self,
        intent_id: nmp_store::IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }
    fn recover_route_revisions(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: nostr::EventId,
        expected_pubkey: nostr::PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

/// Door-level falsifier (issue #122): the `insert` ingest door surfaces a
/// realistic persistence I/O failure as `Err(PersistenceError)` rather than
/// panicking. `MemoryStore` never fails, so the fault is entirely the
/// injected one — this is the exact contract the redb backend now honors via
/// `.map_err(persist_err)?` on every real redb operation.
#[test]
fn ingest_door_surfaces_io_failure_as_persistence_error_not_panic() {
    let a = Keys::generate();
    let mut store = FailIngestStore::armed();
    let event = nmp_resolver::testkit::kind1(&a, "disk is full", 1_000);
    let from = RelayObserved::new(
        RelayUrl::parse("wss://relay.example.com").unwrap(),
        Timestamp::from(1_000u64),
    );
    let outcome = store.insert(event, from);
    assert!(
        matches!(outcome, Err(PersistenceError(_))),
        "an ingest-path I/O failure must surface as Err(PersistenceError), got {outcome:?}"
    );
}

/// Engine-level falsifier (issue #122): a relay EVENT frame whose store
/// `insert` fails on I/O DEGRADES the engine to read-only (a `store_degraded`
/// diagnostic is emitted) and never panics the reducer. The failed frame
/// delivers no phantom rows, and the engine stays usable for later messages.
#[test]
fn ingest_io_failure_degrades_read_only_without_panicking() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    // `query`/coverage doors stay healthy; only `insert` fails — so the
    // subscribe/connect setup below (which reads, never inserts) succeeds,
    // proving the degrade is specific to the failing ingest door.
    let mut core = EngineCore::new(FailIngestStore::armed(), Box::new(dir), 10);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));
    let _ = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
    ));

    // The real relay ingest path — the exact call that used to `.expect()`
    // panic on a disk-full redb `insert`.
    let event = nmp_resolver::testkit::kind1(&a, "disk is full", 1_000);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay),
        event_frame("s", event),
    ));

    // Degrade, don't panic: the read-only signal reaches the diagnostics
    // surface.
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitDiagnostics(snap) if snap.store_degraded.is_some())),
        "an ingest I/O failure must surface a `store_degraded` diagnostic, got {effects:?}"
    );
    // A failed ingest fabricates no rows.
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "a failed ingest must not deliver phantom rows, got {effects:?}"
    );
    // The reducer survives and keeps handling messages (no poisoned state).
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(1u64)));
}

// ---- epic #507 finding E5: wake_relay_lanes lane-relay index -----------
//
// `EngineCore::recover_all_lanes` used to be the ONLY way `wake_relay_lanes`
// (called on every relay connect/disconnect/auth event) could find a
// relay's lanes: a full `O(pending)` store re-read, filtered down to one
// relay afterward, and then run a SECOND time inside `schedule_ready` at the
// end of the same call. The fix adds two reducer-owned indexes
// (`intent_receipts`, `receipts_by_lane_relay`) so a single relay event only
// re-reads the intents actually routed through that relay, with a
// `lane_relay_index_degraded` safety valve that falls back to the exact old
// full-scan behavior whenever the index cannot be proven complete. The
// falsifiers below exercise both the narrow path and the degraded fallback.

/// Instrumented double for finding E5: counts `recover_outbox_lanes` calls
/// through a caller-shared counter (so a test can inspect it after the
/// store has been moved into `EngineCore`), and can be configured to fail
/// `bootstrap_outbox_lanes` exactly once to exercise the degraded-mode
/// safety valve.
struct WakeLaneProbeStore {
    inner: MemoryStore,
    recover_outbox_lanes_calls: Rc<Cell<u64>>,
    fail_next_bootstrap: bool,
}

impl WakeLaneProbeStore {
    fn new(recover_outbox_lanes_calls: Rc<Cell<u64>>) -> Self {
        Self {
            inner: MemoryStore::new(),
            recover_outbox_lanes_calls,
            fail_next_bootstrap: false,
        }
    }

    fn with_failing_bootstrap(recover_outbox_lanes_calls: Rc<Cell<u64>>) -> Self {
        Self {
            inner: MemoryStore::new(),
            recover_outbox_lanes_calls,
            fail_next_bootstrap: true,
        }
    }
}

impl EventStore for WakeLaneProbeStore {
    fn compensate_write_with_state(
        &mut self,
        intent_id: nmp_store::IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write_with_state(intent_id, reason)
    }
    fn cancel_ephemeral_receipt(
        &mut self,
        receipt_id: u64,
    ) -> Result<CancelEphemeralOutcome, PersistenceError> {
        self.inner.cancel_ephemeral_receipt(receipt_id)
    }
    fn mark_ephemeral_signed(&mut self, receipt_id: u64) -> Result<bool, PersistenceError> {
        self.inner.mark_ephemeral_signed(receipt_id)
    }
    fn insert(
        &mut self,
        event: nostr::Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query(filter)
    }
    fn remove(
        &mut self,
        id: nostr::EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.remove(id, reason)
    }
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.expire_due(now)
    }
    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.next_expiration()
    }
    fn record_coverage(
        &mut self,
        atom: &nmp_grammar::ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(atom, relay, proven)
    }
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.inner.get_coverage(key, relay)
    }
    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }
    fn promote_signed(
        &mut self,
        intent_id: nmp_store::IntentId,
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, sig)
    }
    fn compensate_write(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write(intent_id)
    }
    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        self.inner.recover_outbox()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }
    fn record_route_revision(
        &mut self,
        intent_id: nmp_store::IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }
    fn recover_route_revisions(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    fn bootstrap_outbox_lanes(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<nmp_store::RecoveredLane>, PersistenceError> {
        if self.fail_next_bootstrap {
            self.fail_next_bootstrap = false;
            return Err(PersistenceError("injected bootstrap failure".to_string()));
        }
        self.inner.bootstrap_outbox_lanes(intent_id)
    }
    fn recover_outbox_lanes(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<nmp_store::RecoveredLane>, PersistenceError> {
        self.recover_outbox_lanes_calls
            .set(self.recover_outbox_lanes_calls.get() + 1);
        self.inner.recover_outbox_lanes(intent_id)
    }
    fn due_outbox_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<nmp_store::LaneDeadline>, PersistenceError> {
        self.inner.due_outbox_deadlines(now, limit)
    }
    fn next_outbox_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        self.inner.next_outbox_deadline()
    }
    fn set_lane_waiting(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        auth: bool,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        self.inner.set_lane_waiting(key, revision, auth)
    }
    fn set_lane_eligible(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        since: Timestamp,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        self.inner.set_lane_eligible(key, revision, since)
    }
    fn set_lane_transient(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: nmp_store::TransientCause,
        raw_reason: Option<String>,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        self.inner
            .set_lane_transient(key, revision, ordinal, eligible_at, cause, raw_reason)
    }
    fn suspend_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: nmp_store::TransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        self.inner
            .suspend_lane_attempt(key, revision, ordinal, at, cause, raw_reason, auth)
    }
    fn start_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        event: nostr::Event,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, nmp_store::RecoveredLane), PersistenceError> {
        self.inner
            .start_lane_attempt(key, revision, event, started_at)
    }
    fn record_lane_handoff(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        ordinal: u64,
        detail: nmp_store::AttemptHandoffDetail,
        next: nmp_store::PostHandoffState,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        self.inner
            .record_lane_handoff(key, revision, ordinal, detail, next)
    }
    fn finish_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        self.inner
            .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
    }
    fn recover_attempt_details(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<nmp_store::RecoveredAttemptDetails>, PersistenceError> {
        self.inner.recover_attempt_details(intent_id)
    }
    fn close_terminal_intent(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<nmp_store::CloseIntentOutcome, PersistenceError> {
        self.inner.close_terminal_intent(intent_id)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: nostr::EventId,
        expected_pubkey: nostr::PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

/// Falsifier (epic #507 finding E5): a single relay-connected event for
/// relay X must trigger `recover_outbox_lanes` only for X's own intent on
/// the wake path, not for every outstanding durable write. Composition of
/// the expected count: `schedule_ready`'s own `O(pending)` accounting is
/// UNCHANGED (deliberately -- see `recover_all_lanes`'s doc comment) and
/// reads all `N` pending intents once; the wake scan itself collapses from
/// `N` reads (the old `recover_all_lanes` + relay filter) down to exactly
/// `1` (only the receipt actually routed through the woken relay). Total:
/// `N + 1`, strictly less than the old `2 * N`.
#[test]
fn wake_relay_lanes_only_rereads_the_woken_relays_own_intent() {
    const N: usize = 3;
    let author = Keys::generate();
    let relays: Vec<RelayUrl> = (0..N)
        .map(|i| RelayUrl::parse(&format!("wss://wake-falsifier-{i}.example.com")).unwrap())
        .collect();

    let calls = Rc::new(Cell::new(0u64));
    let mut core = EngineCore::new(
        WakeLaneProbeStore::new(calls.clone()),
        Box::new(FixtureDirectory::new()),
        10,
    );
    activate(&mut core, &author);

    // N distinct durable writes, each routed to its OWN distinct relay, none
    // connected yet -- every one lands in `WaitingConnection`.
    for (i, relay) in relays.iter().enumerate() {
        let sink = CapturingReceiptSink::default();
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(
                    &author,
                    100 + i as u64,
                    &format!("falsifier {i}"),
                )),
                durability: Durability::Durable,
                routing: WriteRouting::PrivateNarrow(PrivateRoute {
                    relays: NarrowOnly::new([relay.clone()]),
                }),
                identity_override: None,
                correlation: None,
            },
            Box::new(sink),
        ));
        let (id, generation, u) = find_sign_request(&accepted);
        let signed = u.sign_with_keys(&author).unwrap();
        let _ = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    }

    // Reset the counter right before the event under test -- everything
    // above (N acceptances, each running its own `schedule_ready`) already
    // produced its own, unrelated `recover_outbox_lanes` traffic.
    let woken = relays[0].clone();
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&woken, author.public_key()),
    ));
    // The event under test is the bounded AUTH-discovery release (#8 U4):
    // connect itself now only parks the lane behind the probe; the wake that
    // actually publishes is `AuthProbeReleased`, with the same read
    // composition the old connect-time wake had.
    calls.set(0);
    let effects = core.handle(EngineMsg::AuthProbeReleased(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&woken, author.public_key()),
    ));

    assert_eq!(
        calls.get(),
        (N as u64) + 1,
        "expected exactly N ({N}) reads from schedule_ready's unchanged \
         durable-cap accounting plus 1 read from the wake scan (collapsed \
         from N) -- strictly less than the old 2*N={}",
        2 * N,
    );

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(r, _, _) if r == &signer_session(&woken, author.public_key()))),
        "the woken relay's own write must still actually wake and publish, got {effects:?}"
    );
}

/// Degraded-mode safety valve (epic #507 finding E5): when
/// `bootstrap_outbox_lanes` fails for one intent, the reverse index can no
/// longer be proven a superset of live lanes, so `wake_relay_lanes` must
/// fall back to the full `recover_all_lanes` scan rather than trust a
/// possibly-incomplete index. Proven two ways: an unrelated intent's lane
/// still correctly wakes and publishes (no missed wakeup), and the wake
/// event's `recover_outbox_lanes` call count matches the FULL-scan
/// composition rather than the narrower indexed one.
#[test]
fn degraded_index_falls_back_to_full_scan_and_never_misses_a_wakeup() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://wake-degraded.example.com").unwrap();

    let calls = Rc::new(Cell::new(0u64));
    let mut core = EngineCore::new(
        WakeLaneProbeStore::with_failing_bootstrap(calls.clone()),
        Box::new(FixtureDirectory::new()),
        10,
    );
    activate(&mut core, &author);

    // Intent #1: its `bootstrap_outbox_lanes` call is the injected failure
    // -- the reducer must degrade rather than pretend it has no lanes.
    let sink1 = CapturingReceiptSink::default();
    let accepted1 = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&author, 200, "degraded 1")),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([relay.clone()]),
            }),
            identity_override: None,
            correlation: None,
        },
        Box::new(sink1),
    ));
    let (id1, gen1, u1) = find_sign_request(&accepted1);
    let signed1 = u1.sign_with_keys(&author).unwrap();
    let signed_effects1 = core.handle(EngineMsg::SignerCompleted(id1, gen1, Ok(signed1)));
    assert!(
        signed_effects1.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteStatus::PersistenceBlocked(r))
                if *rid == id1 && r == &relay
        )),
        "the injected bootstrap failure must surface as PersistenceBlocked, got {signed_effects1:?}"
    );

    // Intent #2: an ordinary write to the SAME relay accepted right after --
    // `fail_next_bootstrap` is one-shot, so this one bootstraps normally and
    // the index DOES learn its lane.
    let sink2 = CapturingReceiptSink::default();
    let accepted2 = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&author, 201, "degraded 2")),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([relay.clone()]),
            }),
            identity_override: None,
            correlation: None,
        },
        Box::new(sink2),
    ));
    let (id2, gen2, u2) = find_sign_request(&accepted2);
    let signed2 = u2.sign_with_keys(&author).unwrap();
    let signed_effects2 = core.handle(EngineMsg::SignerCompleted(id2, gen2, Ok(signed2)));
    assert!(
        signed_effects2.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteStatus::AwaitingRelay { relay: r })
                if *rid == id2 && r == &relay
        )),
        "the second write must bootstrap normally and land in WaitingConnection, \
         got {signed_effects2:?}"
    );

    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, author.public_key()),
    ));
    // Same #8 U4 shift as `wake_relay_lanes_only_rereads_...`: the wake that
    // publishes is the bounded AUTH-discovery release, not connect itself.
    calls.set(0);
    let effects = core.handle(EngineMsg::AuthProbeReleased(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, author.public_key()),
    ));

    // No missed wakeup: intent #2's lane -- the only one the index could
    // ever have learned -- still wakes and publishes.
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(r, _, _) if r == &signer_session(&relay, author.public_key()))),
        "a degraded index must never cost a missed wakeup, got {effects:?}"
    );

    // Quantitative proof the FULL scan ran, not the narrow index: 2 pending
    // intents this event; the degraded wake reads both directly (2) plus
    // `schedule_ready`'s own unchanged full scan (2) = 4. The non-degraded
    // composition here would have been 1 (index has exactly 1 receipt for
    // this relay) + 2 (schedule_ready) = 3.
    assert_eq!(
        calls.get(),
        4,
        "expected the full-scan composition (2 wake + 2 schedule_ready), \
         proving the degraded flag drove this wake rather than the (here \
         incomplete) index"
    );
}

/// `receipt_for_intent` resolves correctly after `recover_on_boot` rebuilds
/// `intent_receipts` from scratch (epic #507 finding E5): two durable
/// writes, each on its own relay, are driven to `AwaitingAck` with
/// deliberately staggered deadlines before a simulated crash; after
/// reopening the store and recovering, each due deadline must still resolve
/// back to its OWN correct receipt id -- not the other's, and not silently
/// dropped (a broken index skips the status notification instead of
/// crashing, so this must be checked positively, not just for panics).
#[test]
fn receipt_for_intent_resolves_correctly_after_boot_recovery() {
    // Two DISTINCT authors: `publish_private` freezes a fixed (seq, content)
    // pair, so reusing one author for both calls on the same core would
    // freeze the identical event twice and collide as an exact duplicate
    // instead of creating two independent intents.
    let author_a = Keys::generate();
    let author_b = Keys::generate();
    let relay_a = RelayUrl::parse("wss://receipt-index-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://receipt-index-b.example.com").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipt-index.redb");

    let (receipt_a, receipt_b) = {
        let mut core = EngineCore::new(
            RedbStore::open(&path).unwrap(),
            Box::new(FixtureDirectory::new()),
            10,
        );
        connect_signer(&mut core, 0, &relay_a, author_a.public_key());
        connect_signer(&mut core, 1, &relay_b, author_b.public_key());
        release_author_probe(
            &mut core,
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            &relay_a,
            author_a.public_key(),
        );
        release_author_probe(
            &mut core,
            RelayHandle {
                slot: 1,
                generation: 1,
            },
            &relay_b,
            author_b.public_key(),
        );

        let _ = core.handle(EngineMsg::Tick(Timestamp::from(10)));
        let sink_a = CapturingReceiptSink::default();
        let (receipt_a, _event_a, scheduled_a) =
            publish_private(&mut core, &author_a, [relay_a.clone()], sink_a);
        mark_written(&mut core, &scheduled_a, &relay_a); // AckTimeout deadline = 10 + 30

        let _ = core.handle(EngineMsg::Tick(Timestamp::from(20)));
        let sink_b = CapturingReceiptSink::default();
        let (receipt_b, _event_b, scheduled_b) =
            publish_private(&mut core, &author_b, [relay_b.clone()], sink_b);
        mark_written(&mut core, &scheduled_b, &relay_b); // AckTimeout deadline = 20 + 30

        (receipt_a, receipt_b)
    };

    let mut core = EngineCore::new(
        RedbStore::open(&path).unwrap(),
        Box::new(FixtureDirectory::new()),
        10,
    );
    core.recover_on_boot();

    // relay_a's deadline (40) is due; relay_b's (50) is not yet.
    let effects_a = core.handle(EngineMsg::Tick(Timestamp::from(40)));
    assert!(
        effects_a.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteStatus::RetryEligible { relay, attempt: 1, .. })
                if *rid == receipt_a && relay == &relay_a
        )),
        "receipt_for_intent must resolve intent_a's due AckTimeout back to \
         receipt_a (not receipt_b, not silently dropped) after boot \
         recovery, got {effects_a:?}"
    );
    assert!(
        !effects_a.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteStatus::RetryEligible { relay, .. })
                if relay == &relay_b || *rid == receipt_b
        )),
        "relay_b's deadline is not yet due -- it must not fire early, got {effects_a:?}"
    );

    let effects_b = core.handle(EngineMsg::Tick(Timestamp::from(50)));
    assert!(
        effects_b.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteStatus::RetryEligible { relay, attempt: 1, .. })
                if *rid == receipt_b && relay == &relay_b
        )),
        "receipt_for_intent must resolve intent_b's due AckTimeout back to \
         receipt_b after boot recovery, got {effects_b:?}"
    );
}

/// `receipt_for_intent` for a still-open intent is unaffected by an
/// earlier, unrelated `pending` removal (epic #507 finding E5): closing one
/// durable write's obligation (a real removal, which walks
/// `forget_pending_indexes`) must not corrupt the `intent_receipts` entry
/// of a completely different, still-open write.
#[test]
fn receipt_for_intent_unaffected_by_an_earlier_pending_removal() {
    // Two DISTINCT authors, same reason as the boot-recovery test above:
    // `publish_private` freezes a fixed (seq, content) pair per call, so
    // reusing one author for both writes on the same core would collide as
    // an exact duplicate instead of creating two independent intents.
    let author1 = Keys::generate();
    let author2 = Keys::generate();
    let relay1 = RelayUrl::parse("wss://receipt-index-removal-1.example.com").unwrap();
    let relay2 = RelayUrl::parse("wss://receipt-index-removal-2.example.com").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    connect_signer(&mut core, 0, &relay1, author1.public_key());
    connect_signer(&mut core, 1, &relay2, author2.public_key());
    release_author_probe(
        &mut core,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &relay1,
        author1.public_key(),
    );
    release_author_probe(
        &mut core,
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        &relay2,
        author2.public_key(),
    );

    // Write #1: drive it all the way to a real, permanent `pending` removal
    // -- a successful ACK closes the intent once its one lane is terminal.
    let sink1 = CapturingReceiptSink::default();
    let (_receipt1, event1, first1) = publish_private(&mut core, &author1, [relay1.clone()], sink1);
    mark_written(&mut core, &first1, &relay1);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay1, event1.pubkey),
        RelayFrame::from(RelayMessage::ok(event1.id, true, "")),
    ));

    // Write #2: a completely separate, still-open intent.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(5)));
    let sink2 = CapturingReceiptSink::default();
    let (receipt2, _event2, first2) = publish_private(&mut core, &author2, [relay2.clone()], sink2);
    mark_written(&mut core, &first2, &relay2); // AckTimeout deadline = 5 + 30 = 35

    let effects = core.handle(EngineMsg::Tick(Timestamp::from(35)));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(rid, WriteStatus::RetryEligible { relay, attempt: 1, .. })
                if *rid == receipt2 && relay == &relay2
        )),
        "an earlier, unrelated pending removal (write #1's close) must not \
         corrupt receipt_for_intent's resolution of write #2's own due \
         deadline, got {effects:?}"
    );
}
