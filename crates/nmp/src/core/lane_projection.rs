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
    fn replace_lane_projection(&mut self, id: ReceiptId, lanes: &[RecoveredLane]) {
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
    fn apply_committed_lane(&mut self, lane: &RecoveredLane) {
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
    fn mark_lane_projection_uncertain(&mut self, key: &LaneKey) {
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
        key: &LaneKey,
        operation: impl FnOnce(&mut S) -> Result<(T, RecoveredLane), PersistenceError>,
    ) -> Result<(T, RecoveredLane), PersistenceError> {
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
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        let result = self.resolver.store_mut().bootstrap_outbox_lanes(intent_id);
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
                    self.mark_lane_projection_uncertain(&LaneKey {
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
        key: &LaneKey,
        revision: u64,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .set_lane_waiting(key, revision, auth)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(super) fn commit_lane_eligible(
        &mut self,
        key: &LaneKey,
        revision: u64,
        since: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
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
        key: &LaneKey,
        revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
    ) -> Result<RecoveredLane, PersistenceError> {
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
        key: &LaneKey,
        revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .suspend_lane_attempt(key, revision, ordinal, at, cause, raw_reason, auth)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(super) fn commit_lane_attempt_start(
        &mut self,
        key: &LaneKey,
        revision: u64,
        event: SignedEvent,
        started_at: Timestamp,
    ) -> Result<(nmp_store::RecoveredAttempt, RecoveredLane), PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store.start_lane_attempt(key, revision, event, started_at)
        })
    }

    pub(super) fn commit_lane_handoff(
        &mut self,
        key: &LaneKey,
        revision: u64,
        ordinal: u64,
        detail: AttemptHandoffDetail,
        next: PostHandoffState,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .record_lane_handoff(key, revision, ordinal, detail, next)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(super) fn commit_lane_attempt_finish(
        &mut self,
        key: &LaneKey,
        revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.commit_lane_transition(key, |store| {
            store
                .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_router::FixtureDirectory;
    use nmp_store::{MemoryStore, PersistenceFault, RedbStore};
    use nostr::{Keys, Kind};

    fn publish_waiting<S: EventStore>(
        core: &mut EngineCore<S>,
        author: &Keys,
        relay: &RelayUrl,
        created_at: u64,
    ) -> (ReceiptId, SignedEvent) {
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Unsigned(UnsignedEvent::new(
                author.public_key(),
                Timestamp::from(created_at),
                Kind::TextNote,
                Vec::new(),
                format!("worker projection {created_at}"),
            )),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([relay.clone()]),
            }),
            identity_override: None,
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
            if let Some(intent_id) = pending.intent_id {
                expected.extend(
                    core.resolver
                        .store()
                        .recover_outbox_lanes(intent_id)
                        .expect("oracle lane recovery")
                        .into_iter()
                        .filter(|lane| !matches!(lane.state, LaneState::Terminal { .. }))
                        .map(|lane| RelaySessionKey::new(lane.key.relay, access)),
                );
            }
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
    fn projection_matches_durable_state_after_every_normal_delivery_transition() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-lifecycle.example.com").unwrap();
        let session =
            RelaySessionKey::new(relay.clone(), AccessContext::Nip42(author.public_key()));
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);

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
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);

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
            let mut core = EngineCore::new(
                RedbStore::open(&path).unwrap(),
                Box::new(FixtureDirectory::new()),
                10,
            );
            publish_waiting(&mut core, &author_a, &relay, 20);
            publish_waiting(&mut core, &author_b, &relay, 21);
            assert_projection_matches_store(&core);
            assert_eq!(core.relay_worker_requirements().unwrap().writes, expected);
        }

        let mut recovered = EngineCore::new(
            RedbStore::open(&path).unwrap(),
            Box::new(FixtureDirectory::new()),
            10,
        );
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
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        let (receipt, _) = publish_waiting(&mut core, &author, &relay, 30);
        let key = LaneKey {
            intent_id: core.pending[&receipt].intent_id.unwrap(),
            relay: relay.clone(),
        };

        let result: Result<((), RecoveredLane), PersistenceError> =
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
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        let (receipt, _) = publish_waiting(&mut core, &author, &relay, 31);
        let key = LaneKey {
            intent_id: core.pending[&receipt].intent_id.unwrap(),
            relay,
        };
        let before = core.pending[&receipt].lane_projection.clone();

        let result: Result<((), RecoveredLane), PersistenceError> =
            core.commit_lane_transition(&key, |_store| {
                Err(PersistenceError::invariant(
                    "injected known-absent transition",
                ))
            });

        assert_eq!(result.unwrap_err().durability(), DurabilityOutcome::Absent);
        assert_eq!(core.pending[&receipt].lane_projection, before);
        assert!(core.lane_worker_projection_available());
    }

    /// The projection wrapper is an API mechanism, and this census is its
    /// falsifier: production reducer code may not call a lane-writing store
    /// door directly. Recursing over the directory means a newly added core
    /// module is covered automatically.
    #[test]
    fn every_core_lane_mutation_uses_the_projection_door() {
        const RAW_MUTATIONS: &[&str] = &[
            ".bootstrap_outbox_lanes(",
            ".set_lane_waiting(",
            ".set_lane_eligible(",
            ".set_lane_transient(",
            ".suspend_lane_attempt(",
            ".start_lane_attempt(",
            ".record_lane_handoff(",
            ".finish_lane_attempt(",
        ];

        let core_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/core");
        let projection_file = core_dir.join("lane_projection.rs");
        let mut stack = vec![core_dir];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory).expect("read core source directory") {
                let path = entry.expect("read core source entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs")
                    || path == projection_file
                {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read core source");
                for raw in RAW_MUTATIONS {
                    assert!(
                        !source.contains(raw),
                        "{} bypasses the lane projection door with `{raw}`",
                        path.display()
                    );
                }
            }
        }
    }
}
