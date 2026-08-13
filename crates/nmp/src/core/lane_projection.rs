//! The one reducer-owned door for durable lane projection.
//!
//! Store operations remain authoritative. This module consumes their exact
//! committed post-state and keeps the rebuildable in-memory worker projection
//! synchronized before ordinary effect dispatch can ask which sessions remain
//! owned.

use super::*;

impl<S: EventStore> EngineCore<S> {
    /// Replace one intent's projection from a complete recovered lane set.
    ///
    /// Bootstrap returns every retained lane for the intent, so this is an
    /// exact rebuild rather than an incremental merge. Keeping the reverse
    /// wake index in the same door prevents its membership from drifting from
    /// the per-intent projection.
    fn replace_lane_projection(&mut self, id: ReceiptId, lanes: &[PublishQueueLane]) {
        if !self.pending.contains_key(&id) {
            self.lane_projection_unprovable = true;
            return;
        }
        let next = LaneWorkerProjection::from_recovered(lanes);
        let previous = self
            .pending
            .get(&id)
            .map(|pending| pending.lane_projection.persisted.clone())
            .unwrap_or_default();

        for relay in previous.difference(&next.persisted) {
            if let Some(receipts) = self.receipts_by_lane_relay.get_mut(relay) {
                receipts.remove(&id);
                if receipts.is_empty() {
                    self.receipts_by_lane_relay.remove(relay);
                }
            }
        }
        for relay in next.persisted.difference(&previous) {
            self.receipts_by_lane_relay
                .entry(relay.clone())
                .or_default()
                .insert(id);
        }
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.lane_projection = next;
        }
    }

    /// Apply one successful store mutation's exact post-state.
    fn apply_committed_lane(&mut self, lane: &PublishQueueLane) {
        let Some(id) = self.intent_receipts.get(&lane.key.intent_id).copied() else {
            self.lane_projection_unprovable = true;
            return;
        };
        let Some(pending) = self.pending.get_mut(&id) else {
            self.lane_projection_unprovable = true;
            return;
        };
        let newly_persisted = pending.lane_projection.apply(lane);
        if newly_persisted {
            self.receipts_by_lane_relay
                .entry(lane.key.relay.clone())
                .or_default()
                .insert(id);
        }
    }

    /// Preserve a possible lane/transition after an indeterminate commit.
    ///
    /// This deliberately produces a superset. A false-positive worker can be
    /// retired after explicit recovery; a false-negative can strand a durable
    /// obligation forever.
    fn mark_lane_projection_uncertain(&mut self, key: &PublishQueueLaneKey) {
        let Some(id) = self.intent_receipts.get(&key.intent_id).copied() else {
            self.lane_projection_unprovable = true;
            return;
        };
        let Some(pending) = self.pending.get_mut(&id) else {
            self.lane_projection_unprovable = true;
            return;
        };
        let newly_persisted = pending.lane_projection.mark_uncertain(key.relay.clone());
        if newly_persisted {
            self.receipts_by_lane_relay
                .entry(key.relay.clone())
                .or_default()
                .insert(id);
        }
    }

    fn commit_lane_transition<T>(
        &mut self,
        key: &PublishQueueLaneKey,
        operation: impl FnOnce(&mut S) -> Result<(T, PublishQueueLane), PersistenceError>,
    ) -> Result<(T, PublishQueueLane), PersistenceError> {
        let result = operation(self.resolver.store_mut());
        match result {
            Ok((value, lane)) => {
                self.apply_committed_lane(&lane);
                Ok((value, lane))
            }
            Err(error) => {
                if error.durability() == DurabilityOutcome::Unknown {
                    self.mark_lane_projection_uncertain(key);
                }
                Err(error)
            }
        }
    }

    /// Establish (or re-establish) one intent's projection from the durable
    /// lane set, creating the lanes its recorded route revisions imply.
    ///
    /// `candidate_relays` is what the caller can prove about the intent's
    /// durable route set. `None` means it could not be read at all — the
    /// caller has nothing to hold conservatively, so the whole projection
    /// reports unavailable until a retry commits.
    pub(super) fn bootstrap_projected_lanes(
        &mut self,
        intent_id: IntentId,
        candidate_relays: Option<&BTreeSet<RelayUrl>>,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        let result = self
            .resolver
            .store_mut()
            .bootstrap_publish_queue_lanes(intent_id);
        match result {
            Ok(lanes) => {
                if let Some(id) = self.intent_receipts.get(&intent_id).copied() {
                    self.replace_lane_projection(id, &lanes);
                    // The exact rebuild above supersedes every conservative
                    // guess this gap was standing in for, including any
                    // `uncertain` relay it left behind. This is the one place
                    // a bootstrap gap is allowed to close.
                    self.lane_bootstrap_retries.remove(&id);
                } else {
                    self.lane_projection_unprovable = true;
                }
                Ok(lanes)
            }
            Err(error) => {
                // Bootstrap is both a create-if-missing mutation and the only
                // complete read used to establish the projection. Even an
                // Absent mutation outcome does not prove that older lanes were
                // absent, so every route candidate remains conservatively
                // owned until a retry commits.
                for relay in candidate_relays.into_iter().flatten() {
                    self.mark_lane_projection_uncertain(&PublishQueueLaneKey {
                        intent_id,
                        relay: relay.clone(),
                    });
                }
                self.schedule_lane_bootstrap_retry(intent_id, candidate_relays);
                Err(error)
            }
        }
    }

    /// Record (or re-arm) the retryable gap left by a failed bootstrap.
    ///
    /// The conservative retention taken at the failure is only safe because
    /// this exists: `uncertain` can be cleared solely by a committed lane
    /// fact for that exact relay, and for an intent with no lane rows no
    /// other path in the reducer can ever produce one.
    pub(super) fn schedule_lane_bootstrap_retry(
        &mut self,
        intent_id: IntentId,
        candidates: Option<&BTreeSet<RelayUrl>>,
    ) {
        let Some(id) = self.intent_receipts.get(&intent_id).copied() else {
            // No receipt owns this intent, so there is no pending write to
            // retry on behalf of and nothing that could later close the gap.
            self.lane_projection_unprovable = true;
            return;
        };
        let entry = self
            .lane_bootstrap_retries
            .entry(id)
            .or_insert(LaneBootstrapRetry {
                candidates: Some(BTreeSet::new()),
                due: self.clock,
                failures: 0,
            });
        // Unknown is sticky and unions are conservative: a later failure that
        // happens to know its candidates must not shrink an earlier gap or
        // upgrade an unreadable route set into a covered one.
        entry.candidates = match (entry.candidates.take(), candidates) {
            (Some(mut known), Some(more)) => {
                known.extend(more.iter().cloned());
                Some(known)
            }
            _ => None,
        };
        entry.failures = entry.failures.saturating_add(1);
        entry.due = self.clock + bootstrap_retry_delay_secs(entry.failures);
    }

    pub(super) fn commit_lane_waiting(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .set_lane_waiting(key, revision, auth)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(super) fn commit_lane_eligible(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        since: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .set_lane_eligible(key, revision, since)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_lane_transient(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .set_lane_transient(key, revision, ordinal, eligible_at, cause, raw_reason)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_lane_suspension(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .suspend_lane_attempt(key, revision, ordinal, at, cause, raw_reason, auth)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(super) fn commit_lane_attempt_start(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        event: SignedEvent,
        started_at: Timestamp,
    ) -> Result<(nmp_store::PublishQueueAttempt, PublishQueueLane), PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store.start_lane_attempt(key, revision, event, started_at)
        })
    }

    pub(super) fn commit_lane_handoff(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        detail: PublishQueueAttemptHandoff,
        next: PublishQueuePostHandoffState,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .record_lane_handoff(key, revision, ordinal, detail, next)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(super) fn commit_lane_attempt_finish(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        outcome: PublishQueueAttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(super) fn commit_lane_auth_denied(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        denial: StoredAuthDenial,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .deny_lane_auth(key, revision, denial)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    /// Append a durable route revision through the projection door.
    ///
    /// A revision mints no lane by itself today: its paired
    /// [`Self::bootstrap_projected_lanes`] is what returns the committed
    /// `PublishQueueLane` set, so this applies no projection delta and the
    /// caller's own route-blocked bookkeeping already retains worker demand
    /// when the append fails.
    ///
    /// It is nonetheless a door rather than a direct `store_mut()` call
    /// because under #975 `Auto` re-executes its strategy at every send
    /// opportunity and appends a revision whenever resolution learns
    /// something new — at which point lane minting moves onto this path. The
    /// door plus the enumeration falsifier is what makes that future change
    /// fail mechanically instead of silently projecting nothing.
    pub(super) fn commit_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<nmp_store::PublishQueueRouteRevision, PersistenceError> {
        self.resolver
            .store_mut()
            .record_route_revision(intent_id, relays)
    }

    /// Close one intent's open work through the projection door.
    ///
    /// The store door validates the all-terminal invariant transactionally,
    /// so the projection contributes no precondition of its own. A failure
    /// changes nothing: the caller keeps the pending write, and with it every
    /// relay the projection still owns, rather than retiring a worker on an
    /// unproven close.
    pub(super) fn commit_terminal_close(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        self.resolver.store_mut().close_terminal_intent(intent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_store::{PersistenceFault, RedbStore};
    use nostr::{Keys, Kind};
    use std::time::Instant;

    /// Accept and sign one durable private write, which routes and bootstraps
    /// its lanes.
    fn publish_to<S: EventStore>(
        core: &mut EngineCore<S>,
        author: &Keys,
        relays: &[RelayUrl],
        created_at: u64,
    ) -> (ReceiptId, SignedEvent, Vec<Effect>) {
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(
                nmp_grammar::EventBuilder::new(Kind::TextNote)
                    .content(format!("worker projection {created_at}"))
                    .created_at(Timestamp::from(created_at)),
            ),
            routing: WriteRouting::Explicit(relays.to_vec()),
            identity: Identity::Active,
            correlation: None,
        }));
        let (id, generation, unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(id, generation, unsigned) => {
                    Some((*id, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("accepted write requests signing");
        let signed = unsigned.sign_with_keys(author).expect("sign fixture");
        let effects = core.handle(EngineMsg::SignerCompleted(
            id,
            generation,
            Ok(signed.clone()),
        ));
        (id, signed, effects)
    }

    fn publish_waiting<S: EventStore>(
        core: &mut EngineCore<S>,
        author: &Keys,
        relay: &RelayUrl,
        created_at: u64,
    ) -> (ReceiptId, SignedEvent) {
        let (id, signed, effects) =
            publish_to(core, author, std::slice::from_ref(relay), created_at);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session)
                if session == &RelaySessionKey::new(
                    relay.clone(),
                    AccessContext::Nip42(author.public_key())
                )
        )));
        (id, signed)
    }

    /// Independent semantic oracle: reconstruct the old exact answer from
    /// canonical store rows plus the reducer's non-lane transient owners.
    /// This deliberately does not inspect `LaneWorkerProjection`.
    fn durable_worker_oracle<S: EventStore>(core: &EngineCore<S>) -> BTreeSet<RelaySessionKey> {
        let mut expected: BTreeSet<_> = core
            .attempt_correlations
            .values()
            .map(|target| target.session.clone())
            .collect();
        for pending in core.pending.values() {
            let access = AccessContext::Nip42(pending.signing_pubkey);
            expected.extend(
                pending
                    .pending_relays
                    .iter()
                    .chain(&pending.unstarted_relays)
                    .chain(&pending.route_blocked_relays)
                    .cloned()
                    .map(|relay| RelaySessionKey::new(relay, access)),
            );
            expected.extend(
                core.resolver
                    .store()
                    .recover_publish_queue_lanes(pending.intent_id)
                    .expect("oracle lane recovery")
                    .into_iter()
                    .filter(|lane| !matches!(lane.state, PublishQueueLaneState::Terminal { .. }))
                    .map(|lane| RelaySessionKey::new(lane.key.relay, access)),
            );
        }
        expected
    }

    fn assert_projection_matches_store<S: EventStore>(core: &EngineCore<S>) {
        let actual = core
            .relay_worker_requirements()
            .expect("projection remains available")
            .writes;
        assert_eq!(actual, durable_worker_oracle(core));
    }

    #[test]
    fn projection_matches_durable_state_after_every_normal_publish_queue_transition() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-lifecycle.example.com").unwrap();
        let session =
            RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author.public_key()));
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);

        let (receipt, signed) = publish_waiting(&mut core, &author, &relay, 1);
        assert_projection_matches_store(&core);

        let handle = TransportRelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        assert_projection_matches_store(&core);

        let scheduled = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
        assert_projection_matches_store(&core);
        let correlation = scheduled
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(candidate, _, correlation) if candidate == &session => {
                    Some(*correlation)
                }
                _ => None,
            })
            .expect("eligible lane starts one attempt");

        core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
        assert_projection_matches_store(&core);

        core.handle(EngineMsg::RelayFrame(
            handle,
            session,
            RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
        ));
        assert_projection_matches_store(&core);
        assert!(
            !core.pending.contains_key(&receipt),
            "the exact terminal projection allows the store-validated close"
        );
    }

    #[test]
    fn same_url_keeps_distinct_signing_identities_in_worker_demand() {
        let author_a = Keys::generate();
        let author_b = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-identity.example.com").unwrap();
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);

        publish_waiting(&mut core, &author_a, &relay, 10);
        publish_waiting(&mut core, &author_b, &relay, 11);
        assert_projection_matches_store(&core);

        let actual = core.relay_worker_requirements().unwrap().writes;
        assert_eq!(
            actual,
            BTreeSet::from([
                RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author_a.public_key())),
                RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author_b.public_key())),
            ])
        );
        assert!(!actual.contains(&RelaySessionKey::public(relay)));
    }

    #[test]
    fn close_reopen_rebuilds_the_same_exact_worker_projection() {
        let author_a = Keys::generate();
        let author_b = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-restart.example.com").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker-projection.redb");
        let expected = BTreeSet::from([
            RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author_a.public_key())),
            RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author_b.public_key())),
        ]);

        {
            let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
            publish_waiting(&mut core, &author_a, &relay, 20);
            publish_waiting(&mut core, &author_b, &relay, 21);
            assert_projection_matches_store(&core);
            assert_eq!(core.relay_worker_requirements().unwrap().writes, expected);
        }

        let mut recovered = EngineCore::new(RedbStore::open(&path).unwrap(), 10);
        let effects = recovered.recover_on_boot();
        assert_projection_matches_store(&recovered);
        assert_eq!(
            recovered.relay_worker_requirements().unwrap().writes,
            expected
        );
        for session in expected {
            assert!(effects.iter().any(
                |effect| matches!(effect, Effect::EnsureWriteRelay(candidate) if candidate == &session)
            ));
        }
    }

    #[test]
    fn durability_unknown_marks_the_lane_uncertain_and_retains_its_worker() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-unknown.example.com").unwrap();
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
        let (receipt, _) = publish_waiting(&mut core, &author, &relay, 30);
        let key = PublishQueueLaneKey {
            intent_id: core.pending[&receipt].intent_id,
            relay: relay.clone(),
        };

        let result: Result<((), PublishQueueLane), PersistenceError> =
            core.commit_lane_transition(&key, |_store| {
                Err(PersistenceError::new(
                    PersistenceFault::Io,
                    "injected indeterminate commit",
                ))
            });
        assert_eq!(result.unwrap_err().durability(), DurabilityOutcome::Unknown);
        assert!(core.pending[&receipt]
            .lane_projection
            .uncertain
            .contains(&relay));
        assert!(core
            .relay_worker_requirements()
            .unwrap()
            .writes
            .contains(&RelaySessionKey::new(
                relay,
                AccessContext::Nip42(author.public_key())
            )));
    }

    #[test]
    fn durability_absent_leaves_the_exact_projection_unchanged() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-absent.example.com").unwrap();
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 10);
        let (receipt, _) = publish_waiting(&mut core, &author, &relay, 31);
        let key = PublishQueueLaneKey {
            intent_id: core.pending[&receipt].intent_id,
            relay,
        };
        let before = core.pending[&receipt].lane_projection.clone();

        let result: Result<((), PublishQueueLane), PersistenceError> =
            core.commit_lane_transition(&key, |_store| {
                Err(PersistenceError::invariant(
                    "injected known-absent transition",
                ))
            });

        assert_eq!(result.unwrap_err().durability(), DurabilityOutcome::Absent);
        assert_eq!(core.pending[&receipt].lane_projection, before);
        assert!(core.lane_worker_projection_available());
    }

    /// Reduce Rust source to what the census may match against: comments
    /// removed, then all whitespace stripped.
    ///
    /// Removing comments is what stops a commented-out call from failing the
    /// build spuriously and a real call from hiding behind a trailing
    /// comment. String literals are tracked so a `wss://relay` URL is not
    /// mistaken for the start of a line comment, and `'x'` is distinguished
    /// from a `'lifetime` so a quote character literal cannot desynchronize
    /// the scan.
    fn searchable(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < chars.len() {
            let next = chars.get(i + 1).copied();
            match chars[i] {
                '/' if next == Some('/') => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                }
                '/' if next == Some('*') => {
                    let mut depth = 1usize;
                    i += 2;
                    while i < chars.len() && depth > 0 {
                        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                            depth += 1;
                            i += 2;
                        } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                            depth -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
                '"' => {
                    i += 1;
                    while i < chars.len() {
                        if chars[i] == '\\' {
                            i += 2;
                            continue;
                        }
                        let closing = chars[i] == '"';
                        i += 1;
                        if closing {
                            break;
                        }
                    }
                }
                '\'' if chars.get(i + 2) == Some(&'\'')
                    || (next == Some('\\') && chars.get(i + 3) == Some(&'\'')) =>
                {
                    i += if next == Some('\\') { 4 } else { 3 };
                }
                c => {
                    if !c.is_whitespace() {
                        out.push(c);
                    }
                    i += 1;
                }
            }
        }
        out
    }

    /// The projection wrapper is an API mechanism, and this census is its
    /// falsifier. Both halves are DERIVED from source rather than from a list
    /// someone has to remember to extend, because an enumeration written
    /// against today's call sites is exactly the failure mode #985's
    /// sequencing comment warns about.
    ///
    /// 1. **Nothing is missing from the enumeration.** Every `EventStore`
    ///    door that takes `&mut self` and deals in `PublishQueueLane` is a
    ///    lane-mutation constructor, scraped straight out of `nmp-store`'s
    ///    own trait; adding one there without giving it a `commit_*` door in
    ///    this module fails here. Two further constructors never mention
    ///    `PublishQueueLane` in their signature and are therefore named
    ///    explicitly: `close_terminal_intent`, which removes an intent's open
    ///    work wholesale, and `record_route_revision`.
    ///
    ///    `record_route_revision` is the one the second #985 design comment
    ///    singles out. Today a revision mints no lane by itself -- its paired
    ///    `bootstrap_publish_queue_lanes` does -- so an enumeration written against
    ///    today's call sites would look complete and go silently incomplete
    ///    the moment #975 lands, because `Auto` re-executes its strategy at
    ///    EVERY send opportunity and appends a revision whenever resolution
    ///    learns something new, minting lanes through this projection's own
    ///    door.
    ///
    /// 2. **Nothing bypasses the door.** No file in `crates/nmp/src` -- the
    ///    whole crate, not just `src/core` -- other than this one may reach a
    ///    lane-mutation door; only a file that implements `EventStore` itself
    ///    (a delegating test double) may name one. Matching is on `.method(`
    ///    rather than on `store_mut().method(`, so binding the store to a
    ///    local first does not evade it. Comments are removed and whitespace
    ///    stripped before matching, so rustfmt's multi-line
    ///    `self\n.resolver\n.store_mut()\n.set_lane_waiting(` chain is caught
    ///    exactly like the single-line spelling, while a commented-out call
    ///    neither fails the build spuriously nor hides a real one behind a
    ///    trailing comment.
    #[test]
    fn every_lane_mutation_constructor_goes_through_the_projection_door() {
        /// Lane-minting/removing doors whose signature does not mention
        /// `PublishQueueLane`, so the scrape below cannot find them.
        const NAMED_EXPLICITLY: &[&str] = &["record_route_revision", "close_terminal_intent"];

        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let store_source = std::fs::read_to_string(manifest.join("../nmp-store/src/lib.rs"))
            .expect("read nmp-store/src/lib.rs");

        let mut doors: Vec<String> = Vec::new();
        for chunk in store_source.split("\n    fn ").skip(1) {
            let name = chunk.split('(').next().unwrap_or_default();
            let signature = chunk.split('{').next().unwrap_or_default();
            if signature.contains("&mut self") && signature.contains("PublishQueueLane") {
                doors.push(name.to_string());
            }
        }
        assert!(
            doors.len() >= 8,
            "the signature scrape found only {} lane-mutation doors, which means \
             the scrape itself broke rather than the trait shrinking: {doors:?}",
            doors.len()
        );
        doors.extend(NAMED_EXPLICITLY.iter().map(|door| door.to_string()));

        // Half 1: this module owns a door for every one of them. Only the
        // production half counts, so the census above cannot satisfy itself.
        let projection_file = manifest.join("src/core/lane_projection.rs");
        let projection: String =
            std::fs::read_to_string(&projection_file).expect("read lane_projection.rs");
        let projection = searchable(
            projection
                .split("#[cfg(test)]")
                .next()
                .expect("lane_projection.rs has a production half"),
        );
        for door in &doors {
            assert!(
                projection.contains(&format!(".{door}(")),
                "`EventStore::{door}` mutates lane state but core/lane_projection.rs \
                 has no `commit_*` door for it -- every engine lane mutation must be \
                 funnelled so its committed `PublishQueueLane` updates the projection"
            );
        }

        // Half 2: nobody else reaches those doors around the projection.
        let mut offenders: Vec<String> = Vec::new();
        let mut inspected = 0usize;
        let mut stack = vec![manifest.join("src")];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory).expect("read engine source directory") {
                let path = entry.expect("read engine source entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs")
                    || path == projection_file
                {
                    continue;
                }
                inspected += 1;
                let source = std::fs::read_to_string(&path).expect("read engine source");
                let squeezed = searchable(&source);
                let delegating_double = squeezed.contains("EventStorefor");
                for door in &doors {
                    if squeezed.contains(&format!("store_mut().{door}(")) {
                        offenders.push(format!("{} calls store_mut().{door}", path.display()));
                    } else if !delegating_double && squeezed.contains(&format!(".{door}(")) {
                        offenders.push(format!(
                            "{} names .{door} outside the projection door",
                            path.display()
                        ));
                    }
                }
            }
        }
        assert!(
            inspected > 10,
            "the source walk inspected only {inspected} files; the walk broke, not the crate"
        );
        assert!(
            offenders.is_empty(),
            "every engine lane mutation must go through the reducer-owned door in \
             core/lane_projection.rs, so the committed `PublishQueueLane` can update the \
             worker projection. Bypasses found: {offenders:#?}"
        );
    }

    /// The reproducible before/after for #985's own claim, run on demand:
    ///
    /// ```text
    /// cargo test --release -p nmp --lib measure_worker_demand_cost -- --ignored --nocapture
    /// ```
    ///
    /// It measures BOTH bodies against the SAME populated `RedbStore` in the
    /// same process, so the comparison is not across builds or revisions:
    /// [`EngineCore::write_relay_workers`] (the projection) versus
    /// [`durable_worker_oracle`] (the old body, kept verbatim as this
    /// module's semantic oracle). `#[ignore]`d because it builds a real
    /// on-disk store with hundreds of intents and reports a wall-clock
    /// number, neither of which belongs in the ordinary suite.
    ///
    /// Lane-read counts are not instrumented here: the before body performs
    /// exactly one `recover_publish_queue_lanes` per pending intent per pass by
    /// construction, and the after body's zero is pinned by
    /// `unchanged_worker_demand_reads_zero_publish_queue_lanes` in
    /// `tests/core_headless`.
    ///
    /// This is NOT the Mosaico-shaped end-to-end profile #985 also asks for;
    /// it measures exactly the one calculation this change replaces.
    /// Whole-process CPU remains unmeasured and must not be claimed from it.
    #[test]
    #[ignore = "manual before/after performance qualification"]
    fn measure_worker_demand_cost() {
        const INTENTS: usize = 200;
        const RELAYS_PER_INTENT: usize = 3;
        const PASSES: usize = 500;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("measure-worker-demand.redb");
        let mut core = EngineCore::new(RedbStore::open(&path).unwrap(), INTENTS + 1);

        for i in 0..INTENTS {
            let author = Keys::generate();
            let relays: Vec<RelayUrl> = (0..RELAYS_PER_INTENT)
                .map(|r| {
                    RelayUrl::parse(&format!(
                        "wss://measure-{}.example.com",
                        i * RELAYS_PER_INTENT + r
                    ))
                    .unwrap()
                })
                .collect();
            publish_to(&mut core, &author, &relays, 10_000 + i as u64);
        }

        let started = Instant::now();
        let mut projected = 0usize;
        for _ in 0..PASSES {
            projected += std::hint::black_box(core.write_relay_workers()).len();
        }
        let after = started.elapsed();

        let started = Instant::now();
        let mut reconstructed = 0usize;
        for _ in 0..PASSES {
            reconstructed += std::hint::black_box(durable_worker_oracle(&core)).len();
        }
        let before = started.elapsed();

        assert_eq!(
            projected, reconstructed,
            "the two bodies must agree before their cost is compared"
        );
        println!(
            "measure_worker_demand_cost intents={INTENTS} x {RELAYS_PER_INTENT} relays, \
             {PASSES} worker-demand passes (RedbStore)\n  \
             BEFORE (per-intent recover_publish_queue_lanes): {before:?}, \
             {} lane reads\n  AFTER  (reducer projection): {after:?}, 0 lane reads\n  \
             speedup: {:.1}x",
            INTENTS * PASSES,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }
}
